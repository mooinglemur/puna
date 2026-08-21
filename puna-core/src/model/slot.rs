//! Per-slot identity, credential and claim.
//!
//! `room_slots` is **copied** from `generation_slots` at room creation rather than joined to it,
//! so a room is independent of later generation housekeeping -- and so two rooms on one generation
//! can have different owners, different passwords and different claim state, which they must,
//! because they are two independent multiworlds.
//!
//! ## One authorization function, used by every route
//!
//! [`may_access`] answers "may this person see this slot's patch and password", and it is the only
//! place that question is answered. A slot is visible to its `owner_id`, to anyone on the room's
//! roster, and to global admins. **Nobody else, including other claimed players in the same room**
//! -- a multiworld is not a shared trust boundary, and one player holding another's password would
//! let them connect as them.
//!
//! ## Claiming matters even without passwords
//!
//! With `slot_auth = 'none'` there is no password to protect, but a claim still gates the per-slot
//! patch download and is what puts a room on a player's landing page. So the claim link is issued
//! in every mode.

use diesel::sql_types::{BigInt, Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::artifact::SlotKind;
use crate::ids::{RoomId, TrackerId};
use crate::model::member::RoomRole;

#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    #[error("that claim link is not valid")]
    NoSuchToken,

    #[error(transparent)]
    Db(#[from] diesel::result::Error),
}

/// One slot of a room.
#[derive(Debug, Clone)]
pub struct Slot {
    pub room_id: RoomId,
    pub slot_number: i32,
    pub player_name: String,
    pub game: String,
    pub kind: SlotKind,
    /// `None` unless the room is in `per_slot` mode. Never rendered except to someone
    /// [`may_access`] has admitted.
    pub password: Option<String>,
    pub owner_id: Option<i64>,
    /// `None` once claimed. Present only to whoever holds the link.
    pub claim_token: Option<String>,
    pub claimed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub tracker_id: TrackerId,
}

impl Slot {
    /// A spectator plays nothing, so it has no patch and no checks to report.
    pub fn is_spectator(&self) -> bool {
        self.kind == SlotKind::Spectator
    }
}

#[derive(diesel::QueryableByName)]
struct SlotRow {
    #[diesel(sql_type = SqlUuid)]
    room_id: RoomId,
    #[diesel(sql_type = Integer)]
    slot_number: i32,
    #[diesel(sql_type = Text)]
    player_name: String,
    #[diesel(sql_type = Text)]
    game: String,
    #[diesel(sql_type = Text)]
    kind: String,
    #[diesel(sql_type = Nullable<Text>)]
    password: Option<String>,
    #[diesel(sql_type = Nullable<BigInt>)]
    owner_id: Option<i64>,
    #[diesel(sql_type = Nullable<Text>)]
    claim_token: Option<String>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    claimed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = SqlUuid)]
    tracker_id: TrackerId,
}

impl From<SlotRow> for Slot {
    fn from(row: SlotRow) -> Self {
        Self {
            room_id: row.room_id,
            slot_number: row.slot_number,
            player_name: row.player_name,
            game: row.game,
            // Conservative on an unknown value, as `generation::slots` is and for the same
            // reason: `Player` promises a patch and progress rather than assuming neither.
            kind: match row.kind.as_str() {
                "spectator" => SlotKind::Spectator,
                _ => SlotKind::Player,
            },
            password: row.password,
            owner_id: row.owner_id,
            claim_token: row.claim_token,
            claimed_at: row.claimed_at,
            tracker_id: row.tracker_id,
        }
    }
}

const SLOT_COLUMNS: &str = "room_id, slot_number, player_name, game, kind::text AS kind, \
                            password, owner_id, claim_token, claimed_at, tracker_id";

/// Every slot of a room, in slot order.
pub async fn list(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Vec<Slot>, diesel::result::Error> {
    let rows: Vec<SlotRow> = diesel::sql_query(format!(
        "SELECT {SLOT_COLUMNS} FROM room_slots WHERE room_id = $1 ORDER BY slot_number"
    ))
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;
    Ok(rows.into_iter().map(Slot::from).collect())
}

/// The Discord usernames of everyone holding a slot in this room.
///
/// A separate query rather than a join onto [`list`], deliberately: [`Slot`] is what the Secret
/// builder and the room page both take, and a display-only column on it would travel everywhere a
/// slot goes for the benefit of one table.
///
/// Placeholder names are returned as they are stored. [`crate::model::user::is_placeholder`] is
/// what tells them apart, and the caller decides what to render -- which is not this module's
/// business and is different in different places.
pub async fn owner_names(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<std::collections::HashMap<i64, String>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = BigInt)]
        id: i64,
        #[diesel(sql_type = Text)]
        username: String,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT DISTINCT u.id, u.username
           FROM room_slots s
           JOIN users u ON u.id = s.owner_id
          WHERE s.room_id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;

    Ok(rows.into_iter().map(|r| (r.id, r.username)).collect())
}

/// One slot by number.
pub async fn get(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    slot_number: i32,
) -> Result<Option<Slot>, diesel::result::Error> {
    let rows: Vec<SlotRow> = diesel::sql_query(format!(
        "SELECT {SLOT_COLUMNS} FROM room_slots WHERE room_id = $1 AND slot_number = $2"
    ))
    .bind::<SqlUuid, _>(room)
    .bind::<Integer, _>(slot_number)
    .load(conn)
    .await?;
    Ok(rows.into_iter().next().map(Slot::from))
}

/// The slots this user owns across every room, for their landing page.
pub async fn owned_by(
    conn: &mut AsyncPgConnection,
    user_id: i64,
) -> Result<Vec<Slot>, diesel::result::Error> {
    let rows: Vec<SlotRow> = diesel::sql_query(format!(
        "SELECT {SLOT_COLUMNS} FROM room_slots WHERE owner_id = $1 ORDER BY room_id, slot_number"
    ))
    .bind::<BigInt, _>(user_id)
    .load(conn)
    .await?;
    Ok(rows.into_iter().map(Slot::from).collect())
}

/// May this person see this slot's patch and password?
///
/// The single authorization rule for per-slot material, in one place so that a new route inherits
/// it by calling this rather than by remembering the policy. Note what is deliberately absent:
/// **holding another slot in the same room grants nothing.** A multiworld is a group of people who
/// can see each other's item traffic, not a group who share credentials.
pub fn may_access(
    slot: &Slot,
    user_id: Option<i64>,
    role: Option<RoomRole>,
    is_admin: bool,
) -> bool {
    if is_admin {
        return true;
    }
    // Any roster membership is enough: a helper who cannot see a patch cannot help with it, and
    // the roster is itself organizer-controlled.
    if role.is_some() {
        return true;
    }
    match (user_id, slot.owner_id) {
        (Some(user), Some(owner)) => user == owner,
        _ => false,
    }
}

/// Claim a slot from its single-use link.
///
/// One conditional `UPDATE`, so two people following the same link race on one row and exactly one
/// matches -- the same shape as invite redemption, and for the same reason. Nulling the token in
/// the `SET` is what makes it single-use: a second attempt finds no row with that token.
pub async fn claim(
    conn: &mut AsyncPgConnection,
    token: &str,
    user_id: i64,
) -> Result<Slot, ClaimError> {
    let rows: Vec<SlotRow> = diesel::sql_query(format!(
        "UPDATE room_slots
            SET owner_id = $2, claim_token = NULL, claimed_at = now()
          WHERE claim_token = $1
      RETURNING {SLOT_COLUMNS}"
    ))
    .bind::<Text, _>(token)
    .bind::<BigInt, _>(user_id)
    .load(conn)
    .await?;

    rows.into_iter()
        .next()
        .map(Slot::from)
        .ok_or(ClaimError::NoSuchToken)
}

/// Hand a slot back: clear its owner and issue a fresh claim link.
///
/// The token is regenerated rather than restored, because the old link may have been shared with
/// whoever is being replaced.
pub async fn release(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    slot_number: i32,
) -> Result<String, diesel::result::Error> {
    let token = crate::secret::url_token();
    diesel::sql_query(
        "UPDATE room_slots
            SET owner_id = NULL, claimed_at = NULL, claim_token = $3
          WHERE room_id = $1 AND slot_number = $2",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Integer, _>(slot_number)
    .bind::<Text, _>(&token)
    .execute(conn)
    .await?;
    Ok(token)
}

/// Rotate one slot's password. Only meaningful while the room is in `per_slot` mode.
pub async fn rotate_password(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    slot_number: i32,
) -> Result<String, diesel::result::Error> {
    let password = crate::secret::slot_password();
    diesel::sql_query(
        "UPDATE room_slots SET password = $3 WHERE room_id = $1 AND slot_number = $2",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Integer, _>(slot_number)
    .bind::<Text, _>(&password)
    .execute(conn)
    .await?;
    Ok(password)
}

/// The map that becomes `PAHOA_SLOT_PASSWORDS`.
///
/// **Complete or empty, never partial.** Under pahoa's fail-closed rule a slot missing from a
/// non-empty map is refused, so a partial map is a room some players cannot join -- and the caller
/// must render nothing at all rather than `{}`, which is a room *nobody* can join. Returning every
/// row rather than only the non-null ones is what makes the completeness check possible: the caller
/// can compare against `list` and refuse to build a Secret from a map with a hole in it.
pub async fn passwords(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Vec<(i32, Option<String>)>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Integer)]
        slot_number: i32,
        #[diesel(sql_type = Nullable<Text>)]
        password: Option<String>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT slot_number, password FROM room_slots WHERE room_id = $1 ORDER BY slot_number",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| (row.slot_number, row.password))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(owner: Option<i64>) -> Slot {
        Slot {
            room_id: RoomId::new(),
            slot_number: 1,
            player_name: "Troy".into(),
            game: "A Link to the Past".into(),
            kind: SlotKind::Player,
            password: Some("secret".into()),
            owner_id: owner,
            claim_token: None,
            claimed_at: None,
            tracker_id: TrackerId::new(),
        }
    }

    /// The whole rule, enumerated. The case that matters most is the last one: another player in
    /// the same room is a stranger as far as this slot's credentials are concerned.
    #[test]
    fn slot_access_admits_exactly_the_owner_the_roster_and_admins() {
        let mine = slot(Some(7));
        let unclaimed = slot(None);

        assert!(may_access(&mine, Some(7), None, false), "the owner");
        assert!(may_access(&mine, None, None, true), "a global admin");
        assert!(
            may_access(&mine, Some(9), Some(RoomRole::Helper), false),
            "a helper on the roster"
        );
        assert!(
            may_access(&mine, Some(9), Some(RoomRole::Organizer), false),
            "an organizer on the roster"
        );

        assert!(
            !may_access(&mine, Some(9), None, false),
            "another logged-in user who holds no role here"
        );
        assert!(!may_access(&mine, None, None, false), "anonymous");

        // An unclaimed slot has no owner, so ownership admits nobody -- the claim link is what
        // grants access to it, not the mere absence of a claimant.
        assert!(!may_access(&unclaimed, Some(7), None, false));
        assert!(may_access(
            &unclaimed,
            Some(7),
            Some(RoomRole::Helper),
            false
        ));
    }

    #[test]
    fn a_spectator_is_marked_as_one() {
        let mut s = slot(None);
        assert!(!s.is_spectator());
        s.kind = SlotKind::Spectator;
        assert!(s.is_spectator());
    }
}
