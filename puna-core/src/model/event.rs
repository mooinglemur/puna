//! A room's history: what happened to it, when, and who asked.
//!
//! Written by both tiers and read by the room page. It is the answer to "why is my room doing
//! that", which is a question a support conversation starts with and a state column cannot answer:
//! `idle` does not distinguish "nobody has started it" from "it was up an hour ago and its pod went
//! away".
//!
//! ## `actor` is a string on purpose
//!
//! `web:<discord id>`, `orchestrator`, `reconcile`. Not a foreign key to `users`, because half the
//! actors are not users and never will be, and not an enum, because the set grows every time a new
//! thing writes here. What it must stay is *greppable*: an operator reading a room's history should
//! be able to tell a person's action from a sweep's without knowing the schema.

use chrono::{DateTime, Utc};
use diesel::sql_types::{BigInt, Jsonb, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::ids::RoomId;

#[derive(Debug, Clone)]
pub struct Event {
    pub at: DateTime<Utc>,
    pub actor: String,
    pub kind: String,
    pub detail: serde_json::Value,
}

/// Who caused this.
///
/// A constructor rather than a bare string at each call site, so the `web:<id>` spelling exists in
/// one place and a caller cannot invent `user:<id>` next to it.
#[derive(Debug, Clone, Copy)]
pub enum Actor {
    /// A logged-in person, by Discord id.
    User(i64),
    /// A person acting without a session: the public start button on a room page.
    Anonymous,
    /// The orchestrator, carrying out a step.
    Orchestrator,
    /// A sweep, which is the orchestrator too but not on anyone's behalf.
    Reconcile,
}

impl Actor {
    pub fn as_sql(self) -> String {
        match self {
            Self::User(id) => format!("web:{id}"),
            Self::Anonymous => "web:anonymous".to_string(),
            Self::Orchestrator => "orchestrator".to_string(),
            Self::Reconcile => "reconcile".to_string(),
        }
    }

    /// From a session's optional user id, which is the shape every web handler has.
    pub fn web(user_id: Option<i64>) -> Self {
        match user_id {
            Some(id) => Self::User(id),
            None => Self::Anonymous,
        }
    }
}

pub async fn record(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    actor: Actor,
    kind: &str,
    detail: serde_json::Value,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO room_events (room_id, actor, kind, detail) VALUES ($1, $2, $3, $4)",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Text, _>(actor.as_sql())
    .bind::<Text, _>(kind)
    .bind::<Jsonb, _>(detail)
    .execute(conn)
    .await?;
    Ok(())
}

/// The most recent events, newest first.
pub async fn recent(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    limit: i64,
) -> Result<Vec<Event>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Timestamptz)]
        at: DateTime<Utc>,
        #[diesel(sql_type = Text)]
        actor: String,
        #[diesel(sql_type = Text)]
        kind: String,
        #[diesel(sql_type = Jsonb)]
        detail: serde_json::Value,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT at, actor, kind, detail FROM room_events
          WHERE room_id = $1 ORDER BY at DESC, id DESC LIMIT $2",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<BigInt, _>(limit)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Event {
            at: row.at,
            actor: row.actor,
            kind: row.kind,
            detail: row.detail,
        })
        .collect())
}

/// The latest event, which is what the room page shows while a room is coming up.
pub async fn latest(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Option<Event>, diesel::result::Error> {
    Ok(recent(conn, room, 1).await?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actors_are_spelled_one_way() {
        assert_eq!(Actor::User(1234567890).as_sql(), "web:1234567890");
        assert_eq!(Actor::Anonymous.as_sql(), "web:anonymous");
        assert_eq!(Actor::Orchestrator.as_sql(), "orchestrator");
        assert_eq!(Actor::Reconcile.as_sql(), "reconcile");

        // A person and a process are distinguishable by prefix without knowing the schema, which is
        // the whole reason this is a string rather than an id.
        assert!(Actor::web(Some(7)).as_sql().starts_with("web:"));
        assert!(Actor::web(None).as_sql().starts_with("web:"));
        assert!(!Actor::Orchestrator.as_sql().starts_with("web:"));
    }
}
