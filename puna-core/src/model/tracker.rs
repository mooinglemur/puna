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
//!
//! ## The documents cross this boundary as TEXT, never as a `serde_json::Value`
//!
//! The column is `jsonb` and the tracker tier is a proxy: it serves these documents back verbatim
//! and hashes them for an `ETag`, so on the room-scoped path nothing here ever needs their
//! structure. Handing a `Value` over meant parsing on the read, cloning the tree, and rendering it
//! again — three times the peak of a document that is 17.6 MiB on the wire for a 2000-slot room,
//! which is what OOM-killed the tier. So the read casts to text in Postgres and the write casts
//! back, and the merge of the two documents happens **in SQL** rather than by reading the column
//! into this process first.

use chrono::{DateTime, Utc};
use diesel::sql_types::{Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
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
///
/// Each is the document's own JSON **text**, exactly as it will be served — see the note at the top
/// of this module for why it is not a `serde_json::Value`.
#[derive(Debug, Clone)]
pub struct CachedDocuments {
    pub live: Option<String>,
    pub statics: Option<String>,
    pub at: DateTime<Utc>,
}

impl CachedDocuments {
    /// Take one document out, rather than cloning it.
    ///
    /// A caller wants exactly one of the two and then owns it; at 17.6 MiB a clone is not a detail.
    pub fn take(&mut self, kind: Kind) -> Option<String> {
        match kind {
            Kind::Live => self.live.take(),
            Kind::Static => self.statics.take(),
        }
    }
}

/// Which of a room's two tracker documents a cache entry is for.
///
/// The web tier has its own `Document` for the same distinction, because it also carries the
/// upstream path and the cache window — neither of which belongs in a model. This exists so
/// [`store`] can be told which key to write without being handed a string, where the only two
/// valid values would live in another crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Live,
    Static,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Self::Live => LIVE_KEY,
            Self::Static => STATIC_KEY,
        }
    }
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
        #[diesel(sql_type = Nullable<Text>)]
        live: Option<String>,
        #[diesel(sql_type = Nullable<Text>)]
        statics: Option<String>,
        #[diesel(sql_type = Nullable<Timestamptz>)]
        last_tracker_at: Option<DateTime<Utc>>,
    }

    // **Postgres renders each document to text and this process never parses it.** The keys are
    // bound rather than interpolated -- they are compile-time constants, not input, but binding
    // them is what keeps `store`'s idea of the keys and this one from being two things that could
    // drift. `->` then `::text` rather than `->>`, because the two agree for an object and only the
    // first stays valid JSON for anything else.
    let rows: Vec<Row> = diesel::sql_query(
        "SELECT (last_tracker_doc -> $2)::text AS live,
                (last_tracker_doc -> $3)::text AS statics,
                last_tracker_at
           FROM rooms WHERE id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Text, _>(LIVE_KEY)
    .bind::<Text, _>(STATIC_KEY)
    .load(conn)
    .await?;

    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let Some(at) = row.last_tracker_at else {
        return Ok(None);
    };
    if row.live.is_none() && row.statics.is_none() {
        return Ok(None);
    }

    Ok(Some(CachedDocuments {
        live: row.live,
        statics: row.statics,
        at,
    }))
}

/// Store one document, if it is small enough to be worth storing.
///
/// **Over the cap the column is left alone rather than truncated**, and the caller says so on the
/// page. A truncated tracker document is not a smaller tracker document; it is invalid JSON that
/// would fail to parse on every later read, turning a size problem into a permanent one.
///
/// **`max_bytes` bounds one document rather than the pair**, which is a change from the form that
/// merged in Rust: the merge is now a SQL `||`, so this side never holds both at once and has
/// nothing to measure the pair with. The column's worst case is therefore twice the cap. That is
/// the better bargain anyway — under the old rule a live document that fit was refused because the
/// static one beside it did not.
///
/// The merge is server-side for the same reason the read casts to text: doing it here meant reading
/// the column back, parsing it, and re-rendering it on every write, which is exactly the cost this
/// module exists to stop paying. It also cannot evict the other key, which is what the read-modify-
/// write was for.
///
/// `body` is parsed **by Postgres** on the cast to `jsonb`, so a room that answers with something
/// that is not JSON fails here rather than being cached. That is an `Err`, and the caller warns:
/// the document is still served, because the room is what said it.
pub async fn store(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    kind: Kind,
    body: &str,
    max_bytes: usize,
) -> Result<bool, diesel::result::Error> {
    if body.len() > max_bytes {
        return Ok(false);
    }

    diesel::sql_query(
        "UPDATE rooms
            SET last_tracker_doc = COALESCE(last_tracker_doc, '{}'::jsonb)
                                   || jsonb_build_object($2::text, $3::jsonb),
                last_tracker_at = now()
          WHERE id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Text, _>(kind.key())
    .bind::<Text, _>(body)
    .execute(conn)
    .await?;
    Ok(true)
}
