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
pub async fn ensure_exists(
    conn: &mut AsyncPgConnection,
    discord_id: i64,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO users (id, username) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
    )
    .bind::<BigInt, _>(discord_id)
    .bind::<Text, _>(format!("<{discord_id}>"))
    .execute(conn)
    .await?;
    Ok(())
}
