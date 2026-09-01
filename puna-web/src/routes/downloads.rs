//! Serving a room's artifacts to the people entitled to them: patches, and the spoiler.
//!
//! Both read from `generations/<sha256>/`, which is the only part of the volume the web tier mounts.
//! Neither can reach a room's state directory, and that is enforced by the `subPath` on the mount
//! rather than by anything here.
//!
//! ## The patch carries the room's address, and can even while the room is down
//!
//! `embed_server` rewrites `archipelago.json`'s `server` on the way out, so a player downloads a
//! file that already knows where to connect. **Puna can do this for a torn-down room and the lobby
//! cannot**, because the port reservation outlives the Service: an async that has idled out still
//! owns its pair, so the address embedded now is the address it will come back on. The one thing
//! that invalidates it is an LRU reclaim under range pressure, which is why the room page stays
//! authoritative and a reclaim is recorded against the victim.
//!
//! ## Two different authorization models, on purpose
//!
//! A patch is **per slot**, and how narrowly depends on the room: `PatchAccess` applies
//! `patch_policy`, so a `claimed` room admits its owner, the room's staff and global admins while
//! an `open` one serves anybody holding the room's URL, as archipelago.gg does. Under `claimed`,
//! holding one slot in a room grants nothing about another. A spoiler is **per
//! room**, and its audience is a policy the organizer chose, defaulted from the seed's `race_mode`
//! because a leaked spoiler in a race cannot be taken back.

use puna_core::artifact::{Credential, GenerationPaths, embed_server, storage};
use puna_core::model::member;
use puna_core::model::room::{self, SpoilerPolicy};
use puna_core::model::{generation, port, slot};
use rocket::http::Header;
use rocket::{Responder, State, get, routes};

use crate::auth::Session;
use crate::error::{Result, forbidden, not_found};
use crate::guards::PatchAccess;
use crate::params::RoomParam;
use crate::{AdvertiseHost, DataDir};

type Pool = puna_core::db::Pool;

/// A file to save, rather than to render.
#[derive(Responder)]
#[response(content_type = "application/octet-stream")]
struct Download {
    body: Vec<u8>,
    disposition: Header<'static>,
}

/// The spoiler, as a page to read.
#[derive(Responder)]
#[response(content_type = "text/plain; charset=utf-8")]
struct SpoilerText(String);

/// A slot's patch, with the room's address embedded.
#[get("/room/<_id>/slot/<n>/patch")]
async fn slot_patch(
    _id: RoomParam,
    n: i32,
    access: PatchAccess,
    pool: &State<Pool>,
    data_dir: &State<DataDir>,
    advertise_host: &State<AdvertiseHost>,
) -> Result<Download> {
    let mut conn = pool.get().await?;

    // The room's slots are a copy taken at creation; the *patch* belongs to the generation, which
    // is shared between every room built from it. So the file is found through the generation and
    // the authorization is done against the room -- two different questions about one slot number.
    let generation = generation::get(&mut conn, access.room.generation_id)
        .await?
        .ok_or_else(|| not_found("this room's generation is no longer indexed"))?;

    let entry = generation::slots(&mut conn, generation.id)
        .await?
        .into_iter()
        .find(|entry| entry.slot_number == n)
        .ok_or_else(|| not_found("no such slot"))?;

    // Patchless games are ordinary -- most of them are -- and spectators never have one. Saying so
    // beats a 404 that reads as "your download is broken".
    let member = entry.patch_member.as_deref().ok_or_else(|| {
        not_found(
            "this slot has no patch file. Most games are played with a client rather than a \
             patched ROM; check the room page for what your game needs.",
        )
    })?;

    let sha256: [u8; 32] = generation
        .sha256
        .as_slice()
        .try_into()
        .map_err(|_| not_found("this room's generation is not addressable"))?;
    let paths = GenerationPaths::new(&data_dir.0, &sha256);
    // The same sanitizing the writer used, from the same function: two copies of that rule would
    // diverge on exactly the inputs it exists for.
    let path = paths.patch(entry.slot_number, &storage::patch_extension(member));

    let stored = tokio::fs::read(&path).await.map_err(|e| {
        tracing::error!(
            room = %access.room.id,
            slot = n,
            path = %path.display(),
            error = %e,
            "a slot's patch is indexed but not on disk"
        );
        not_found("this slot's patch is missing from storage")
    })?;

    // Sticky reservations are what make this work while the room is down. No reservation at all is
    // a room that has never started, and the honest answer is the file without an address rather
    // than a refusal: the player can still type one in.
    let body = match port::reserved_pair(&mut conn, access.room.id).await? {
        // A failure here is a 500 by way of `From`, and it should be: the file is in Puna's own
        // storage, so a patch that cannot be rewritten is a broken artifact rather than a bad
        // request, and the log gets the chain while the caller gets a status.
        Some(base_port) => {
            // **The credential is read from the ROOM's slot, not the generation's.** The entry
            // above found the file, which belongs to the shared generation; the password belongs to
            // this room's copy of the slot, and two rooms on one seed have different ones.
            //
            // `room` mode uses the room-wide password with the slot's own name as the username --
            // pahoa authenticates the password and the name identifies the slot, so the pair is
            // what a client needs either way.
            let credential = match access.room.patch_policy {
                room::PatchPolicy::Open => None,
                room::PatchPolicy::Claimed => match access.room.slot_auth {
                    room::SlotAuth::None => None,
                    room::SlotAuth::Room => access.room.password.as_deref(),
                    room::SlotAuth::PerSlot => access.slot.password.as_deref(),
                },
            };
            let credential = credential.map(|password| Credential {
                slot_name: &access.slot.player_name,
                password,
            });
            // **The port the room leads with, not the base port.** A patch is what a GAME client
            // launches with, and game clients are precisely who the filtered listener exists for --
            // so a 500-slot room whose organizer chose `Filtered` because the full feed drowns
            // clients was, until this line, handing every player who downloaded a patch the address
            // that drowns them.
            //
            // `base_port + 1` is the pair's filtered half by construction: reservations are
            // allocated as an adjacent even/odd pair and `spec::args` passes exactly that. Read from
            // the reservation rather than from `advertised_filtered_port` for the reason this whole
            // route exists -- the reservation outlives the Service, so a torn-down room still
            // embeds the address it will come back on.
            //
            // Already-downloaded patches keep whatever they were built with, which is what the
            // option's hint means by "at download time".
            let port = if access.room.leads_with_filtered() {
                base_port + 1
            } else {
                base_port
            };
            embed_server(stored, &advertise_host.0, port, credential)?
        }
        None => {
            tracing::info!(
                room = %access.room.id,
                slot = n,
                "serving a patch with no address: this room has never been allocated a port"
            );
            stored
        }
    };

    Ok(Download {
        body,
        disposition: attachment(&filename(&generation.seed_name, &entry, member)),
    })
}

/// The room's spoiler, for whoever `spoiler_policy` admits.
#[get("/room/<id>/spoiler")]
async fn spoiler(
    id: RoomParam,
    session: Session,
    pool: &State<Pool>,
    data_dir: &State<DataDir>,
) -> Result<SpoilerText> {
    let mut conn = pool.get().await?;
    let room = puna_core::model::room::get(&mut conn, id.0)
        .await?
        .ok_or_else(|| not_found("no such room"))?;

    // `never` is a 404 rather than a 403: with no spoiler to be had, "you may not see this" would
    // tell a visitor something about the room that a race deliberately does not.
    if room.spoiler_policy == SpoilerPolicy::Never {
        return Err(not_found("this room's spoiler is not available"));
    }

    let is_staff = if session.is_admin {
        true
    } else if let Some(user_id) = session.user_id {
        member::role_of(&mut conn, room.id, user_id)
            .await?
            .is_some()
    } else {
        false
    };

    let owns_a_slot = match session.user_id {
        Some(user_id) => slot::list(&mut conn, room.id)
            .await?
            .iter()
            .any(|slot| slot.owner_id == Some(user_id)),
        None => false,
    };

    // The same function the room page asks, so the link and the download cannot disagree.
    if !puna_core::model::room::may_see_spoiler(room.spoiler_policy, is_staff, owns_a_slot) {
        // Unauthenticated callers get 401 -> login rather than a flat refusal, since logging in may
        // well be what makes them eligible. The catcher turns the status into the round trip.
        return Err(if session.user_id.is_none() {
            crate::error::unauthorized("log in to see this room's spoiler")
        } else {
            forbidden("this room's spoiler is not available to you")
        });
    }

    let generation = generation::get(&mut conn, room.generation_id)
        .await?
        .ok_or_else(|| not_found("this room's generation is no longer indexed"))?;
    if !generation.has_spoiler {
        return Err(not_found("this seed was generated without a spoiler"));
    }

    let sha256: [u8; 32] = generation
        .sha256
        .as_slice()
        .try_into()
        .map_err(|_| not_found("this room's generation is not addressable"))?;
    let path = GenerationPaths::new(&data_dir.0, &sha256).spoiler();

    let text = tokio::fs::read_to_string(&path).await.map_err(|e| {
        tracing::error!(
            room = %room.id,
            path = %path.display(),
            error = %e,
            "a spoiler is indexed but not on disk"
        );
        not_found("this room's spoiler is missing from storage")
    })?;

    Ok(SpoilerText(text))
}

/// `Content-Disposition`, with the filename quoted.
fn attachment(filename: &str) -> Header<'static> {
    Header::new(
        "Content-Disposition",
        format!("attachment; filename=\"{filename}\""),
    )
}

/// A download name built from the seed, the slot and the player.
///
/// **Every part of this is untrusted text** (a player name and a member name both come out of a
/// zip somebody uploaded), and it is going into a response header, where a newline would be a
/// response-splitting bug and a quote would end the filename early. So it is not sanitized so much
/// as reconstructed: an allowlist of characters, a length cap, and the extension taken from the
/// same function that named the file on disk.
fn filename(seed_name: &str, entry: &generation::Slot, member: &str) -> String {
    let extension = storage::patch_extension(member);
    let stem = format!("{seed_name}_P{}_{}", entry.slot_number, entry.player_name);
    format!("{}.{extension}", sanitize(&stem, 96))
}

/// Alphanumerics, `-` and `_`. **No `.`**, deliberately.
///
/// The extension is appended separately, so the stem never needs one, and excluding it makes `..`
/// unspellable rather than merely harmless-in-practice, and stops a name that begins with a dot from
/// arriving as a hidden file. Anything else becomes `_` rather than being dropped, so two players
/// whose names differ only in punctuation still get different files.
fn sanitize(raw: &str, max: usize) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(max)
        .collect();

    // Unreachable through `filename`, which always contributes `_P<n>_`, but a stem is not allowed
    // to be nothing whatever it is built from.
    if cleaned.chars().all(|c| c == '_') {
        "patch".to_string()
    } else {
        cleaned
    }
}

pub fn routes() -> Vec<rocket::Route> {
    routes![slot_patch, spoiler]
}

#[cfg(test)]
mod tests {
    use super::*;
    use puna_core::artifact::SlotKind;

    fn entry(slot_number: i32, player_name: &str) -> generation::Slot {
        generation::Slot {
            slot_number,
            player_name: player_name.to_string(),
            game: "A Link to the Past".into(),
            kind: SlotKind::Player,
            patch_member: None,
            patch_size_bytes: None,
        }
    }

    #[test]
    fn an_ordinary_name_survives_intact() {
        assert_eq!(
            filename(
                "14318265276849580066",
                &entry(3, "Troy"),
                "AP_1_P3_Troy.apz3"
            ),
            "14318265276849580066_P3_Troy.apz3"
        );
    }

    /// The header is the reason this is an allowlist rather than a blocklist: a newline in a player
    /// name would end the header and start another one.
    #[test]
    fn nothing_a_player_can_name_themselves_escapes_the_header() {
        let hostile = entry(1, "a\"\r\nX-Evil: yes");
        let name = filename("seed", &hostile, "x.apz3");

        assert!(!name.contains('"'), "{name}");
        assert!(!name.contains('\r') && !name.contains('\n'), "{name}");
        assert!(!name.contains(' '), "{name}");
        assert!(!name.contains(':'), "{name}");

        // And the whole header, assembled, is still one line.
        let header = attachment(&name);
        assert!(!header.value().contains('\n'), "{}", header.value());
    }

    /// The stem carries no dots at all, so the only `.` in the result is the extension's.
    #[test]
    fn a_traversal_cannot_be_spelled() {
        let name = filename("../../etc", &entry(1, "../passwd"), "x.apz3");
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains(".."), "{name}");
        assert_eq!(name.matches('.').count(), 1, "{name}");
        assert!(!name.starts_with('.'), "{name} would be a hidden file");
    }

    /// A stem that sanitizes away entirely still has to be a filename.
    ///
    /// Tested against `sanitize` rather than `filename`, because `filename` always contributes
    /// `_P<n>_` and so can never produce one: asserting it through the caller would be a test that
    /// passes for the wrong reason.
    #[test]
    fn a_stem_with_nothing_usable_in_it_falls_back() {
        assert_eq!(sanitize("。。。", 96), "patch");
        assert_eq!(sanitize("", 96), "patch");
        // ...and a name that keeps one usable character keeps it.
        assert_eq!(sanitize("。a。", 96), "_a_");
    }

    #[test]
    fn names_are_capped_rather_than_unbounded() {
        let long = "x".repeat(500);
        let name = filename(&long, &entry(1, &long), "x.apz3");
        assert!(name.len() <= 96 + ".apz3".len(), "{}", name.len());
    }

    /// The extension is what a client dispatches on, so it comes from the same function that named
    /// the file on disk rather than from a second guess at the same rule.
    #[test]
    fn the_extension_comes_from_the_member() {
        assert!(filename("s", &entry(1, "p"), "AP_1_P1_p.APBP").ends_with(".apbp"));
        assert!(filename("s", &entry(1, "p"), "no-extension").ends_with(".bin"));
    }
}
