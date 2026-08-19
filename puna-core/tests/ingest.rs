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

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).expect("zip");
    let name = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .find(|n| n.ends_with(".archipelago"))
        .expect("multidata");
    let raw = {
        use std::io::Read;
        let mut f = archive.by_name(&name).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        buf
    };
    let data = MultiData::parse(&raw).expect("multidata parses");

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
