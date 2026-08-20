//! Pulling the tracker's name tables out of a seed.
//!
//! Three of the tracker's four tables render **names** — items received, locations checked, hints —
//! and the two documents pahoa serves carry only numeric ids. The reference implementation solves
//! this by being an Archipelago install; Puna solves it by reading the seed it already has, which
//! it can do better than the reference in one respect: `MultiData.locations` carries every location
//! in a slot's world, so Puna can show the ones a slot has **not** checked.
//!
//! ## What is deliberately not extracted
//!
//! `LocationEntry` is `(location, item, sender, receiver, flags)`, and only `location` is taken.
//! The other fields are the answer to "what is in that chest" — the seed's central spoiler — and a
//! tracker that leaked them would be a spoiler log with a search box. Dropping them here, rather
//! than filtering them at render time, is what makes that structural: the data never enters the
//! cache, so no later code path can be careless with it.
//!
//! ## Names are per game, and per generation
//!
//! Item and location ids are namespaced by game, so the tables are keyed by game — and the whole
//! set is keyed by *generation* upstream of that, because these names come out of a datapackage
//! embedded in an uploaded zip. See the migration for why that scoping is the load-bearing part.

use std::collections::BTreeMap;

use pahoa_multidata::MultiData;

use super::IngestError;

/// One game's id → name lookups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameNames {
    pub items: BTreeMap<i64, String>,
    pub locations: BTreeMap<i64, String>,
}

/// Everything the tracker needs from a seed that the live documents do not carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameTables {
    /// Keyed by game name, as the documents spell it.
    pub games: BTreeMap<String, GameNames>,
    /// Slot number → every location in **that slot's own world**, in ascending order.
    ///
    /// Sorted because the tracker renders them in a stable order and a client sorting a few
    /// thousand rows on load is work that can be done once here instead.
    pub slot_locations: BTreeMap<i32, Vec<i64>>,
}

impl NameTables {
    /// Roughly what this will cost in the database, for the log line at ingest.
    ///
    /// Worth having because the size is the one thing about this cache that could surprise someone:
    /// it scales with the seed's *games*, not its slots, so a twelve-player twelve-game room is
    /// dearer than a hundred-player one-game room.
    pub fn approximate_bytes(&self) -> usize {
        let names: usize = self
            .games
            .iter()
            .map(|(game, names)| {
                game.len()
                    + names
                        .items
                        .values()
                        .chain(names.locations.values())
                        .map(|n| n.len() + 8)
                        .sum::<usize>()
            })
            .sum();
        let locations: usize = self.slot_locations.values().map(|l| l.len() * 8).sum();
        names + locations
    }
}

/// Read a `.archipelago` and extract the tracker's tables.
///
/// **Takes the seed's bytes rather than the zip**, and that is what lets one function serve both
/// callers: ingest has just written `seed.archipelago`, and a rebuild reads the same file back.
/// A rebuild that re-derived this some other way would be a second implementation of the thing
/// most worth having only one of.
pub fn from_seed(seed: &[u8]) -> Result<NameTables, IngestError> {
    let data = MultiData::parse(seed).map_err(|e| IngestError::Multidata(e.to_string()))?;
    Ok(from_multidata(&data))
}

/// Split from [`from_seed`] so tests can hold the `MultiData` and compare against it directly —
/// asserting the tables *match the seed* rather than merely that they parsed.
pub fn from_multidata(data: &MultiData) -> NameTables {
    // The MERGED package, not `embedded_datapackage`: `resolve_datapackage` is what pahoa itself
    // resolves names through, so anything else would put two different answers to "what is this
    // item called" in front of the same player.
    let (package, _report) = data.resolve_datapackage();

    let games = package
        .games()
        .map(|(game, names)| {
            // `GameNames` keeps its own reverse maps private, so the inversion happens here. The
            // forward maps are `BTreeMap<String, i64>` and are the authoritative direction; a
            // duplicate id would be a broken datapackage, and last-writer-wins matches what
            // pahoa's own `build_reverse` does with one.
            let invert = |forward: &BTreeMap<String, i64>| -> BTreeMap<i64, String> {
                forward
                    .iter()
                    .map(|(name, id)| (*id, name.clone()))
                    .collect()
            };

            (
                game.clone(),
                GameNames {
                    items: invert(&names.package.item_name_to_id),
                    locations: invert(&names.package.location_name_to_id),
                },
            )
        })
        .collect();

    // Only slots that own locations. A spectator owns none, and storing an empty array for one
    // would make "this slot has nothing to check" and "this slot's row is missing" the same shape.
    let mut slot_locations: BTreeMap<i32, Vec<i64>> = BTreeMap::new();
    for slot in data.slot_info.keys() {
        let entries = data.locations.for_slot(*slot);
        if entries.is_empty() {
            continue;
        }
        // `.location` and nothing else. See the module docs: the rest of the entry is the spoiler.
        let mut ids: Vec<i64> = entries.iter().map(|entry| entry.location).collect();
        ids.sort_unstable();
        slot_locations.insert(i32::try_from(*slot).unwrap_or(i32::MAX), ids);
    }

    NameTables {
        games,
        slot_locations,
    }
}
