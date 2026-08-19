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
//! Not a zip, no `archipelago.json`, a manifest that is not an object — every one of those returns
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

/// Rewrite `archipelago.json`'s `server` to `host:port`, leaving every other member's contents
/// exactly as they were.
///
/// `server` is written as `<host>:<port>` with no scheme, which is what the reference implementation
/// writes and what clients parse.
pub fn embed_server(patch: Vec<u8>, host: &str, port: u16) -> Result<Vec<u8>, PatchError> {
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
    manifest["server"] = serde_json::Value::String(format!("{host}:{port}"));
    let rewritten = serde_json::to_vec(&manifest)?;

    let mut out = Cursor::new(Vec::with_capacity(patch.len()));
    {
        let mut writer = zip::ZipWriter::new(&mut out);

        for index in 0..archive.len() {
            let mut member = archive.by_index(index)?;
            let name = member.name().to_string();

            // Members that are not files -- directory entries -- have no contents to copy, and
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

        let embedded = embed_server(original.clone(), "mw.ionium-dev.us", 41234).expect("embed");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&embedded, MANIFEST)).expect("json");
        assert_eq!(manifest["server"], "mw.ionium-dev.us:41234");
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
            "mw.ionium-dev.us",
            40000,
        )
        .expect("embed");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&embedded, MANIFEST)).expect("json");
        assert_eq!(manifest["server"], "mw.ionium-dev.us:40000");
        assert_eq!(manifest["game"], "Timespinner");
    }

    /// Puna serves what it stored. A file it does not recognize is still the player's file.
    #[test]
    fn anything_unrecognized_is_returned_untouched() {
        // Not a zip at all -- some games ship a bare binary patch.
        let raw = b"BSDIFF40\x00\x01\x02".to_vec();
        assert_eq!(
            embed_server(raw.clone(), "host", 1).expect("passthrough"),
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
            embed_server(no_manifest.clone(), "host", 1).expect("passthrough"),
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
            embed_server(odd.clone(), "host", 1).expect("passthrough"),
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
            embed_server(out.into_inner(), "host", 1),
            Err(PatchError::Json(_))
        ));
    }

    /// Embedding twice is embedding once: the room page hands out patches repeatedly, and a room
    /// that moved ports has to overwrite rather than accumulate.
    #[test]
    fn embedding_is_idempotent_and_overwrites_a_previous_address() {
        let original = patch(serde_json::json!({ "server": "old.example:1234" }), b"data");

        let once = embed_server(original, "mw.ionium-dev.us", 41234).expect("first");
        let twice = embed_server(once.clone(), "mw.ionium-dev.us", 41234).expect("second");
        let moved = embed_server(twice, "mw.ionium-dev.us", 40002).expect("third");

        let manifest: serde_json::Value =
            serde_json::from_slice(&member(&moved, MANIFEST)).expect("json");
        assert_eq!(manifest["server"], "mw.ionium-dev.us:40002");

        let first: serde_json::Value =
            serde_json::from_slice(&member(&once, MANIFEST)).expect("json");
        assert_eq!(first["server"], "mw.ionium-dev.us:41234");
    }

    /// Directory entries are members too, and reading one as a file yields nothing -- so a patch
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

        let embedded = embed_server(out.into_inner(), "host", 40000).expect("embed");
        let archive = zip::ZipArchive::new(Cursor::new(&embedded)).expect("a zip");
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();

        assert!(names.iter().any(|n| n == "assets/"), "{names:?}");
        assert_eq!(member(&embedded, "assets/thing.bin"), b"payload");
    }
}
