//! Embedding a room's address into a slot's patch.
//!
//! A patch is a zip whose `archipelago.json` carries a `server` field. A client that opens one
//! connects to whatever is written there, so filling it in is the difference between "download this
//! and type the address in" and "download this and play".
//!
//! Ported from Archipelago-lobby's `embed_server_info_in_patch`, with one behavioral difference that
//! is Puna's whole advantage here: **the lobby can only embed an address for a room that is up,
//! while Puna's port reservations are sticky**, so a patch downloaded from a room that is torn down
//! already carries the address it will come back on. The one case that invalidates it is an LRU
//! reclaim under range pressure, which the room page is authoritative about.
//!
//! ## Anything unexpected is served unchanged, deliberately
//!
//! Not a zip, no `archipelago.json`, a manifest that is not an object: every one of those returns
//! the bytes as stored rather than an error. **Puna serves what it was given**: these are a game's
//! own files, the set of patch formats is open, and refusing to hand over a file because Puna did not
//! recognize its shape would break a game Puna has never heard of. The address is a convenience; the
//! patch is the thing the player needs.

use std::io::{Cursor, Read, Write};

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("reading the patch: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("writing the patch: {0}")]
    Io(#[from] std::io::Error),
    #[error("the patch's archipelago.json is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// The member every Archipelago patch carries, and the only one this touches.
const MANIFEST: &str = "archipelago.json";

/// The credential a `claimed` patch carries, if the room has one for this slot.
///
/// The slot name is always needed (it is the *username* half of the URL) so this is `None` only
/// when the room has no password at all, in which case the patch keeps the bare address.
#[derive(Debug, Clone)]
pub struct Credential<'a> {
    pub slot_name: &'a str,
    pub password: &'a str,
}

/// Percent-encode one userinfo component.
///
/// **Archipelago `unquote`s both halves** (`CommonClient.py`'s `server_loop`), so this is the other
/// side of a round trip rather than decoration. It matters because a slot name is arbitrary text
/// out of an uploaded seed: an `@` in one would end the userinfo early and point the client at a
/// different host, a `:` would split the password in the wrong place, and a space would produce a
/// netloc `urlparse` cannot read. Encoded, every one of them survives to be decoded back.
///
/// The set kept unescaped is RFC 3986's `unreserved`. Anything else is escaped, including the
/// sub-delims a URL would technically allow in userinfo: there is nothing to gain from leaving
/// them raw, and each one is a character somebody's parser might treat specially.
fn encode_userinfo(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Rewrite `archipelago.json`'s `server`, leaving every other member's contents exactly as they
/// were.
///
/// Without a credential the value is `<host>:<port>`, no scheme: what the reference implementation
/// writes and what every client parses.
///
/// With one it is `wss://<slot>:<password>@<host>:<port>`, which Archipelago's own client reads:
/// `server_loop` hands the address to `urlparse` and takes `username` and `password` off it, and a
/// patch's `server` reaches that same parser through `args.connect`. The `wss://` is load-bearing
/// and survives: only `archipelago://` is rewritten to `ws://`, and the TLS context is chosen by
/// the `wss` prefix, which is what a Puna room needs since it terminates its own TLS.
pub fn embed_server(
    patch: Vec<u8>,
    host: &str,
    port: u16,
    credential: Option<Credential<'_>>,
) -> Result<Vec<u8>, PatchError> {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(&patch)) else {
        return Ok(patch);
    };
    if archive.by_name(MANIFEST).is_err() {
        return Ok(patch);
    }

    let mut manifest_json = String::new();
    archive
        .by_name(MANIFEST)?
        .read_to_string(&mut manifest_json)?;
    let mut manifest: serde_json::Value = serde_json::from_str(&manifest_json)?;
    if !manifest.is_object() {
        // A manifest that is not an object is not one this code understands, and inventing a shape
        // for it would produce a patch that opens and then fails somewhere less obvious.
        return Ok(patch);
    }
    manifest["server"] = serde_json::Value::String(match credential {
        Some(c) => format!(
            "wss://{}:{}@{host}:{port}",
            encode_userinfo(c.slot_name),
            encode_userinfo(c.password)
        ),
        None => format!("{host}:{port}"),
    });
    let rewritten = serde_json::to_vec(&manifest)?;

    let mut out = Cursor::new(Vec::with_capacity(patch.len()));
    {
        let mut writer = zip::ZipWriter::new(&mut out);

        for index in 0..archive.len() {
            let mut member = archive.by_index(index)?;
            let name = member.name().to_string();

            // Members that are not files (directory entries) have no contents to copy, and
            // `read_to_end` on one yields nothing rather than failing, so they are handled first.
            if member.is_dir() {
                writer.add_directory(&name, options(&member))?;
                continue;
            }

            let mut contents = Vec::new();
            member.read_to_end(&mut contents)?;
            let options = options(&member);
            drop(member);

            writer.start_file(&name, options)?;
            if name == MANIFEST {
                writer.write_all(&rewritten)?;
            } else {
                writer.write_all(&contents)?;
            }
        }

        writer.finish()?;
    }

    Ok(out.into_inner())
}

/// Keep each member's original compression rather than forcing one.
///
/// A patch's large member is usually already-compressed game data that a game's own tooling chose to
/// store uncompressed; re-deflating it would cost time and produce a bigger file than it started as.
/// Anything else keeps Deflated, which is what every writer in this ecosystem produces.
fn options(member: &zip::read::ZipFile<'_, Cursor<&Vec<u8>>>) -> zip::write::SimpleFileOptions {
    let method = match member.compression() {
        zip::CompressionMethod::Stored => zip::CompressionMethod::Stored,
        _ => zip::CompressionMethod::Deflated,
    };
    zip::write::SimpleFileOptions::default().compression_method(method)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal patch: the manifest plus one member of "game data".
    fn patch(manifest: serde_json::Value, payload: &[u8]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut out);
            let options = zip::write::SimpleFileOptions::default();

            writer.start_file(MANIFEST, options).expect("manifest");
            writer
                .write_all(serde_json::to_string(&manifest).unwrap().as_bytes())
                .expect("manifest body");

            writer
                .start_file(
                    "data.bsdiff4",
                    options.compression_method(zip::CompressionMethod::Stored),
                )
                .expect("payload");
            writer.write_all(payload).expect("payload body");

            writer.finish().expect("finish");
        }
        out.into_inner()
    }

    fn member(zip: &[u8], name: &str) -> Vec<u8> {
        let mut archive = zip::ZipArchive::new(Cursor::new(zip)).expect("a zip");
        let mut file = archive.by_name(name).expect("the member");
        let mut out = Vec::new();
        file.read_to_end(&mut out).expect("read");
        out
    }

    #[test]
    fn the_server_is_written_and_nothing_else_changes() {
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let original = patch(
            serde_json::json!({
                "game": "A Link to the Past",
                "player": "Troy",
                "server": "",
                "compatible_version": 6,
            }),
            &payload,
        );

        let embedded =
            embed_server(original.clone(), "rooms.example.com", 41234, None).expect("embed");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&embedded, MANIFEST)).expect("json");
        assert_eq!(manifest["server"], "rooms.example.com:41234");
        // No scheme and no path: this is what the reference writes and what clients parse.
        assert!(!manifest["server"].as_str().unwrap().contains("://"));

        // Every other key survives, including ones this code has never heard of.
        assert_eq!(manifest["game"], "A Link to the Past");
        assert_eq!(manifest["player"], "Troy");
        assert_eq!(manifest["compatible_version"], 6);

        // The game data is the whole point of the file and must come back bit for bit.
        assert_eq!(member(&embedded, "data.bsdiff4"), payload);
    }

    #[test]
    fn an_absent_server_field_is_added() {
        let embedded = embed_server(
            patch(serde_json::json!({ "game": "Timespinner" }), b"x"),
            "rooms.example.com",
            40000,
            None,
        )
        .expect("embed");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&embedded, MANIFEST)).expect("json");
        assert_eq!(manifest["server"], "rooms.example.com:40000");
        assert_eq!(manifest["game"], "Timespinner");
    }

    /// Puna serves what it stored. A file it does not recognize is still the player's file.
    #[test]
    fn anything_unrecognized_is_returned_untouched() {
        // Not a zip at all: some games ship a bare binary patch.
        let raw = b"BSDIFF40\x00\x01\x02".to_vec();
        assert_eq!(
            embed_server(raw.clone(), "host", 1, None).expect("passthrough"),
            raw
        );

        // A zip with no manifest.
        let mut out = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut out);
            writer
                .start_file("rom.bin", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"data").unwrap();
            writer.finish().unwrap();
        }
        let no_manifest = out.into_inner();
        assert_eq!(
            embed_server(no_manifest.clone(), "host", 1, None).expect("passthrough"),
            no_manifest
        );

        // A manifest that is valid JSON but not an object.
        let mut out = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut out);
            writer
                .start_file(MANIFEST, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"[1, 2, 3]").unwrap();
            writer.finish().unwrap();
        }
        let odd = out.into_inner();
        assert_eq!(
            embed_server(odd.clone(), "host", 1, None).expect("passthrough"),
            odd
        );
    }

    /// A manifest that is not JSON at all is an error rather than a passthrough: the member is named
    /// `archipelago.json`, so something has gone wrong with the file rather than with Puna's guess
    /// about it, and serving it silently would hide that.
    #[test]
    fn a_corrupt_manifest_is_an_error() {
        let mut out = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut out);
            writer
                .start_file(MANIFEST, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"{not json").unwrap();
            writer.finish().unwrap();
        }
        assert!(matches!(
            embed_server(out.into_inner(), "host", 1, None),
            Err(PatchError::Json(_))
        ));
    }

    /// Embedding twice is embedding once: the room page hands out patches repeatedly, and a room
    /// that moved ports has to overwrite rather than accumulate.
    #[test]
    fn embedding_is_idempotent_and_overwrites_a_previous_address() {
        let original = patch(serde_json::json!({ "server": "old.example:1234" }), b"data");

        let once = embed_server(original, "rooms.example.com", 41234, None).expect("first");
        let twice = embed_server(once.clone(), "rooms.example.com", 41234, None).expect("second");
        let moved = embed_server(twice, "rooms.example.com", 40002, None).expect("third");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&moved, MANIFEST)).expect("json");
        assert_eq!(manifest["server"], "rooms.example.com:40002");

        let first: serde_json::Value =
            serde_json::from_slice(&member(&once, MANIFEST)).expect("json");
        assert_eq!(first["server"], "rooms.example.com:41234");
    }

    /// Directory entries are members too, and reading one as a file yields nothing, so a patch
    /// with a folder in it must not come out with the folder flattened away.
    #[test]
    fn directory_entries_survive() {
        let mut out = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut out);
            let options = zip::write::SimpleFileOptions::default();
            writer.add_directory("assets/", options).unwrap();
            writer.start_file(MANIFEST, options).unwrap();
            writer.write_all(br#"{"game":"x"}"#).unwrap();
            writer.start_file("assets/thing.bin", options).unwrap();
            writer.write_all(b"payload").unwrap();
            writer.finish().unwrap();
        }

        let embedded = embed_server(out.into_inner(), "host", 40000, None).expect("embed");
        let archive = zip::ZipArchive::new(Cursor::new(&embedded)).expect("a zip");
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();

        assert!(names.iter().any(|n| n == "assets/"), "{names:?}");
        assert_eq!(member(&embedded, "assets/thing.bin"), b"payload");
    }

    /// **A claimed patch carries a URL Archipelago's own client can read.**
    ///
    /// The shape is `wss://<slot>:<password>@<host>:<port>`, and it is not a guess: `server_loop`
    /// in `CommonClient.py` hands the address to `urlparse` and takes `username` and `password` off
    /// it, and a patch's `server` reaches that parser through `args.connect`. `wss://` survives
    /// (only `archipelago://` is rewritten) and the TLS context is chosen by that prefix, which a
    /// Puna room needs because it terminates its own TLS.
    #[test]
    fn a_claimed_patch_embeds_a_url_the_client_can_connect_with() {
        let embedded = embed_server(
            patch(serde_json::json!({ "server": "" }), b"x"),
            "mw.example",
            41234,
            Some(Credential {
                slot_name: "MooingYacht1",
                password: "abcde-fghij",
            }),
        )
        .expect("embed");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&embedded, MANIFEST)).expect("json");
        assert_eq!(
            manifest["server"],
            "wss://MooingYacht1:abcde-fghij@mw.example:41234"
        );
    }

    /// **Every character that would silently change the address is escaped.**
    ///
    /// Archipelago `unquote`s both halves, so this is one side of a round trip rather than
    /// decoration, and it is load-bearing because a slot name is arbitrary text out of an uploaded
    /// seed. Raw, an `@` ends the userinfo early and points the client at a different host
    /// entirely; a `:` splits the password in the wrong place; a space produces a netloc `urlparse`
    /// cannot read. None of those fail loudly: they connect somewhere else, or refuse with a
    /// message about the address rather than about the name.
    #[test]
    fn a_slot_name_cannot_rewrite_the_address_it_is_embedded_in() {
        let embedded = embed_server(
            patch(serde_json::json!({ "server": "" }), b"x"),
            "mw.example",
            41234,
            Some(Credential {
                // An `@` and a host, which is the hostile case: unescaped this reads as
                // `evil.test:1/` being the server.
                slot_name: "a@evil.test:1/ b",
                password: "p:s@w/d",
            }),
        )
        .expect("embed");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&embedded, MANIFEST)).expect("json");
        let server = manifest["server"].as_str().expect("a string");

        assert_eq!(
            server, "wss://a%40evil.test%3A1%2F%20b:p%3As%40w%2Fd@mw.example:41234",
            "a name or password reached the URL unescaped"
        );
        // The property behind the literal above, stated so a future change to the escaping set
        // still has to keep it: exactly one `@`, and the host follows it.
        assert_eq!(server.matches('@').count(), 1);
        assert!(server.ends_with("@mw.example:41234"));
    }

    /// No credential is no change: the bare address, exactly as before.
    #[test]
    fn an_open_patch_carries_the_address_and_nothing_else() {
        let embedded = embed_server(
            patch(serde_json::json!({ "server": "" }), b"x"),
            "mw.example",
            41234,
            None,
        )
        .expect("embed");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&embedded, MANIFEST)).expect("json");
        assert_eq!(manifest["server"], "mw.example:41234");
        assert!(
            !manifest["server"].as_str().expect("string").contains('@'),
            "a patch with no credential carries userinfo"
        );
    }
}
