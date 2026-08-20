//! The tracker's name tables, against a real generation zip.
//!
//! Gated on `PUNA_TEST_GENERATION_ZIP` alone, like the ingest and patch suites, because a real zip
//! is tens of megabytes and carries real players' names. **Deliberately independent of
//! `PUNA_REQUIRE_DB_TESTS`**: these tests touch no database, and tying the two guards together is
//! exactly the mistake that once made a suite pass locally and fail every CI run, since CI has a
//! database and no zip.
//!
//! ```text
//! PUNA_TEST_GENERATION_ZIP=~/games/Archipelago/output/AP_14318265276849580066.zip cargo test
//! ```

use pahoa_multidata::MultiData;
use puna_core::artifact::{self, names};

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

/// Promote a real zip into a temp data dir and hand back the seed exactly as the tracker's cache
/// will read it — through `GenerationPaths::seed`, which is the path both the ingest route and the
/// admin rebuild use.
fn promoted_seed(bytes: &[u8]) -> (tempfile::TempDir, Vec<u8>, MultiData) {
    let meta = artifact::inspect(bytes, LIMIT).expect("a real generation zip must parse");
    let dir = tempfile::tempdir().expect("tempdir");
    artifact::promote(dir.path(), bytes, &meta, "test-nonce").expect("promote");

    let paths = artifact::GenerationPaths::new(dir.path(), &meta.sha256);
    let seed = std::fs::read(paths.seed()).expect("the promoted seed");
    let data = MultiData::parse(&seed).expect("the seed parses");
    (dir, seed, data)
}

/// **The spoiler property, and the reason this cache stores location ids and nothing else.**
///
/// `MultiData.locations` hands out `(location, item, receiver, flags)` per entry — the answer to
/// "what is in that chest" — and a tracker that leaked it would be a searchable spoiler log. The
/// assertion is that what was extracted is exactly the location list, so the item, the receiver and
/// the flags are absent by construction rather than by a filter someone could later relax.
#[test]
fn a_slots_locations_are_the_location_list_and_nothing_more() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    let (_dir, seed, data) = promoted_seed(&bytes);
    let tables = names::from_seed(&seed).expect("name tables");

    assert!(
        !tables.slot_locations.is_empty(),
        "the fixture has slots that own locations"
    );

    for (slot, ids) in &tables.slot_locations {
        let entries = data
            .locations
            .for_slot(u32::try_from(*slot).expect("a slot number"));

        let mut expected: Vec<i64> = entries.iter().map(|e| e.location).collect();
        expected.sort_unstable();

        assert_eq!(ids, &expected, "slot {slot}'s location list");
        assert!(
            ids.windows(2).all(|w| w[0] <= w[1]),
            "slot {slot}'s locations are not sorted, so the client would have to sort them"
        );
    }

    // A spectator owns no locations and therefore has no row -- rather than an empty one, which
    // would make "nothing to check" and "not cached" the same shape.
    for (slot, info) in &data.slot_info {
        if data.locations.for_slot(*slot).is_empty() {
            let key = i32::try_from(*slot).expect("a slot number");
            assert!(
                !tables.slot_locations.contains_key(&key),
                "slot {slot} ({}) owns no locations but has a row",
                info.name
            );
        }
    }
}

/// Names resolve through the same merge pahoa resolves them through, so the tracker and the room's
/// own chat cannot disagree about what an item is called.
#[test]
fn names_match_the_resolved_datapackage() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    let (_dir, seed, data) = promoted_seed(&bytes);
    let tables = names::from_seed(&seed).expect("name tables");
    let (package, _report) = data.resolve_datapackage();

    assert!(!tables.games.is_empty(), "the fixture has games");

    let mut compared = 0usize;
    for (game, extracted) in &tables.games {
        let reference = package
            .get(game)
            .unwrap_or_else(|| panic!("{game} is missing from the resolved package"));

        for (id, name) in &extracted.items {
            assert_eq!(&reference.item_name(*id), name, "{game} item {id}");
            compared += 1;
        }
        for (id, name) in &extracted.locations {
            assert_eq!(&reference.location_name(*id), name, "{game} location {id}");
            compared += 1;
        }
    }

    assert!(
        compared > 0,
        "no names were compared, so this proved nothing"
    );
    eprintln!(
        "compared {compared} names across {} games, ~{} KiB",
        tables.games.len(),
        tables.approximate_bytes() / 1024
    );
}

/// Every game a slot is playing has names, because a slot whose game is missing renders a table of
/// raw ids — survivable, but it would make the whole feature look broken for that player.
#[test]
fn every_played_game_has_a_name_table() {
    let Some(bytes) = fixture() else {
        eprintln!("PUNA_TEST_GENERATION_ZIP unset; skipping");
        return;
    };
    let (_dir, seed, data) = promoted_seed(&bytes);
    let tables = names::from_seed(&seed).expect("name tables");

    for info in data.slot_info.values() {
        assert!(
            tables.games.contains_key(&info.game),
            "{} is played but has no name table",
            info.game
        );
    }
}
