//! Feature gates: who may create a room, and from which source.
//!
//! Both gates ship `disabled` and **admins bypass every gate**, so a fresh deployment is
//! admin-only with no further configuration, which is exactly the posture the first rounds of
//! testing want. Opening up is a deliberate `settings` change through `/admin/gates`, recorded
//! with `updated_by`.
//!
//! ## Why the policy lives here and not in the request guard
//!
//! Two callers need the same answer and only one of them has a session. The upload form reaches
//! this through `CanCreateRoom` in the web tier; the lobby push (M14) authenticates with
//! `X-Api-Key` and takes the acting user from its manifest, so it cannot pass through a
//! cookie-shaped guard and must call [`evaluate`] directly. Putting the decision in the guard
//! would mean writing it twice, and the second copy is the one that drifts.
//!
//! It also puts the interesting half where it can be tested against a real database rather than
//! against a mocked request.
//!
//! ## Everything here fails closed
//!
//! A missing `settings` row, or a `mode` this build does not recognize, resolves to
//! [`GateMode::Disabled`] with a loud warning, never to `Open`. A gate is a thing that
//! *permits*, so the absence of one must permit nothing. The alternative, treating an
//! unreadable gate as absent and therefore open, turns a deleted row or a botched migration into
//! open room creation, which is the failure nobody would notice until it mattered.

use diesel::sql_types::{BigInt, Bool, Nullable, Text, Timestamptz};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::model::RoomSource;

/// How open a creation gate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// Nobody but an admin.
    Disabled,
    /// Admins, plus anyone in `creator_allowlist`.
    Allowlist,
    /// Any logged-in user.
    Open,
}

impl GateMode {
    /// The value as the `gate_mode` enum spells it.
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Allowlist => "allowlist",
            Self::Open => "open",
        }
    }

    /// Parse what the database returned. `None` for anything unrecognized, which the caller
    /// turns into `Disabled` rather than guessing.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "disabled" => Some(Self::Disabled),
            "allowlist" => Some(Self::Allowlist),
            "open" => Some(Self::Open),
            _ => None,
        }
    }

    /// Every mode, for rendering the admin form and for exhaustive tests.
    pub const ALL: [GateMode; 3] = [Self::Disabled, Self::Allowlist, Self::Open];
}

/// Which rule admitted a caller.
///
/// Carried rather than discarded because it is what an audit row should record: "created while
/// the gate was open" and "created by an admin over a disabled gate" are very different facts
/// about the same room, and only one of them stays true if the gate is later closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Admin,
    Allowlisted,
    Open,
}

/// Which rule refused a caller. Chooses the message the user sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The gate is closed to everyone but admins.
    Disabled,
    /// The gate is open to an allowlist this caller is not on.
    NotAllowlisted,
}

impl Refusal {
    /// What to tell the caller.
    ///
    /// Deliberately does not distinguish "closed" from "you specifically are not on the list" any
    /// more than it must: both say who to ask, neither invites a retry.
    ///
    /// **A whole sentence, and it names both actions**, because this is what a page renders where
    /// a control would otherwise be, not only what a `403` carries into the log. One gate governs
    /// opening a room and uploading a generation, so a message naming one of them would be read on
    /// the page for the other as an answer to a different question.
    pub fn message(self) -> &'static str {
        match self {
            Self::Disabled => {
                "Opening rooms and uploading generations are turned off here. Ask an administrator \
                 to enable them."
            }
            Self::NotAllowlisted => {
                "Opening rooms and uploading generations are limited to approved accounts. Ask an \
                 administrator for access."
            }
        }
    }
}

/// The outcome of a gate check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed(Grant),
    Refused(Refusal),
}

impl Decision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed(_))
    }

    pub fn grant(self) -> Option<Grant> {
        match self {
            Self::Allowed(grant) => Some(grant),
            Self::Refused(_) => None,
        }
    }
}

/// May this caller create a room from this source?
///
/// **Admins short-circuit before any query runs.** That is not just an optimization: it means an
/// administrator can still act when the `settings` row is missing or unreadable, which is the
/// state they would be logging in to repair.
pub async fn evaluate(
    conn: &mut AsyncPgConnection,
    source: RoomSource,
    user_id: i64,
    is_admin: bool,
) -> Result<Decision, diesel::result::Error> {
    if is_admin {
        return Ok(Decision::Allowed(Grant::Admin));
    }

    match mode(conn, source.settings_key()).await? {
        GateMode::Disabled => Ok(Decision::Refused(Refusal::Disabled)),
        GateMode::Open => Ok(Decision::Allowed(Grant::Open)),
        GateMode::Allowlist => {
            if is_allowlisted(conn, user_id).await? {
                Ok(Decision::Allowed(Grant::Allowlisted))
            } else {
                Ok(Decision::Refused(Refusal::NotAllowlisted))
            }
        }
    }
}

/// Read one gate. Missing or unrecognized resolves to [`GateMode::Disabled`], loudly.
pub async fn mode(
    conn: &mut AsyncPgConnection,
    key: &str,
) -> Result<GateMode, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        mode: String,
    }

    // Cast to text rather than mapping `gate_mode` into diesel: the enum's values are a closed
    // set this module already owns, and one cast is cheaper than the derive plumbing.
    let rows: Vec<Row> =
        diesel::sql_query("SELECT mode::text AS mode FROM settings WHERE key = $1")
            .bind::<Text, _>(key)
            .load(conn)
            .await?;

    let Some(row) = rows.into_iter().next() else {
        tracing::warn!(
            key,
            "no settings row for this gate; treating it as disabled. The migration seeds both \
             gates, so a missing row means one was deleted. Repair it from /admin/gates."
        );
        return Ok(GateMode::Disabled);
    };

    match GateMode::parse(&row.mode) {
        Some(mode) => Ok(mode),
        None => {
            tracing::warn!(
                key,
                mode = %row.mode,
                "unrecognized gate mode; treating it as disabled. This build is older than the \
                 database."
            );
            Ok(GateMode::Disabled)
        }
    }
}

/// Set one gate, recording who did it.
///
/// An upsert rather than an update, so a row deleted by hand can be repaired from the admin page
/// instead of requiring psql.
pub async fn set_mode(
    conn: &mut AsyncPgConnection,
    key: &str,
    mode: GateMode,
    updated_by: i64,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO settings (key, mode, updated_by, updated_at)
              VALUES ($1, $2::gate_mode, $3, now())
         ON CONFLICT (key) DO UPDATE
            SET mode = EXCLUDED.mode, updated_by = EXCLUDED.updated_by, updated_at = now()",
    )
    .bind::<Text, _>(key)
    .bind::<Text, _>(mode.as_sql())
    .bind::<BigInt, _>(updated_by)
    .execute(conn)
    .await?;
    Ok(())
}

/// Every gate and its current mode, for `/admin/gates`.
pub async fn all(conn: &mut AsyncPgConnection) -> Result<Vec<Gate>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        key: String,
        #[diesel(sql_type = Text)]
        mode: String,
        #[diesel(sql_type = Timestamptz)]
        updated_at: chrono::DateTime<chrono::Utc>,
        #[diesel(sql_type = Nullable<BigInt>)]
        updated_by: Option<i64>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT key, mode::text AS mode, updated_at, updated_by FROM settings ORDER BY key",
    )
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Gate {
            mode: GateMode::parse(&row.mode).unwrap_or(GateMode::Disabled),
            key: row.key,
            updated_at: row.updated_at,
            updated_by: row.updated_by,
        })
        .collect())
}

/// One gate as the admin page shows it.
#[derive(Debug, Clone)]
pub struct Gate {
    pub key: String,
    pub mode: GateMode,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub updated_by: Option<i64>,
}

/// Is this user on the creator allowlist?
pub async fn is_allowlisted(
    conn: &mut AsyncPgConnection,
    user_id: i64,
) -> Result<bool, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Bool)]
        present: bool,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM creator_allowlist WHERE user_id = $1) AS present",
    )
    .bind::<BigInt, _>(user_id)
    .load(conn)
    .await?;

    // `EXISTS` always returns exactly one row, so an empty result is an impossibility rather than
    // an absence: treat it as not allowlisted, which is the closed direction.
    Ok(rows.into_iter().next().is_some_and(|row| row.present))
}

/// Add someone to the creator allowlist.
///
/// Note there is deliberately no foreign key from `creator_allowlist.user_id` to `users`: an
/// administrator must be able to authorize a Discord id before its owner has ever logged in,
/// which is the normal case when access is arranged in a Discord channel first.
pub async fn allow(
    conn: &mut AsyncPgConnection,
    user_id: i64,
    note: Option<&str>,
    added_by: i64,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO creator_allowlist (user_id, note, added_by) VALUES ($1, $2, $3)
         ON CONFLICT (user_id) DO UPDATE SET note = EXCLUDED.note",
    )
    .bind::<BigInt, _>(user_id)
    .bind::<Nullable<Text>, _>(note)
    .bind::<BigInt, _>(added_by)
    .execute(conn)
    .await?;
    Ok(())
}

/// Remove someone from the creator allowlist. Rooms they already created are unaffected.
pub async fn revoke(
    conn: &mut AsyncPgConnection,
    user_id: i64,
) -> Result<bool, diesel::result::Error> {
    let removed = diesel::sql_query("DELETE FROM creator_allowlist WHERE user_id = $1")
        .bind::<BigInt, _>(user_id)
        .execute(conn)
        .await?;
    Ok(removed > 0)
}

/// The whole allowlist, newest first, for `/admin/gates`.
pub async fn allowlist(
    conn: &mut AsyncPgConnection,
) -> Result<Vec<AllowlistEntry>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = BigInt)]
        user_id: i64,
        #[diesel(sql_type = Nullable<Text>)]
        note: Option<String>,
        #[diesel(sql_type = Nullable<BigInt>)]
        added_by: Option<i64>,
        #[diesel(sql_type = Timestamptz)]
        added_at: chrono::DateTime<chrono::Utc>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT user_id, note, added_by, added_at FROM creator_allowlist ORDER BY added_at DESC",
    )
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| AllowlistEntry {
            user_id: row.user_id,
            note: row.note,
            added_by: row.added_by,
            added_at: row.added_at,
        })
        .collect())
}

/// One allowlist row as the admin page shows it.
#[derive(Debug, Clone)]
pub struct AllowlistEntry {
    pub user_id: i64,
    pub note: Option<String>,
    pub added_by: Option<i64>,
    pub added_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_modes_round_trip_through_their_sql_spelling() {
        for mode in GateMode::ALL {
            assert_eq!(GateMode::parse(mode.as_sql()), Some(mode), "{mode:?}");
        }
    }

    /// The failure this guards is silent: an unrecognized value resolving to `Open` would be a
    /// gate that stops gating, which nothing surfaces until someone unexpected creates a room.
    #[test]
    fn an_unknown_gate_mode_does_not_parse() {
        for raw in ["", "OPEN", "enabled", "allow", "disabled "] {
            assert_eq!(GateMode::parse(raw), None, "{raw:?}");
        }
    }

    /// The keys must match the rows the initial migration seeds, or every gate reads as missing
    /// and the whole system is admin-only with a warning nobody reads.
    #[test]
    fn settings_keys_match_the_seeded_rows() {
        assert_eq!(RoomSource::Direct.settings_key(), "room_creation.direct");
        assert_eq!(RoomSource::Lobby.settings_key(), "room_creation.lobby");
    }
}
