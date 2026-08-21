//! Discord users.
//!
//! The Discord snowflake IS the primary key. There is no internal user id anywhere in Puna, which
//! means a room's membership, a slot's owner and an audit row all reference the same value a
//! human can paste into Discord to find out who it was.

use diesel::sql_types::{BigInt, Text};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

/// Record a user, or refresh their display name.
///
/// Called on every login, because Discord usernames change and a stale one in an audit log is
/// worse than useless -- it names the wrong person.
pub async fn upsert(
    conn: &mut AsyncPgConnection,
    discord_id: i64,
    username: &str,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO users (id, username) VALUES ($1, $2)
         ON CONFLICT (id) DO UPDATE
            SET username = EXCLUDED.username, last_seen_at = now()",
    )
    .bind::<BigInt, _>(discord_id)
    .bind::<Text, _>(username)
    .execute(conn)
    .await?;
    Ok(())
}

/// Ensure a row exists for a user we have only ever seen by id.
///
/// Membership and slot ownership are foreign-keyed to `users`, so someone can be granted a role
/// before they have ever logged in. The username is a placeholder until they do.
/// The stand-in username for somebody who has a row but has never logged in.
///
/// `users.username` is `NOT NULL` and `room_slots.owner_id` references it, so a slot can only be
/// owned by a row that exists -- which means a person the lobby push assigns a slot to, before they
/// have ever signed in here, needs *something* in the column. This is that something.
///
/// It sits beside [`ensure_exists`], which writes it, and [`is_placeholder`], which reads it back.
/// Three copies of one format string was how this would have gone wrong.
pub fn placeholder_username(discord_id: i64) -> String {
    format!("<{discord_id}>")
}

/// Whether this username is the stand-in rather than a real one.
///
/// Callers render "never logged in" instead, because showing `<493204...>` puts a Discord ID in
/// front of people who have no use for one, and reads as a bug.
///
/// Matched on the SHAPE rather than by re-deriving the string from an id, so a caller holding only
/// a name can ask. Discord usernames cannot contain `<` or `>`, which is what makes the shape
/// unambiguous.
pub fn is_placeholder(username: &str) -> bool {
    username.starts_with('<') && username.ends_with('>')
}

pub async fn ensure_exists(
    conn: &mut AsyncPgConnection,
    discord_id: i64,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO users (id, username) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
    )
    .bind::<BigInt, _>(discord_id)
    .bind::<Text, _>(placeholder_username(discord_id))
    .execute(conn)
    .await?;
    Ok(())
}
