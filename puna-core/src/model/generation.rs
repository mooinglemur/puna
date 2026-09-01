//! Indexing a generation in Postgres.
//!
//! The bytes live on CephFS, content-addressed (see [`crate::artifact::storage`]); Postgres holds
//! only the index. That split is why a `generations` row can be shared by any number of rooms and
//! by both sources at once, and why deleting a room never deletes a generation.
//!
//! ## Insertion is idempotent on the content hash
//!
//! `sha256` is `UNIQUE`, so re-uploading the same zip converges on the existing row rather than
//! creating a second one: the same convergence the filesystem gets from naming a directory after
//! its hash, expressed with the same input. [`insert`] reports which happened.
//!
//! ## Two different questions, and telling them apart is a disclosure boundary
//!
//! [`Insertion::created`] is GLOBAL: were these bytes already indexed, by anyone. [`record_upload`]
//! is PER USER: had *this* person uploaded them before. They diverge exactly when a second account
//! uploads a zip somebody else already has, and there, only the second answer may be shown.
//! Reporting the global one tells the uploader that another account holds the same seed, which they
//! came with their own copy of the bytes and no right to learn.
//!
//! So: `created` decides whether to write the index and the files. `record_upload` decides what the
//! page says. A caller reaching for `created` to phrase a message is reaching for the wrong one.
//!
//! Provenance beyond that lives elsewhere. `first_ingested_by` is who got here first and nothing
//! more: authority over who holds a reference is `generation_uploads`. Who opened a room and where
//! it came from live on `rooms`, because one generation may back a direct upload and a lobby push at
//! the same time and the bytes cannot say which.

// In scope for `#[diesel(embed)]`, which expands to an unqualified call on the embedded struct.
use diesel::deserialize::QueryableByName;
use diesel::sql_types::{Array, BigInt, Bool, Bytea, Integer, Nullable, Text, Uuid as SqlUuid};
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

use crate::artifact::{GenerationMeta, SlotKind};
use crate::ids::GenerationId;

/// What [`insert`] did: two answers to two different questions.
///
/// **They are not interchangeable and only one of them may be shown to the uploader.** See the
/// module docs: `created` is about everybody, `first_for_this_user` is about the caller, and they
/// disagree exactly when somebody uploads a zip another account already had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insertion {
    pub id: GenerationId,
    /// False when these bytes were already indexed **by anyone**, in which case `id` is the
    /// existing row's. Decides whether the index and the files were written.
    ///
    /// Never render anything derived from this. It is a fact about other people's uploads.
    pub created: bool,
    /// False when **this** user had already uploaded these bytes. The only one of the two that may
    /// reach a page.
    pub first_for_this_user: bool,
}

/// Index a generation, or find the existing row for the same bytes, and record the caller's
/// reference to it either way.
///
/// Runs in one transaction: a `generations` row without its `generation_slots` would be a
/// generation from which no room could be built, and the slot table is not reconstructible from
/// the row: it would need the zip re-read.
///
/// **The reference is recorded HERE rather than by the caller**, and that is not tidiness. Indexing
/// a generation without recording who uploaded it produces an upload that succeeded and then does
/// not appear in the uploader's list, with nothing failing anywhere. A caller that must remember
/// a second call is a caller that will forget it, and the lobby push is a second caller waiting to
/// happen. Same transaction, so it cannot half-happen either.
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
            // pahoa derives its outbound budget from `slot_info.len()`: every slot, groups
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
                // The interesting path: somebody has these bytes. Whether that somebody is the
                // caller is what the answer below distinguishes, and it is the only part of this
                // the caller may show them.
                let first_for_this_user = record_upload(conn, id, first_ingested_by).await?;
                return Ok(Insertion {
                    id,
                    created: false,
                    first_for_this_user,
                });
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

            // A row this statement just created cannot have a reference yet, so this is always
            // true, taken from the write rather than assumed, so the two can never disagree.
            let first_for_this_user = record_upload(conn, row.id, first_ingested_by).await?;

            Ok(Insertion {
                id: row.id,
                created: true,
                first_for_this_user,
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

/// The same columns, prefixed for a join. Derived from [`GENERATION_COLUMNS`] rather than written
/// out again, so a column added there cannot be missing here, which would fail at runtime, in the
/// listing only, as a deserialization error rather than as anything that names the cause.
fn qualified_generation_columns(alias: &str) -> String {
    GENERATION_COLUMNS
        .split(',')
        .map(|column| format!("{alias}.{}", column.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

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

/// Record that `user_id` uploaded `generation`, and say whether that was news.
///
/// **`false` means this user had already uploaded these exact bytes**, which is the only sense in
/// which an upload is a duplicate to the person making it. It is deliberately not the same question
/// as [`Insertion::created`]: that one is global, and answering a second uploader with it discloses
/// that somebody else holds the same seed.
///
/// Idempotent, so a repeat upload converges on one reference rather than accumulating them, and
/// `uploaded_at` keeps the FIRST time this user uploaded it: a re-upload is the same act, not a
/// newer one, and touching the timestamp would reshuffle their listing for no reason.
pub async fn record_upload(
    conn: &mut AsyncPgConnection,
    generation: GenerationId,
    user_id: i64,
) -> Result<bool, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        #[allow(dead_code)]
        generation_id: GenerationId,
    }

    // `DO NOTHING` returns no row on conflict, the same signal `insert` reads: one row back means
    // this user had not uploaded it before.
    let inserted: Vec<Row> = diesel::sql_query(
        "INSERT INTO generation_uploads (generation_id, user_id)
         VALUES ($1, $2)
         ON CONFLICT (generation_id, user_id) DO NOTHING
         RETURNING generation_id",
    )
    .bind::<SqlUuid, _>(generation)
    .bind::<BigInt, _>(user_id)
    .load(conn)
    .await?;
    Ok(!inserted.is_empty())
}

/// A generation as it appears in one user's own listing.
///
/// Carries `uploaded_at` separately from `generation.created_at` because for a second uploader they
/// are different moments: the generation dates from whenever it first arrived, under whoever's
/// account that was. The listing must show the reader's own, or it dates their upload to a day they
/// had nothing to do with.
#[derive(Debug, Clone)]
pub struct Upload {
    pub generation: Generation,
    pub uploaded_at: chrono::DateTime<chrono::Utc>,
}

/// The generations a user has uploaded, newest of THEIR uploads first.
///
/// Scoped through `generation_uploads` rather than listing everything: a generation is shared and
/// content-addressed, so a global list would show every other user's uploads to anyone who reached
/// the page. Scoped through that table rather than `first_ingested_by` because dedup means a
/// generation legitimately has more than one uploader, and the column can only hold one.
pub async fn list_for_user(
    conn: &mut AsyncPgConnection,
    user_id: i64,
    limit: i64,
) -> Result<Vec<Upload>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(embed)]
        generation: GenerationRow,
        #[diesel(sql_type = diesel::sql_types::Timestamptz)]
        uploaded_at: chrono::DateTime<chrono::Utc>,
    }

    let rows: Vec<Row> = diesel::sql_query(format!(
        "SELECT {}, u.uploaded_at
           FROM generation_uploads u
           JOIN generations g ON g.id = u.generation_id
          WHERE u.user_id = $1
          ORDER BY u.uploaded_at DESC
          LIMIT $2",
        qualified_generation_columns("g")
    ))
    .bind::<BigInt, _>(user_id)
    .bind::<BigInt, _>(limit)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Upload {
            generation: Generation::from(row.generation),
            uploaded_at: row.uploaded_at,
        })
        .collect())
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
