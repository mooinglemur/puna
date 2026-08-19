//! Resolving a tracker id, and the cached document behind it.
//!
//! ## One id space, two kinds of target
//!
//! `rooms.tracker_id` and `room_slots.tracker_id` are both unguessable uuids drawn from the same
//! space, and [`resolve`] tries them in that order. A bare `/tracker/<uuid>` therefore does not
//! disclose which kind it is until it resolves — which matters because the two are shared with
//! different audiences: a multiworld tracker goes to everyone watching, and a slot tracker is what
//! one player hands their own stream chat.
//!
//! **Neither id is derivable from the room's own.** That is the whole point: the reference's
//! `/tracker/<id>/<team>/<player>` leaks the multiworld id to anyone holding a per-slot link, so
//! sharing your own tracker shares the room. Here the room id, the room's tracker id and each
//! slot's tracker id are three unrelated uuids, and only the room id opens the room page.
//!
//! ## The cache is a column, not a memory
//!
//! `rooms.last_tracker_doc` holds the last document a proxy fetch got. It exists for the case that
//! is most of an async's life: **the room is torn down**, and a tracker link is the thing people
//! keep open. Serving the last known state with an "as of" stamp is better than an error page, and
//! it is also what makes the shared cache shared — a per-process one would multiply upstream
//! fetches by the replica count instead of amortizing them.

use chrono::{DateTime, Utc};
use diesel::sql_types::{Integer, Jsonb, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::ids::{RoomId, TrackerId};

/// What a tracker id turned out to name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The whole multiworld.
    Room { room_id: RoomId },
    /// One slot of it, by an id that is not derivable from the room's.
    Slot {
        room_id: RoomId,
        slot_number: i32,
        player_name: String,
    },
}

impl Target {
    pub fn room_id(&self) -> RoomId {
        match self {
            Self::Room { room_id } | Self::Slot { room_id, .. } => *room_id,
        }
    }

    pub fn slot_number(&self) -> Option<i32> {
        match self {
            Self::Room { .. } => None,
            Self::Slot { slot_number, .. } => Some(*slot_number),
        }
    }
}

/// Rooms first, then slots. `None` is an id that names nothing.
pub async fn resolve(
    conn: &mut AsyncPgConnection,
    id: TrackerId,
) -> Result<Option<Target>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct RoomRow {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
    }

    let rooms: Vec<RoomRow> = diesel::sql_query("SELECT id FROM rooms WHERE tracker_id = $1")
        .bind::<SqlUuid, _>(id)
        .load(conn)
        .await?;
    if let Some(row) = rooms.into_iter().next() {
        return Ok(Some(Target::Room { room_id: row.id }));
    }

    #[derive(diesel::QueryableByName)]
    struct SlotRow {
        #[diesel(sql_type = SqlUuid)]
        room_id: RoomId,
        #[diesel(sql_type = Integer)]
        slot_number: i32,
        #[diesel(sql_type = Text)]
        player_name: String,
    }

    let slots: Vec<SlotRow> = diesel::sql_query(
        "SELECT room_id, slot_number, player_name FROM room_slots WHERE tracker_id = $1",
    )
    .bind::<SqlUuid, _>(id)
    .load(conn)
    .await?;

    Ok(slots.into_iter().next().map(|row| Target::Slot {
        room_id: row.room_id,
        slot_number: row.slot_number,
        player_name: row.player_name,
    }))
}

/// The last documents a proxy fetch saw, and when.
#[derive(Debug, Clone)]
pub struct CachedDocuments {
    pub live: Option<serde_json::Value>,
    pub statics: Option<serde_json::Value>,
    pub at: DateTime<Utc>,
}

/// The two documents share one column, under these keys.
///
/// One column rather than two because they are written together and read together, and because a
/// room whose live document is cached and whose static one is not would render a slot table with no
/// game names — a state worth not being able to represent.
const LIVE_KEY: &str = "tracker";
const STATIC_KEY: &str = "static_tracker";

pub async fn cached(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Option<CachedDocuments>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Nullable<Jsonb>)]
        last_tracker_doc: Option<serde_json::Value>,
        #[diesel(sql_type = Nullable<Timestamptz>)]
        last_tracker_at: Option<DateTime<Utc>>,
    }

    let rows: Vec<Row> =
        diesel::sql_query("SELECT last_tracker_doc, last_tracker_at FROM rooms WHERE id = $1")
            .bind::<SqlUuid, _>(room)
            .load(conn)
            .await?;

    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let (Some(doc), Some(at)) = (row.last_tracker_doc, row.last_tracker_at) else {
        return Ok(None);
    };

    Ok(Some(CachedDocuments {
        live: doc.get(LIVE_KEY).cloned(),
        statics: doc.get(STATIC_KEY).cloned(),
        at,
    }))
}

/// Store what a fetch returned, if it is small enough to be worth storing.
///
/// **Over the cap the column is left alone rather than truncated**, and the caller says so on the
/// page. A truncated tracker document is not a smaller tracker document; it is invalid JSON that
/// would fail to parse on every later read, turning a size problem into a permanent one.
pub async fn store(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    live: Option<&serde_json::Value>,
    statics: Option<&serde_json::Value>,
    max_bytes: usize,
) -> Result<bool, diesel::result::Error> {
    let mut doc = serde_json::Map::new();
    if let Some(live) = live {
        doc.insert(LIVE_KEY.to_string(), live.clone());
    }
    if let Some(statics) = statics {
        doc.insert(STATIC_KEY.to_string(), statics.clone());
    }
    if doc.is_empty() {
        return Ok(false);
    }

    let value = serde_json::Value::Object(doc);
    // Measured on the rendered form, because that is what Postgres stores and what a later read
    // has to parse.
    if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > max_bytes) {
        return Ok(false);
    }

    diesel::sql_query(
        "UPDATE rooms SET last_tracker_doc = $2, last_tracker_at = now() WHERE id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Jsonb, _>(value)
    .execute(conn)
    .await?;
    Ok(true)
}
