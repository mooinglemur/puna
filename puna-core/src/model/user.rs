//! Discord users.
//!
//! The Discord snowflake IS the primary key. There is no internal user id anywhere in Puna, which
//! means a room's membership, a slot's owner and an audit row all reference the same value a
//! human can paste into Discord to find out who it was.

use chrono::{DateTime, Utc};
use diesel::sql_types::{BigInt, Nullable, Text, Timestamptz};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

/// What an account may do.
///
/// **`Ord`, ascending by severity**, so a check reads `status >= Restricted` rather than listing
/// variants, the same shape as [`crate::model::member::RoomRole`], and for the same reason: a rung
/// added between two of these must not silently invert a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum UserStatus {
    /// Everything.
    #[default]
    Active,
    /// May play and may not create: no new rooms, no generation uploads. Withheld in exactly one
    /// place, `CanCreateRoom`, which is already the only door onto both.
    ///
    /// The point of the middle rung is that somebody who misused uploads is usually still mid-async
    /// in other people's games, and taking their slots away punishes those people too.
    Restricted,
    /// May not log in, and an existing session is refused on every authenticated request.
    ///
    /// **Nothing is deleted.** Their rooms, slots and memberships survive, because a ban is a
    /// statement about a person and not about the games other people are part-way through.
    Banned,
}

impl UserStatus {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Restricted => "restricted",
            Self::Banned => "banned",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "restricted" => Some(Self::Restricted),
            "banned" => Some(Self::Banned),
            _ => None,
        }
    }

    /// Whether this account may open rooms and upload generations.
    pub fn may_create(self) -> bool {
        self < Self::Restricted
    }

    /// Whether this account may make an authenticated request at all.
    pub fn may_act(self) -> bool {
        self < Self::Banned
    }

    pub const ALL: [UserStatus; 3] = [Self::Active, Self::Restricted, Self::Banned];
}

/// Record a user, or refresh their display name.
///
/// Called on every login, because Discord usernames change and a stale one in an audit log is
/// worse than useless: it names the wrong person.
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
/// owned by a row that exists, which means a person the lobby push assigns a slot to, before they
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

/// A Discord mention, `<@id>`, for pasting into a message.
///
/// **Built from the snowflake and never from a username**, which is the whole reason it is worth
/// offering: typing a handle into Discord does not reliably reach anybody, and it reaches nobody at
/// all for somebody who has never signed in here: the lobby-push case, where the id is all Puna
/// has. So this is defined for every owner, including one whose stored name is
/// [`placeholder_username`].
///
/// Two callers, which is why it is here rather than a `format!` at each: the tracker's owner column
/// and the room page's roster. One string, so they cannot spell a ping two ways.
///
/// **Deliberately not confusable with [`placeholder_username`]**, which is `<id>`: this is `<@id>`,
/// and the `@` is what makes Discord resolve it. They live beside each other so the difference is
/// read once rather than inferred later.
pub fn mention(discord_id: i64) -> String {
    format!("<@{discord_id}>")
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

/// One row of `/admin/users`.
///
/// `status_note` and `status_changed_by` come along because the table's job is answering *why* an
/// account is in the state it is in: a list of names and a colored tag with no reason attached is
/// a list somebody has to ask about in Discord.
#[derive(Debug, Clone, diesel::QueryableByName)]
pub struct AdminUser {
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    #[diesel(sql_type = Text)]
    pub username: String,
    #[diesel(sql_type = Timestamptz)]
    pub first_seen_at: DateTime<Utc>,
    #[diesel(sql_type = Timestamptz)]
    pub last_seen_at: DateTime<Utc>,
    #[diesel(sql_type = Text)]
    pub status: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub status_note: Option<String>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub status_changed_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = Nullable<Text>)]
    pub changed_by_name: Option<String>,
    /// How many rooms this account opened, and how many slots it holds right now.
    ///
    /// Not decoration: they are what turns "should I ban this account" into a question with an
    /// answer, by saying how many other people's games the decision touches.
    #[diesel(sql_type = BigInt)]
    pub rooms_created: i64,
    #[diesel(sql_type = BigInt)]
    pub slots_held: i64,
}

impl AdminUser {
    pub fn status(&self) -> UserStatus {
        // An unparseable value means the database knows a variant this build does not. Reading it
        // as `Active` would quietly hand back privileges somebody removed, so it reads as the
        // *most* restricted thing instead: wrong in the safe direction.
        UserStatus::parse(&self.status).unwrap_or(UserStatus::Banned)
    }
}

/// Everybody Puna has a row for, newest activity first.
///
/// No pagination, deliberately, and it is worth saying why given `/admin/rooms` needed some: one
/// row per person who has ever logged in is bounded by the size of a Discord community, where the
/// room list is bounded by how many games they have played. The table filters and sorts in the
/// browser, which is enough until this is thousands rather than hundreds.
pub async fn list(conn: &mut AsyncPgConnection) -> Result<Vec<AdminUser>, diesel::result::Error> {
    diesel::sql_query(
        "SELECT u.id, u.username, u.first_seen_at, u.last_seen_at, u.status::text AS status,
                u.status_note, u.status_changed_at, a.username AS changed_by_name,
                (SELECT count(*) FROM rooms r WHERE r.created_by = u.id) AS rooms_created,
                (SELECT count(*) FROM room_slots s WHERE s.owner_id = u.id) AS slots_held
           FROM users u
           LEFT JOIN users a ON a.id = u.status_changed_by
          ORDER BY u.last_seen_at DESC",
    )
    .load(conn)
    .await
}

/// This account's standing, for the request guards.
///
/// `None` means there is no row at all, which for a session-bearing request should not happen:
/// login upserts one. The caller decides what to make of it rather than being handed a default,
/// because "no such user" and "an ordinary user" are different answers and only one of them is
/// worth logging.
pub async fn status_of(
    conn: &mut AsyncPgConnection,
    discord_id: i64,
) -> Result<Option<(UserStatus, Option<String>)>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        status: String,
        #[diesel(sql_type = Nullable<Text>)]
        status_note: Option<String>,
    }

    let rows: Vec<Row> =
        diesel::sql_query("SELECT status::text AS status, status_note FROM users WHERE id = $1")
            .bind::<BigInt, _>(discord_id)
            .load(conn)
            .await?;

    Ok(rows.into_iter().next().map(|r| {
        (
            UserStatus::parse(&r.status).unwrap_or(UserStatus::Banned),
            r.status_note,
        )
    }))
}

/// Set an account's standing.
///
/// The note is cleared when returning somebody to `Active`: it explained a sanction that no longer
/// applies, and leaving it on the row would show a reason beside an account that is fine.
pub async fn set_status(
    conn: &mut AsyncPgConnection,
    discord_id: i64,
    status: UserStatus,
    note: Option<&str>,
    by: i64,
) -> Result<(), diesel::result::Error> {
    let note = match status {
        UserStatus::Active => None,
        _ => note.map(str::trim).filter(|n| !n.is_empty()),
    };

    diesel::sql_query(
        "UPDATE users
            SET status = $2::user_status, status_note = $3,
                status_changed_at = now(), status_changed_by = $4
          WHERE id = $1",
    )
    .bind::<BigInt, _>(discord_id)
    .bind::<Text, _>(status.as_sql())
    .bind::<Nullable<Text>, _>(note)
    .bind::<BigInt, _>(by)
    .execute(conn)
    .await?;
    Ok(())
}
