//! End-to-end ingest against a real generation zip.
//!
//! Gated on `PUNA_TEST_GENERATION_ZIP` rather than shipping a fixture, because a real zip is tens
//! of megabytes and carries real players' names. Point it at one to run:
//!
//! ```text
//! PUNA_TEST_GENERATION_ZIP=~/games/Archipelago/output/AP_14318265276849580066.zip cargo test
//! ```
//!
//! The unit tests in `artifact::ingest` cover the filename cases exhaustively; this covers the
//! parts only a real archive can exercise -- multidata parsing, manifest reads, and the slot/patch
//! join across every naming convention at once.

use pahoa_multidata::{MultiData, SlotType};
use puna_core::artifact;

const LIMIT: u64 = 512 * 1024 * 1024;

fn fixture() -> Option<Vec<u8>> {
    let path = std::env::var("PUNA_TEST_GENERATION_ZIP").ok()?;
    let path = shellexpand_tilde(&path);
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) => panic!("PUNA_TEST_GENERATION_ZIP={path} could not be read: {e}"),
    }
}

/// The seed out of a generation zip, parsed.
fn multidata(bytes: &[u8]) -> MultiData {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
    let name = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .find(|n| n.ends_with(".archipelago"))
        .expect("multidata");
    let mut raw = Vec::new();
    archive
        .by_name(&name)
        .unwrap()
        .read_to_end(&mut raw)
        .unwrap();
    MultiData::parse(&raw).expect("multidata parses")
}

fn shellexpand_tilde(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => p.to_string(),
        },
        None => p.to_string(),
    }
}

#[test]
fn a_real_generation_zip_is_fully_indexed() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };

    let meta = artifact::inspect(&bytes, LIMIT).expect("a real generation zip must parse");

    assert!(!meta.seed_name.is_empty(), "seed_name must be populated");
    assert!(meta.slot_count > 0, "slot_count must be positive");
    assert!(meta.locations > 0, "locations must be positive");
    assert!(!meta.games.is_empty(), "games must be populated");
    assert!(!meta.slots.is_empty(), "player slots must be populated");
    assert!(
        meta.multidata_member.ends_with(".archipelago"),
        "multidata member: {}",
        meta.multidata_member
    );

    // Every patch found must belong to exactly one slot: a patch attributed twice would mean two
    // players downloading the same file, and one of them getting a world they are not playing.
    let mut attributed: Vec<&str> = meta
        .slots
        .iter()
        .filter_map(|s| s.patch_member.as_deref())
        .collect();
    let before = attributed.len();
    attributed.sort_unstable();
    attributed.dedup();
    assert_eq!(
        before,
        attributed.len(),
        "a patch was attributed to two slots"
    );

    // Nothing should be left over. An unmatched patch is a player who cannot join, so this is the
    // assertion that would catch a new naming convention appearing upstream.
    assert!(
        meta.unmatched_patches.is_empty(),
        "unattributed patches: {:?}",
        meta.unmatched_patches
    );

    eprintln!(
        "seed={} slot_count={} players={} games={} locations={} race_mode={} spoiler={:?}",
        meta.seed_name,
        meta.slot_count,
        meta.slots.len(),
        meta.games.len(),
        meta.locations,
        meta.race_mode,
        meta.spoiler_member.is_some(),
    );
    let with_patch = meta
        .slots
        .iter()
        .filter(|s| s.patch_member.is_some())
        .count();
    eprintln!(
        "players with a patch: {}/{} (games that emit no patch are normal)",
        with_patch,
        meta.slots.len()
    );
}

/// Puna keeps exactly the connectable slots: players and spectators, never groups.
///
/// Matches the reference at both ends: `WebHostLib/upload.py` skips only `SlotType.group`, and
/// `MultiServer.py:1880` resolves a Connect through `connect_names` with no slot-type filter, so a
/// spectator logs in like anyone else.
///
/// Also prints the distribution, since which types a seed contains is worth seeing when pointing
/// this at a new fixture.
#[test]
fn connectable_slots_are_kept_and_groups_are_dropped() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };

    let data = multidata(&bytes);

    let mut players = 0;
    let mut spectators = 0;
    let mut groups = 0;
    for (slot, info) in &data.slot_info {
        match info.slot_type {
            SlotType::Player => players += 1,
            SlotType::Spectator => {
                spectators += 1;
                eprintln!(
                    "  SPECTATOR slot {slot}: name={:?} game={:?}",
                    info.name, info.game
                );
            }
            SlotType::Group => {
                groups += 1;
                eprintln!(
                    "  GROUP     slot {slot}: name={:?} game={:?}",
                    info.name, info.game
                );
            }
        }
    }
    eprintln!("slot types: players={players} spectators={spectators} groups={groups}");

    let meta = artifact::inspect(&bytes, LIMIT).expect("inspect");
    assert_eq!(
        meta.slots.len(),
        players + spectators,
        "puna's slot list should hold every connectable slot"
    );
    assert_eq!(
        meta.slots
            .iter()
            .filter(|s| s.kind == artifact::SlotKind::Spectator)
            .count(),
        spectators,
        "every spectator should be kept, and marked as one"
    );

    // Each kept slot must agree with the multidata on type -- a spectator recorded as a player
    // would be handed a claim link promising a patch and a game that do not exist.
    for slot in &meta.slots {
        let info = data
            .slot_info
            .get(&(slot.slot_number as u32))
            .expect("kept slot must exist in the multidata");
        let expected = match info.slot_type {
            SlotType::Player => artifact::SlotKind::Player,
            SlotType::Spectator => artifact::SlotKind::Spectator,
            SlotType::Group => panic!("slot {} is a group and must be dropped", slot.slot_number),
        };
        assert_eq!(slot.kind, expected, "slot {}", slot.slot_number);
        if slot.kind == artifact::SlotKind::Spectator {
            assert!(
                slot.patch_member.is_none(),
                "a spectator plays nothing and must never hold a patch"
            );
        }
    }

    assert_eq!(
        meta.slot_count as usize,
        data.slot_info.len(),
        "slot_count should be every slot, matching how pahoa sizes its outbound budget"
    );
}

/// **A check that is too strict does not fail a unit test -- it refuses somebody's multiworld**,
/// so the direction that matters most is this one: real generation output must pass.
///
/// It is not idle. Switching these checks on in pahoa refused every seed with a spectator in it,
/// because the locations rule counted slots that *own* something where the reference counts slots
/// that are *declared*, and a spectator declares one and owns nothing. That was a fix to a check
/// nobody had ever run; the same class of mistake arrives here as a `pahoa-multidata` bump.
#[test]
fn a_real_seed_would_load() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    assert_eq!(
        artifact::load_refusal(&multidata(&bytes)),
        None,
        "real generation output must not be refused"
    );
}

/// The three refusals Puna can reach, each mutated into a seed that is otherwise real.
///
/// Mutating a parsed `MultiData` rather than hand-building one is what makes these discriminating:
/// everything except the one broken fact is a working seed, so a refusal can only be the mutation.
/// It is also the only route available -- there is no pickle *encoder*, so a bad seed cannot be
/// written back into a zip and pushed through `inspect`. The call site is pinned by a source lint
/// in `artifact::ingest` instead.
#[test]
fn a_seed_a_room_would_refuse_is_refused_here() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    let seed = multidata(&bytes);

    let refusal = |data: &MultiData, what: &str| -> String {
        artifact::load_refusal(data).unwrap_or_else(|| panic!("{what} must be refused"))
    };

    // A name somebody could authenticate as, with no world behind it. This is the one with a
    // security shape rather than merely a crash: the connect resolves and the slot does not exist.
    let mut orphan = seed.clone();
    orphan.connect_names.insert("Nobody".to_string(), (0, 9999));
    assert!(
        refusal(&orphan, "a connect name pointing at no slot").contains("9999"),
        "the refusal should name the slot"
    );

    // Team 1. Nothing generates one -- `Main.py:337` writes team 0 unconditionally -- and neither
    // server can serve it: the reference accepts the seed and raises inside `ctx.clients[team]` on
    // the connect that used the name, with the room already up.
    let mut other_team = seed.clone();
    let (name, (_, slot)) = other_team
        .connect_names
        .iter()
        .next()
        .map(|(n, v)| (n.clone(), *v))
        .expect("a real seed has connect names");
    other_team.connect_names.insert(name, (1, slot));
    assert!(
        refusal(&other_team, "a slot on team 1").contains("team 1"),
        "the refusal should name the team"
    );

    // A group listing a member that does not exist. The server builds its item links from this.
    let mut bad_group = seed.clone();
    let first = *bad_group
        .slot_info
        .keys()
        .next()
        .expect("a real seed has slots");
    bad_group
        .slot_info
        .get_mut(&first)
        .unwrap()
        .group_members
        .push(9999);
    assert!(
        refusal(&bad_group, "a group member that does not exist").contains("9999"),
        "the refusal should name the member"
    );

    // And the seed those three were cut from is still fine, so the mutations are what is failing.
    assert_eq!(artifact::load_refusal(&seed), None);
}
