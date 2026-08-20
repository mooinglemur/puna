//! The tracker's name cache, in Postgres.
//!
//! Written by the **web** tier at ingest, because it is the tier that opens the zip; read by the
//! **tracker** tier, which has no filesystem at all and could not otherwise resolve an item id to
//! a word. See [`crate::artifact::names`] for what is extracted and what is deliberately not.
//!
//! ## Everything here is a cache, and the code has to behave like it
//!
//! Three rules follow, and they are the whole reason this is a table rather than a source of truth:
//!
//! 1. **A missing row is normal**, not an error. Generations ingested before this existed have
//!    none, and a rebuild may not have run. Readers return what they have and callers render the
//!    raw id — which is what the reference does too (`Unknown Item (ID: n)`).
//! 2. **Writes are upserts**, so a rebuild is the same code path as a first write. A repair that
//!    needed its own statement would be a repair nobody had tested.
//! 3. **A write failure must not fail the thing that triggered it.** A generation whose names did
//!    not store is a usable generation with a worse tracker, and refusing the upload over it would
//!    trade something that matters for something that does not.
//!
//! ## Scoped per generation
//!
//! Never keyed by game alone. These names come from a datapackage embedded in an uploaded zip, so
//! a hostile or malformed one is confined to the generation it arrived in. The migration carries
//! the full argument.

use std::collections::BTreeMap;

use diesel::sql_types::{Array, BigInt, Integer, Jsonb, Text, Uuid as SqlUuid};
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

use crate::artifact::names::{GameNames, NameTables};
use crate::ids::GenerationId;

/// Replace a generation's name tables.
///
/// **Replace, not merge**, and the distinction is the whole reason this deletes first. An upsert
/// keyed by `(generation_id, game)` can only ever add or overwrite, so a rebuild that produced
/// *fewer* games than the run before it — after a fix to the extraction, say — would leave the
/// extra rows behind with nothing able to shift them. "Rebuild" has to mean what it says, or the
/// repair path has a state it cannot repair.
///
/// One transaction, so a rebuild cannot be observed half-done: the delete and the writes land
/// together, and a reader holding a mix of two datapackages would produce names that are
/// individually plausible and collectively wrong — the hardest kind of wrong to notice.
pub async fn store(
    conn: &mut AsyncPgConnection,
    generation_id: GenerationId,
    tables: &NameTables,
) -> Result<(), diesel::result::Error> {
    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        async move {
            for table in ["generation_game_names", "generation_slot_locations"] {
                // The table name is a literal from the array above, never a caller's string.
                diesel::sql_query(format!("DELETE FROM {table} WHERE generation_id = $1"))
                    .bind::<SqlUuid, _>(generation_id)
                    .execute(conn)
                    .await?;
            }

            for (game, names) in &tables.games {
                diesel::sql_query(
                    "INSERT INTO generation_game_names
                        (generation_id, game, item_names, location_names)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (generation_id, game) DO UPDATE
                        SET item_names = EXCLUDED.item_names,
                            location_names = EXCLUDED.location_names",
                )
                .bind::<SqlUuid, _>(generation_id)
                .bind::<Text, _>(game)
                .bind::<Jsonb, _>(as_json(&names.items))
                .bind::<Jsonb, _>(as_json(&names.locations))
                .execute(conn)
                .await?;
            }

            for (slot, locations) in &tables.slot_locations {
                diesel::sql_query(
                    "INSERT INTO generation_slot_locations
                        (generation_id, slot_number, location_ids)
                     VALUES ($1, $2, $3)
                     ON CONFLICT (generation_id, slot_number) DO UPDATE
                        SET location_ids = EXCLUDED.location_ids",
                )
                .bind::<SqlUuid, _>(generation_id)
                .bind::<Integer, _>(*slot)
                .bind::<Array<BigInt>, _>(locations)
                .execute(conn)
                .await?;
            }

            Ok(())
        }
        .scope_boxed()
    })
    .await
}

/// One game's names, or `None` if this generation has no cached row for it.
///
/// Queried per game rather than as one blob because a hint table needs two *different* games' —
/// the item resolves in the receiving slot's game and the location in the finding slot's — so
/// "load everything" and "load what this row needs" are far apart on a multi-game seed.
pub async fn game(
    conn: &mut AsyncPgConnection,
    generation_id: GenerationId,
    game: &str,
) -> Result<Option<GameNames>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Jsonb)]
        item_names: serde_json::Value,
        #[diesel(sql_type = Jsonb)]
        location_names: serde_json::Value,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT item_names, location_names FROM generation_game_names
          WHERE generation_id = $1 AND game = $2",
    )
    .bind::<SqlUuid, _>(generation_id)
    .bind::<Text, _>(game)
    .load(conn)
    .await?;

    Ok(rows.into_iter().next().map(|row| GameNames {
        items: from_json(&row.item_names),
        locations: from_json(&row.location_names),
    }))
}

/// Every game's names for one generation — what a whole-multiworld hint table needs.
pub async fn all_games(
    conn: &mut AsyncPgConnection,
    generation_id: GenerationId,
) -> Result<BTreeMap<String, GameNames>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        game: String,
        #[diesel(sql_type = Jsonb)]
        item_names: serde_json::Value,
        #[diesel(sql_type = Jsonb)]
        location_names: serde_json::Value,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT game, item_names, location_names FROM generation_game_names
          WHERE generation_id = $1",
    )
    .bind::<SqlUuid, _>(generation_id)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.game,
                GameNames {
                    items: from_json(&row.item_names),
                    locations: from_json(&row.location_names),
                },
            )
        })
        .collect())
}

/// Every location in one slot's own world, checked or not.
///
/// `None` is "not cached"; an empty vector is not a state this stores, because a slot with no
/// locations — a spectator — has no row rather than an empty one.
pub async fn slot_locations(
    conn: &mut AsyncPgConnection,
    generation_id: GenerationId,
    slot_number: i32,
) -> Result<Option<Vec<i64>>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Array<BigInt>)]
        location_ids: Vec<i64>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT location_ids FROM generation_slot_locations
          WHERE generation_id = $1 AND slot_number = $2",
    )
    .bind::<SqlUuid, _>(generation_id)
    .bind::<Integer, _>(slot_number)
    .load(conn)
    .await?;

    Ok(rows.into_iter().next().map(|row| row.location_ids))
}

/// Whether this generation has any cached names at all — the question an admin listing asks.
pub async fn is_cached(
    conn: &mut AsyncPgConnection,
    generation_id: GenerationId,
) -> Result<bool, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        present: bool,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM generation_game_names WHERE generation_id = $1) AS present",
    )
    .bind::<SqlUuid, _>(generation_id)
    .load(conn)
    .await?;

    Ok(rows.into_iter().next().is_some_and(|row| row.present))
}

/// What the admin page shows: one generation, and how much of it is cached.
///
/// The counts are deliberately counts rather than a boolean. "Some games cached" is a real state — a
/// rebuild that half-failed, or a seed whose extraction changed — and it looks nothing like "none",
/// which is the ordinary pre-Stage-A case. Collapsing them would hide the one that needs attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStatus {
    pub id: GenerationId,
    pub seed_name: String,
    pub games: Vec<String>,
    pub slots: i32,
    pub games_cached: i64,
    pub slots_cached: i64,
}

impl CacheStatus {
    /// Whether the tracker can name anything at all for this generation.
    pub fn cached(&self) -> bool {
        self.games_cached > 0
    }

    /// Whether every game being played has names. False here with `cached()` true is the
    /// half-a-rebuild state worth looking at.
    pub fn complete(&self) -> bool {
        self.games_cached >= self.games.len() as i64 && !self.games.is_empty()
    }
}

/// Every generation, newest first, with its cache counts.
///
/// A global listing, unlike `generation::list_for_user` — generations are content-addressed and
/// shared, so a per-user list is the right default *for a user*. An operator repairing a cache needs
/// to see the ones nobody has claimed as well.
pub async fn status(
    conn: &mut AsyncPgConnection,
) -> Result<Vec<CacheStatus>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: GenerationId,
        #[diesel(sql_type = Text)]
        seed_name: String,
        #[diesel(sql_type = Array<Text>)]
        games: Vec<String>,
        #[diesel(sql_type = Integer)]
        slots: i32,
        #[diesel(sql_type = BigInt)]
        games_cached: i64,
        #[diesel(sql_type = BigInt)]
        slots_cached: i64,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT g.id, g.seed_name, g.games, g.slots,
                (SELECT count(*) FROM generation_game_names n WHERE n.generation_id = g.id)
                  AS games_cached,
                (SELECT count(*) FROM generation_slot_locations l WHERE l.generation_id = g.id)
                  AS slots_cached
           FROM generations g
          ORDER BY g.created_at DESC",
    )
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| CacheStatus {
            id: row.id,
            seed_name: row.seed_name,
            games: row.games,
            slots: row.slots,
            games_cached: row.games_cached,
            slots_cached: row.slots_cached,
        })
        .collect())
}

/// One generation, as the rebuild path needs it: its id and the sha that names its directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuildable {
    pub id: GenerationId,
    pub sha256: [u8; 32],
}

/// Generations with no cached names, oldest first.
///
/// This is the backfill's work list, and the reason it is a query rather than a migration: the
/// names cannot be derived in SQL — they come out of a file on a volume Postgres cannot see — so
/// the repair has to run somewhere with the mount, which means it has to be able to ask what is
/// missing.
///
/// A row whose `sha256` is not 32 bytes is skipped rather than surfaced: it could not name a
/// directory, so there is nothing on disk to rebuild from.
pub async fn uncached(
    conn: &mut AsyncPgConnection,
) -> Result<Vec<Rebuildable>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: GenerationId,
        #[diesel(sql_type = diesel::sql_types::Bytea)]
        sha256: Vec<u8>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT g.id, g.sha256 FROM generations g
          WHERE NOT EXISTS (
                SELECT 1 FROM generation_game_names n WHERE n.generation_id = g.id)
          ORDER BY g.created_at",
    )
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id = row.id;
            let length = row.sha256.len();
            match <[u8; 32]>::try_from(row.sha256) {
                Ok(sha256) => Some(Rebuildable { id, sha256 }),
                Err(_) => {
                    // Warned rather than dropped quietly. A repair tool that silently omits work is
                    // worse than one that fails: the backfill would report success having skipped
                    // the one generation nobody could explain.
                    tracing::warn!(
                        generation = %id,
                        length,
                        "this generation's stored hash is not 32 bytes, so it names no directory \
                         on disk and its names cannot be rebuilt"
                    );
                    None
                }
            }
        })
        .collect())
}

/// Ids become JSON object keys, which are strings by definition.
///
/// Deliberately not an array indexed by id: Archipelago ids are sparse and can be negative (the
/// reference uses negatives for a few built-ins), so an array would be enormous and wrong.
fn as_json(map: &BTreeMap<i64, String>) -> serde_json::Value {
    serde_json::Value::Object(
        map.iter()
            .map(|(id, name)| (id.to_string(), serde_json::Value::String(name.clone())))
            .collect(),
    )
}

/// The inverse, tolerating anything that is not a well-formed entry by dropping it.
///
/// A cache that fails to parse must degrade to *fewer names*, never to an error: the caller's
/// fallback is rendering the id, which is exactly what a dropped entry produces.
fn from_json(value: &serde_json::Value) -> BTreeMap<i64, String> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter_map(|(id, name)| Some((id.parse().ok()?, name.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_json_including_negative_and_sparse_ones() {
        let mut map = BTreeMap::new();
        map.insert(-1, "Server item".to_string());
        map.insert(0, "Zero".to_string());
        map.insert(9_999_999_999, "A high id".to_string());

        assert_eq!(from_json(&as_json(&map)), map);
    }

    /// A cache this build cannot read degrades to fewer names, never to an error — because the
    /// caller's fallback for a missing name is rendering the id, which is a worse label and not a
    /// broken page.
    #[test]
    fn unreadable_entries_are_dropped_rather_than_failing() {
        let value = serde_json::json!({
            "12": "Bow",
            "not-a-number": "Ignored",
            "13": 7,
        });

        let parsed = from_json(&value);
        assert_eq!(parsed.get(&12).map(String::as_str), Some("Bow"));
        assert_eq!(parsed.len(), 1, "{parsed:?}");

        // And something that is not an object at all is empty rather than a panic.
        assert!(from_json(&serde_json::json!([1, 2, 3])).is_empty());
        assert!(from_json(&serde_json::Value::Null).is_empty());
    }
}
