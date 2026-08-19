//! Indexing a generation in Postgres.
//!
//! The bytes live on CephFS, content-addressed (see [`crate::artifact::storage`]); Postgres holds
//! only the index. That split is why a `generations` row can be shared by any number of rooms and
//! by both sources at once, and why deleting a room never deletes a generation.
//!
//! ## Insertion is idempotent on the content hash
//!
//! `sha256` is `UNIQUE`, so re-uploading the same zip converges on the existing row rather than
//! creating a second one -- the same convergence the filesystem gets from naming a directory after
//! its hash, expressed with the same input. [`insert`] reports which happened, because the caller
//! wants to say "already uploaded, here it is" rather than "created".
//!
//! Provenance deliberately is NOT recorded here beyond `first_ingested_by`. Who opened a room and
//! where it came from live on `rooms`, because one generation may back a direct upload and a lobby
//! push at the same time and the bytes cannot say which.

use diesel::sql_types::{Array, BigInt, Bool, Bytea, Integer, Nullable, Text, Uuid as SqlUuid};
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

use crate::artifact::{GenerationMeta, SlotKind};
use crate::ids::GenerationId;

/// What [`insert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insertion {
    pub id: GenerationId,
    /// False when these bytes were already indexed, in which case `id` is the existing row's.
    pub created: bool,
}

/// Index a generation, or find the existing row for the same bytes.
///
/// Runs in one transaction: a `generations` row without its `generation_slots` would be a
/// generation from which no room could be built, and the slot table is not reconstructible from
/// the row -- it would need the zip re-read.
pub async fn insert(
    conn: &mut AsyncPgConnection,
    meta: &GenerationMeta,
    first_ingested_by: i64,
) -> Result<Insertion, diesel::result::Error> {
    let sha256 = meta.sha256.to_vec();

    conn.transaction::<Insertion, diesel::result::Error, _>(|conn| {
        async move {
            #[derive(diesel::QueryableByName)]
            struct Row {
                #[diesel(sql_type = SqlUuid)]
                id: GenerationId,
            }

            let fresh = GenerationId::new();

            // `DO NOTHING` returns no row on conflict, which is exactly the signal wanted: an
            // empty result means somebody else already indexed these bytes.
            let inserted: Vec<Row> = diesel::sql_query(
                "INSERT INTO generations
                    (id, sha256, size_bytes, seed_name, slots, locations, games, race_mode,
                     spoiler_member, min_server_version, first_ingested_by)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 ON CONFLICT (sha256) DO NOTHING
                 RETURNING id",
            )
            .bind::<SqlUuid, _>(fresh)
            .bind::<Bytea, _>(&sha256)
            .bind::<BigInt, _>(meta.size_bytes)
            .bind::<Text, _>(&meta.seed_name)
            // `slot_count`, NOT `slots.len()`. This column sizes the room's memory request, and
            // pahoa derives its outbound budget from `slot_info.len()` -- every slot, groups
            // included. The connectable list is a different number and would under-request.
            .bind::<Integer, _>(meta.slot_count)
            .bind::<BigInt, _>(meta.locations)
            .bind::<Array<Text>, _>(&meta.games)
            .bind::<Bool, _>(meta.race_mode)
            .bind::<Nullable<Text>, _>(meta.spoiler_member.as_deref())
            .bind::<Nullable<Text>, _>(meta.min_server_version.as_deref())
            .bind::<BigInt, _>(first_ingested_by)
            .load(conn)
            .await?;

            let Some(row) = inserted.into_iter().next() else {
                let existing: Vec<Row> =
                    diesel::sql_query("SELECT id FROM generations WHERE sha256 = $1")
                        .bind::<Bytea, _>(&sha256)
                        .load(conn)
                        .await?;
                let id = existing
                    .into_iter()
                    .next()
                    .ok_or(diesel::result::Error::NotFound)?
                    .id;
                return Ok(Insertion { id, created: false });
            };

            for slot in &meta.slots {
                diesel::sql_query(
                    "INSERT INTO generation_slots
                        (generation_id, slot_number, player_name, game, kind, patch_member,
                         patch_size_bytes)
                     VALUES ($1, $2, $3, $4, $5::slot_kind, $6, $7)",
                )
                .bind::<SqlUuid, _>(row.id)
                .bind::<Integer, _>(slot.slot_number)
                .bind::<Text, _>(&slot.player_name)
                .bind::<Text, _>(&slot.game)
                .bind::<Text, _>(kind_as_sql(slot.kind))
                .bind::<Nullable<Text>, _>(slot.patch_member.as_deref())
                .bind::<Nullable<BigInt>, _>(slot.patch_size_bytes)
                .execute(conn)
                .await?;
            }

            Ok(Insertion {
                id: row.id,
                created: true,
            })
        }
        .scope_boxed()
    })
    .await
}

/// The `slot_kind` enum's spelling.
///
/// Deliberately a free function rather than a method on [`SlotKind`]: `puna-core::artifact` reads
/// zips and knows nothing about the schema, and giving it a SQL-shaped method would be the first
/// thread tying the two together.
fn kind_as_sql(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Player => "player",
        SlotKind::Spectator => "spectator",
    }
}

/// One generation, as a listing or a room-creation form shows it.
#[derive(Debug, Clone)]
pub struct Generation {
    pub id: GenerationId,
    pub sha256: Vec<u8>,
    pub size_bytes: i64,
    pub seed_name: String,
    pub slots: i32,
    pub locations: i64,
    pub games: Vec<String>,
    pub race_mode: bool,
    pub has_spoiler: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(diesel::QueryableByName)]
struct GenerationRow {
    #[diesel(sql_type = SqlUuid)]
    id: GenerationId,
    #[diesel(sql_type = Bytea)]
    sha256: Vec<u8>,
    #[diesel(sql_type = BigInt)]
    size_bytes: i64,
    #[diesel(sql_type = Text)]
    seed_name: String,
    #[diesel(sql_type = Integer)]
    slots: i32,
    #[diesel(sql_type = BigInt)]
    locations: i64,
    #[diesel(sql_type = Array<Text>)]
    games: Vec<String>,
    #[diesel(sql_type = Bool)]
    race_mode: bool,
    #[diesel(sql_type = Nullable<Text>)]
    spoiler_member: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Timestamptz)]
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<GenerationRow> for Generation {
    fn from(row: GenerationRow) -> Self {
        Self {
            id: row.id,
            sha256: row.sha256,
            size_bytes: row.size_bytes,
            seed_name: row.seed_name,
            slots: row.slots,
            locations: row.locations,
            games: row.games,
            race_mode: row.race_mode,
            has_spoiler: row.spoiler_member.is_some(),
            created_at: row.created_at,
        }
    }
}

const GENERATION_COLUMNS: &str = "id, sha256, size_bytes, seed_name, slots, locations, games, \
                                  race_mode, spoiler_member, created_at";

/// One generation by id.
pub async fn get(
    conn: &mut AsyncPgConnection,
    id: GenerationId,
) -> Result<Option<Generation>, diesel::result::Error> {
    let rows: Vec<GenerationRow> = diesel::sql_query(format!(
        "SELECT {GENERATION_COLUMNS} FROM generations WHERE id = $1"
    ))
    .bind::<SqlUuid, _>(id)
    .load(conn)
    .await?;
    Ok(rows.into_iter().next().map(Generation::from))
}

/// The generations a user has ingested, newest first.
///
/// Scoped to `first_ingested_by` rather than listing everything: a generation is shared and
/// content-addressed, so a global list would show every other user's uploads to anyone who
/// reached the page.
pub async fn list_for_user(
    conn: &mut AsyncPgConnection,
    user_id: i64,
    limit: i64,
) -> Result<Vec<Generation>, diesel::result::Error> {
    let rows: Vec<GenerationRow> = diesel::sql_query(format!(
        "SELECT {GENERATION_COLUMNS} FROM generations
          WHERE first_ingested_by = $1
          ORDER BY created_at DESC
          LIMIT $2"
    ))
    .bind::<BigInt, _>(user_id)
    .bind::<BigInt, _>(limit)
    .load(conn)
    .await?;
    Ok(rows.into_iter().map(Generation::from).collect())
}

/// One indexed slot.
#[derive(Debug, Clone)]
pub struct Slot {
    pub slot_number: i32,
    pub player_name: String,
    pub game: String,
    pub kind: SlotKind,
    pub patch_member: Option<String>,
    pub patch_size_bytes: Option<i64>,
}

/// Every slot of a generation, in slot order.
pub async fn slots(
    conn: &mut AsyncPgConnection,
    id: GenerationId,
) -> Result<Vec<Slot>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Integer)]
        slot_number: i32,
        #[diesel(sql_type = Text)]
        player_name: String,
        #[diesel(sql_type = Text)]
        game: String,
        #[diesel(sql_type = Text)]
        kind: String,
        #[diesel(sql_type = Nullable<Text>)]
        patch_member: Option<String>,
        #[diesel(sql_type = Nullable<BigInt>)]
        patch_size_bytes: Option<i64>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT slot_number, player_name, game, kind::text AS kind, patch_member, patch_size_bytes
           FROM generation_slots WHERE generation_id = $1 ORDER BY slot_number",
    )
    .bind::<SqlUuid, _>(id)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Slot {
            slot_number: row.slot_number,
            player_name: row.player_name,
            game: row.game,
            // A kind this build does not know cannot arise from `kind_as_sql`, so it would mean a
            // database ahead of the binary. `Player` is the conservative reading: it promises a
            // patch and a game rather than assuming a slot has neither.
            kind: match row.kind.as_str() {
                "spectator" => SlotKind::Spectator,
                _ => SlotKind::Player,
            },
            patch_member: row.patch_member,
            patch_size_bytes: row.patch_size_bytes,
        })
        .collect())
}
