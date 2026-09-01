//! Per-room staff: the role ladder, membership, and delegation by invite link.
//!
//! ## One resolution path, no creator special case
//!
//! The uploader is simply the first `organizer` row. There is no "is this the creator" branch
//! anywhere, and `rooms.created_by` is informational only. That matters because a creator
//! special-case is a second authorization path that has to agree with the first forever, and the
//! day it stops agreeing is the day an organizer cannot remove someone who can still act.
//!
//! ## The ladder is `Ord`
//!
//! [`RoomRole`] derives `Ord` with `Helper < Organizer`, so every check in the codebase is
//! `role >= required` and adding a rung between them cannot silently invert a comparison. Copied
//! from `community-ap-tools`'s `review/db.rs`, which does the same thing for the same reason.
//!
//! A global admin session short-circuits to the top in the web tier, so an administrator never
//! needs a membership row, and never gets one implicitly either, which keeps "who is staff on
//! this room" an honest answer rather than a list that quietly omits the people who can act.
//!
//! ## The last organizer cannot be removed
//!
//! Enforced by a `BEFORE DELETE OR UPDATE` trigger in the migration, not here, because it spans
//! rows and because a room with no organizer has nobody who can repair it. [`remove`] and
//! [`set_role`] translate the trigger's `restrict_violation` into [`MemberError::LastOrganizer`]
//! rather than surfacing a raw database error: the constraint is a rule users hit, not a fault.

use diesel::sql_types::{BigInt, Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::ids::RoomId;

/// What someone may do in one room.
///
/// `Ord`, deliberately: `Helper < Organizer`, and every check is `>=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RoomRole {
    /// **Runs the room.** The whole console (hints, chat, countdowns, releases, collects, kicks,
    /// item sends) plus the roster of *players*: releasing a slot, handing out a fresh claim
    /// link, rotating one slot's password.
    Helper,
    /// Everything a helper may do, plus the three things a helper may not: whether the room runs
    /// at all (start, stop, close), how it is configured (the password mode, which is a restart),
    /// and **who is staff**: adding a member, demoting an organizer, minting an invite.
    ///
    /// The split is *the room versus the game inside it*. A helper is trusted with the multiworld
    /// and cannot change who is trusted with it.
    Organizer,
}

impl RoomRole {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Helper => "helper",
            Self::Organizer => "organizer",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "helper" => Some(Self::Helper),
            "organizer" => Some(Self::Organizer),
            _ => None,
        }
    }

    pub const ALL: [RoomRole; 2] = [Self::Helper, Self::Organizer];
}

#[derive(Debug, thiserror::Error)]
pub enum MemberError {
    /// The operation would leave the room with nobody who can administer it.
    ///
    /// A rule rather than a fault: the UI says so and offers to promote someone first.
    #[error("that would leave the room with no organizer; promote somebody else first")]
    LastOrganizer,

    /// The invite exists but may not be used: expired, or out of uses.
    #[error("this invite link is no longer valid")]
    InviteSpent,

    #[error("no such invite")]
    NoSuchInvite,

    #[error(transparent)]
    Db(#[from] diesel::result::Error),
}

/// Is this database error the last-organizer trigger firing?
///
/// Matched on the message text, which is not ideal and is deliberate: the trigger raises SQLSTATE
/// 23001 (`restrict_violation`), and diesel 2 has no `DatabaseErrorKind` variant for it, so it
/// arrives as `Unknown`, indistinguishable from every other unmapped database error. The message
/// is the only distinguishing feature there is.
///
/// What keeps that honest is the Postgres-backed test `removing_the_last_organizer_is_refused`,
/// which asserts the translation end to end against the real trigger. Reword the exception without
/// rewording this and the test fails, rather than the rule quietly degrading into a 500.
fn is_last_organizer(e: &diesel::result::Error) -> bool {
    const MARKER: &str = "would be left with no organizer";
    matches!(e, diesel::result::Error::DatabaseError(_, info) if info.message().contains(MARKER))
}

/// One member, as the roster page shows them.
#[derive(Debug, Clone)]
pub struct Member {
    pub user_id: i64,
    pub username: Option<String>,
    pub role: RoomRole,
    pub added_by: Option<i64>,
    pub added_at: chrono::DateTime<chrono::Utc>,
}

/// What role does this user hold in this room?
///
/// `None` means no membership row. Global admins are handled by the caller, not here: this
/// function answers "what does the roster say", and conflating that with "may this person act"
/// would make the roster page lie about who is actually on it.
pub async fn role_of(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    user_id: i64,
) -> Result<Option<RoomRole>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        role: String,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT role::text AS role FROM room_members WHERE room_id = $1 AND user_id = $2",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<BigInt, _>(user_id)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .next()
        .and_then(|r| RoomRole::parse(&r.role)))
}

/// Everyone on a room's roster, organizers first.
pub async fn list(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Vec<Member>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = BigInt)]
        user_id: i64,
        #[diesel(sql_type = Nullable<Text>)]
        username: Option<String>,
        #[diesel(sql_type = Text)]
        role: String,
        #[diesel(sql_type = Nullable<BigInt>)]
        added_by: Option<i64>,
        #[diesel(sql_type = Timestamptz)]
        added_at: chrono::DateTime<chrono::Utc>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT m.user_id, u.username, m.role::text AS role, m.added_by, m.added_at
           FROM room_members m
           LEFT JOIN users u ON u.id = m.user_id
          WHERE m.room_id = $1
          ORDER BY m.role DESC, m.added_at",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Member {
                user_id: row.user_id,
                username: row.username,
                role: RoomRole::parse(&row.role)?,
                added_by: row.added_by,
                added_at: row.added_at,
            })
        })
        .collect())
}

/// Add someone, or change the role they already hold.
///
/// The caller must have ensured a `users` row exists: membership is foreign-keyed, unlike the
/// creator allowlist, because a member is someone a room's pages will name.
pub async fn set_role(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    user_id: i64,
    role: RoomRole,
    added_by: Option<i64>,
) -> Result<(), MemberError> {
    let result = diesel::sql_query(
        "INSERT INTO room_members (room_id, user_id, role, added_by)
              VALUES ($1, $2, $3::room_role, $4)
         ON CONFLICT (room_id, user_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<BigInt, _>(user_id)
    .bind::<Text, _>(role.as_sql())
    .bind::<Nullable<BigInt>, _>(added_by)
    .execute(conn)
    .await;

    match result {
        Ok(_) => Ok(()),
        // Demoting the last organizer trips the same trigger as removing them.
        Err(e) if is_last_organizer(&e) => Err(MemberError::LastOrganizer),
        Err(e) => Err(MemberError::Db(e)),
    }
}

/// Remove someone from a room's roster.
///
/// Returns whether a row was actually removed, so a double-submitted form does not report success
/// twice over.
pub async fn remove(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    user_id: i64,
) -> Result<bool, MemberError> {
    let result = diesel::sql_query("DELETE FROM room_members WHERE room_id = $1 AND user_id = $2")
        .bind::<SqlUuid, _>(room)
        .bind::<BigInt, _>(user_id)
        .execute(conn)
        .await;

    match result {
        Ok(n) => Ok(n > 0),
        Err(e) if is_last_organizer(&e) => Err(MemberError::LastOrganizer),
        Err(e) => Err(MemberError::Db(e)),
    }
}

/// An outstanding invite link.
#[derive(Debug, Clone)]
pub struct Invite {
    pub token: String,
    pub room_id: RoomId,
    pub role: RoomRole,
    pub created_by: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub uses_remaining: Option<i32>,
}

/// How much of an invite token identifies a row, for a page that lists several.
///
/// Eight of thirty-two: enough that an organizer can tell two of their own links apart, and short
/// enough that the column is a label rather than a wall. Nothing resolves a prefix (it names a row
/// on a page and never travels) so this is a display length rather than a namespace, and it does
/// not have to grow with the number of invites a room has open.
pub const INVITE_PREFIX_CHARS: usize = 8;

impl Invite {
    /// The first [`INVITE_PREFIX_CHARS`] of the token.
    ///
    /// **Characters, not bytes**, and `take` rather than a slice: `url_token`'s alphabet is ASCII
    /// today, so the two agree, but a slice is what turns "somebody widened the alphabet" into a
    /// panic on a page rather than a shorter label, and the panic is in the render of an
    /// organizer's own page.
    ///
    /// The whole token still reaches the markup, in the link and in what the copy control carries.
    /// **This shortens what is READ, never what is sent**: a prefix somebody pasted to a helper
    /// would be an invitation that does not work, which is worse than a long one.
    pub fn prefix(&self) -> String {
        self.token.chars().take(INVITE_PREFIX_CHARS).collect()
    }
}

/// Mint an invite link.
///
/// The path that does not require knowing anyone's Discord snowflake, which is the normal case:
/// an organizer knows their helpers by name in a chat channel, not by id.
pub async fn create_invite(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    role: RoomRole,
    created_by: i64,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    uses: Option<i32>,
) -> Result<String, diesel::result::Error> {
    let token = crate::secret::url_token();
    diesel::sql_query(
        "INSERT INTO room_invites (token, room_id, role, created_by, expires_at, uses_remaining)
              VALUES ($1, $2, $3::room_role, $4, $5, $6)",
    )
    .bind::<Text, _>(&token)
    .bind::<SqlUuid, _>(room)
    .bind::<Text, _>(role.as_sql())
    .bind::<BigInt, _>(created_by)
    .bind::<Nullable<Timestamptz>, _>(expires_at)
    .bind::<Nullable<Integer>, _>(uses)
    .execute(conn)
    .await?;
    Ok(token)
}

/// Every outstanding invite for a room, so an organizer can see and revoke them.
pub async fn list_invites(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Vec<Invite>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        token: String,
        #[diesel(sql_type = SqlUuid)]
        room_id: RoomId,
        #[diesel(sql_type = Text)]
        role: String,
        #[diesel(sql_type = BigInt)]
        created_by: i64,
        #[diesel(sql_type = Timestamptz)]
        created_at: chrono::DateTime<chrono::Utc>,
        #[diesel(sql_type = Nullable<Timestamptz>)]
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
        #[diesel(sql_type = Nullable<Integer>)]
        uses_remaining: Option<i32>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT token, room_id, role::text AS role, created_by, created_at, expires_at,
                uses_remaining
           FROM room_invites WHERE room_id = $1 ORDER BY created_at DESC",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Invite {
                role: RoomRole::parse(&row.role)?,
                token: row.token,
                room_id: row.room_id,
                created_by: row.created_by,
                created_at: row.created_at,
                expires_at: row.expires_at,
                uses_remaining: row.uses_remaining,
            })
        })
        .collect())
}

pub async fn revoke_invite(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    token: &str,
) -> Result<bool, diesel::result::Error> {
    let removed = diesel::sql_query("DELETE FROM room_invites WHERE room_id = $1 AND token = $2")
        .bind::<SqlUuid, _>(room)
        .bind::<Text, _>(token)
        .execute(conn)
        .await?;
    Ok(removed > 0)
}

/// Redeem an invite: consume a use, then grant the membership.
///
/// The consuming `UPDATE` is the whole of the concurrency control. Two people following the last
/// use of a link race on one row, and the conditional `uses_remaining > 0` in the `WHERE` means
/// exactly one of them matches: no `SELECT` then `UPDATE`, and therefore no window between them.
///
/// **A redemption never lowers an existing role.** Someone who is already an organizer following a
/// helper link stays an organizer, because a link is an offer of access and not an instruction to
/// take some away.
/// What an invite is offering, without spending a use of it.
///
/// Same separation as [`crate::model::slot::ClaimOffer`] and for the same reason: `redeem_invite`
/// decrements `uses_remaining`, so describing a link must not go through it. An invite with a use
/// count would otherwise be one prefetch shorter than its organizer intended.
#[derive(Debug, Clone)]
pub struct InviteOffer {
    pub room_id: RoomId,
    pub room_name: String,
    pub role: RoomRole,
}

/// Look up an invite without spending it. `None` when it never existed, has expired, or is spent:
/// the three cases a landing page has nothing useful to say about and no reason to distinguish.
pub async fn offered_by_invite_token(
    conn: &mut AsyncPgConnection,
    token: &str,
) -> Result<Option<InviteOffer>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        room_id: RoomId,
        #[diesel(sql_type = Text)]
        room_name: String,
        #[diesel(sql_type = Text)]
        role: String,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT i.room_id, r.name AS room_name, i.role::text AS role
           FROM room_invites i
           JOIN rooms r ON r.id = i.room_id
          WHERE i.token = $1
            AND (i.expires_at IS NULL OR i.expires_at > now())
            AND (i.uses_remaining IS NULL OR i.uses_remaining > 0)",
    )
    .bind::<Text, _>(token)
    .load(conn)
    .await?;

    Ok(rows.into_iter().next().map(|row| InviteOffer {
        room_id: row.room_id,
        room_name: row.room_name,
        role: RoomRole::parse(&row.role).unwrap_or(RoomRole::Helper),
    }))
}

pub async fn redeem_invite(
    conn: &mut AsyncPgConnection,
    token: &str,
    user_id: i64,
) -> Result<(RoomId, RoomRole), MemberError> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        room_id: RoomId,
        #[diesel(sql_type = Text)]
        role: String,
    }

    let claimed: Vec<Row> = diesel::sql_query(
        "UPDATE room_invites
            SET uses_remaining = uses_remaining - 1
          WHERE token = $1
            AND (expires_at IS NULL OR expires_at > now())
            AND (uses_remaining IS NULL OR uses_remaining > 0)
      RETURNING room_id, role::text AS role",
    )
    .bind::<Text, _>(token)
    .load(conn)
    .await?;

    let Some(row) = claimed.into_iter().next() else {
        // Distinguish "never existed" from "spent", because they need different messages: one is
        // a mistyped link, the other is a link that worked for somebody else first.
        let exists: Vec<Row> = diesel::sql_query(
            "SELECT room_id, role::text AS role FROM room_invites WHERE token = $1",
        )
        .bind::<Text, _>(token)
        .load(conn)
        .await?;
        return Err(if exists.is_empty() {
            MemberError::NoSuchInvite
        } else {
            MemberError::InviteSpent
        });
    };

    let offered = RoomRole::parse(&row.role).ok_or(MemberError::NoSuchInvite)?;

    // GREATEST over the existing role, so redeeming never demotes. `room_role` is an ordered
    // Postgres enum, so the comparison is the same ladder Rust's `Ord` gives.
    diesel::sql_query(
        "INSERT INTO room_members (room_id, user_id, role, added_by)
              VALUES ($1, $2, $3::room_role, NULL)
         ON CONFLICT (room_id, user_id)
         DO UPDATE SET role = GREATEST(room_members.role, EXCLUDED.role)",
    )
    .bind::<SqlUuid, _>(row.room_id)
    .bind::<BigInt, _>(user_id)
    .bind::<Text, _>(offered.as_sql())
    .execute(conn)
    .await?;

    // Report what they now hold, which may be higher than the link offered.
    let held = role_of(conn, row.room_id, user_id)
        .await?
        .unwrap_or(offered);
    Ok((row.room_id, held))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole ladder, written as `held >= required`: the expression every guard uses.
    ///
    /// Exhaustive rather than sampled because it is four cases, and because the one that matters
    /// is the single `false`: a helper must not satisfy an organizer requirement.
    #[test]
    fn the_ladder_permits_exactly_what_it_should() {
        use RoomRole::{Helper, Organizer};
        for (held, required, permitted) in [
            (Helper, Helper, true),
            (Helper, Organizer, false),
            (Organizer, Helper, true),
            (Organizer, Organizer, true),
        ] {
            assert_eq!(
                held >= required,
                permitted,
                "{held:?} acting where {required:?} is required"
            );
        }
    }

    #[test]
    fn roles_round_trip_through_their_sql_spelling() {
        for role in RoomRole::ALL {
            assert_eq!(RoomRole::parse(role.as_sql()), Some(role));
        }
        assert_eq!(RoomRole::parse("admin"), None);
        assert_eq!(RoomRole::parse(""), None);
    }

    /// The prefix shortens what is READ. It never has to resolve, so the only two things that can
    /// go wrong are cutting the wrong amount and panicking instead of cutting.
    #[test]
    fn an_invite_prefix_shortens_without_ever_failing() {
        let invite = |token: &str| Invite {
            token: token.into(),
            room_id: RoomId::new(),
            role: RoomRole::Helper,
            created_by: 7,
            created_at: chrono::Utc::now(),
            expires_at: None,
            uses_remaining: None,
        };

        let real = crate::secret::url_token();
        assert_eq!(invite(&real).prefix().chars().count(), INVITE_PREFIX_CHARS);
        assert!(
            real.starts_with(&invite(&real).prefix()),
            "the prefix is not the start of the token it names"
        );

        // Shorter than the cut: the whole thing, not a panic. Nothing mints one this short today,
        // which is exactly why a slice here would go unnoticed until it did.
        assert_eq!(invite("abc").prefix(), "abc");
        assert_eq!(invite("").prefix(), "");

        // Characters, not bytes. `url_token`'s alphabet is ASCII, so this is unreachable through
        // the minting path, and it is the reason `take` is right rather than a slice, which would
        // panic mid-character here and take an organizer's own page down with it.
        assert_eq!(invite(&"é".repeat(12)).prefix(), "é".repeat(8));
    }
}
