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
//!
//! ## A SLOT IS KEYED BY `(room_id, slot_number)`, AND ARCHIPELAGO'S REAL KEY IS `(team, slot)`
//!
//! **Decided 2026-08-24, and the day upstream grows teams this is the note to find.**
//!
//! Archipelago's data model is team-aware throughout — `(team, slot)` keys everything the server
//! owns, and `Connected` and `NetworkPlayer` both carry a team — but **nothing can generate a second
//! one**. Generation writes `{name: (0, player)}` unconditionally (`Main.py:337`), the server seeds
//! `self.clients = {0: {}}` and never grows it (`MultiServer.py:521`), and pahoa now **refuses at
//! load** a seed that names any other team rather than half-serving it. So team is provably 0 for
//! every slot that can exist today.
//!
//! pahoa asked Puna to key on the pair anyway, on the grounds that a caller assuming slot numbers
//! are unique is what would have to be found and fixed everywhere later, and that carrying a
//! constant costs nothing. **That is true on their side and not on ours**, which is why this is a
//! note rather than a column: a label on a metric is free, while here it is four primary keys —
//! `room_slots`, `generation_slots`, `room_slot_filters`, `generation_slot_locations` — plus every
//! query, route parameter and template that names a slot.
//!
//! **What to do if it ever changes.** The trigger is upstream Archipelago allowing generation to
//! produce a second team; pahoa refusing such a seed is what makes that visible rather than
//! silent, and `pahoa_*{team!="0"}` appearing in a scrape is the cheapest early warning. Then:
//! every table above takes a team column, `SlotKey` becomes a pair, and the surfaces that render a
//! slot number — the roster, the console, the filter editors, the tracker's slot views — have to
//! say which team they mean. Until then the assumption is **stated here rather than implied
//! everywhere**, which is the whole point of writing it down.
//!
//! Nothing breaks in the meantime: pahoa's status document now carries `team` on every slot row and
//! `crate::probe::http::parse` reads named fields and ignores the rest, so the extra field arrives
//! and is dropped. Its admin commands take an optional `team`, which Puna does not send.

use diesel::sql_types::{BigInt, Bool, Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
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
    /// Set when staff barred this slot from connecting, which is expressed by **omitting it from
    /// `PAHOA_SLOT_PASSWORDS`** — a map pahoa fails closed on, so a missing slot is refused.
    ///
    /// Independent of [`password`](Self::password), which is deliberately left in place: unlocking
    /// then restores the credential the holder already has rather than minting one somebody has to
    /// deliver. So a locked slot normally *has* a password and still cannot connect.
    pub locked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub locked_by: Option<i64>,
}

impl Slot {
    /// A spectator plays nothing, so it has no patch and no checks to report.
    pub fn is_spectator(&self) -> bool {
        self.kind == SlotKind::Spectator
    }

    /// Whether this slot is barred from connecting.
    ///
    /// A predicate rather than `locked_at.is_some()` at each call site, for the reason
    /// `DesiredState::is_at_rest` exists: the Secret builder, the planner's spec hash and the page
    /// all ask this question, and three spellings of it is how one of them ends up asking a
    /// slightly different one.
    pub fn is_locked(&self) -> bool {
        self.locked_at.is_some()
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
    #[diesel(sql_type = Nullable<Timestamptz>)]
    locked_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = Nullable<BigInt>)]
    locked_by: Option<i64>,
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
            locked_at: row.locked_at,
            locked_by: row.locked_by,
        }
    }
}

const SLOT_COLUMNS: &str = "room_id, slot_number, player_name, game, kind::text AS kind, \
                            password, owner_id, claim_token, claimed_at, tracker_id, \
                            locked_at, locked_by";

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

/// Does this person hold a slot in this room?
///
/// The **participant** half of the room page's two-tier rule: staff, or somebody playing here. It
/// answers the same question the room page already derives from a full `list`, and exists because
/// the lifecycle panel needs it on a path where loading every slot would be absurd — a 2000-slot
/// room's roster, fetched to decide whether to render one password.
///
/// One indexed lookup, on `room_slots_owner_idx`.
pub async fn owns_a_slot(
    conn: &mut AsyncPgConnection,
    room_id: RoomId,
    user_id: i64,
) -> Result<bool, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Bool)]
        present: bool,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM room_slots WHERE room_id = $1 AND owner_id = $2) AS present",
    )
    .bind::<SqlUuid, _>(room_id)
    .bind::<BigInt, _>(user_id)
    .load(conn)
    .await?;

    // `EXISTS` returns exactly one row, so an empty result is an impossibility rather than an
    // absence -- treat it as "not a participant", which is the closed direction.
    Ok(rows.into_iter().next().is_some_and(|row| row.present))
}

/// May this person see this slot's patch and password?
///
/// The single authorization rule for per-slot material, in one place so that a new route inherits
/// it by calling this rather than by remembering the policy. Note what is deliberately absent:
/// **holding another slot in the same room grants nothing.** A multiworld is a group of people who
/// can see each other's item traffic, not a group who share credentials.
/// Who may download a slot's **patch**, which is a wider question than [`may_access`].
///
/// **`PatchPolicy::Open` is the reference implementation's behavior**: archipelago.gg lists every
/// slot's patch on the room page and serves it to anyone holding the room's URL. Puna's own default
/// is narrower, and the narrowing is real friction — a player has to sign in and claim before they
/// can download the file they came for.
///
/// The two policies are one decision because they are the same argument in both directions. A
/// public patch must not carry a credential, and a patch that carries one must not be public — so
/// `Claimed` buys back the friction it costs by embedding the address *and* the password, and
/// `Open` trades that away for the reference's convenience.
///
/// **Deliberately not applied to a slot's password**, which stays on [`may_access`] under every
/// policy. `Open` is a statement about a game file, not about credentials, and the two routes that
/// take a slot guard must not be widened together.
pub fn may_download_patch(
    patch_policy: crate::model::room::PatchPolicy,
    slot: &Slot,
    user_id: Option<i64>,
    role: Option<RoomRole>,
    is_admin: bool,
) -> bool {
    match patch_policy {
        crate::model::room::PatchPolicy::Open => true,
        crate::model::room::PatchPolicy::Claimed => may_access(slot, user_id, role, is_admin),
    }
}

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
/// What a claim link is offering, without spending it.
///
/// **The read and the claim are separate operations, and that is not a convenience.** A claim
/// token is single-use and [`claim`] consumes it, so anything that wants to *describe* a link —
/// a landing page, and the chat client that unfurls it before a person has even clicked — must be
/// able to ask without redeeming. A page that redeemed on `GET` would be spent by the first
/// prefetch, and the recipient would arrive at a link that had already worked for somebody else.
#[derive(Debug, Clone)]
pub struct ClaimOffer {
    pub room_id: RoomId,
    pub room_name: String,
    pub slot_number: i32,
    pub player_name: String,
    pub game: String,
}

/// Look up a claim link without redeeming it. `None` for a token that never existed or is spent.
pub async fn offered_by_claim_token(
    conn: &mut AsyncPgConnection,
    token: &str,
) -> Result<Option<ClaimOffer>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        room_id: RoomId,
        #[diesel(sql_type = Text)]
        room_name: String,
        #[diesel(sql_type = Integer)]
        slot_number: i32,
        #[diesel(sql_type = Text)]
        player_name: String,
        #[diesel(sql_type = Text)]
        game: String,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT s.room_id, r.name AS room_name, s.slot_number, s.player_name, s.game
           FROM room_slots s
           JOIN rooms r ON r.id = s.room_id
          WHERE s.claim_token = $1",
    )
    .bind::<Text, _>(token)
    .load(conn)
    .await?;

    Ok(rows.into_iter().next().map(|row| ClaimOffer {
        room_id: row.room_id,
        room_name: row.room_name,
        slot_number: row.slot_number,
        player_name: row.player_name,
        game: row.game,
    }))
}

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

/// Bar one slot from connecting, or let it back in.
///
/// **Does not touch the password**, which is what makes unlocking free of any credential handling:
/// the value stays in the row, out of `PAHOA_SLOT_PASSWORDS` while the lock stands, and back in it
/// afterwards. The holder's password never changes and nobody has to be told anything.
///
/// Locking an already-locked slot keeps the **original** timestamp and actor rather than rewriting
/// history to whoever pressed last — the same rule `room::pin` follows, and for the same reason:
/// the useful question is who first decided this and when.
///
/// Returns whether the row moved, so a caller can tell a real change from a repeat and skip the
/// Secret rewrite and the audit row for a no-op.
pub async fn set_locked(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    slot_number: i32,
    locked: bool,
    by: i64,
) -> Result<bool, diesel::result::Error> {
    let changed = if locked {
        diesel::sql_query(
            "UPDATE room_slots SET locked_at = now(), locked_by = $3
              WHERE room_id = $1 AND slot_number = $2 AND locked_at IS NULL",
        )
        .bind::<SqlUuid, _>(room)
        .bind::<Integer, _>(slot_number)
        .bind::<BigInt, _>(by)
        .execute(conn)
        .await?
    } else {
        diesel::sql_query(
            "UPDATE room_slots SET locked_at = NULL, locked_by = NULL
              WHERE room_id = $1 AND slot_number = $2 AND locked_at IS NOT NULL",
        )
        .bind::<SqlUuid, _>(room)
        .bind::<Integer, _>(slot_number)
        .execute(conn)
        .await?
    };

    Ok(changed > 0)
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

/// Claim slots on behalf of the accounts a lobby room named.
///
/// **Guarded on `owner_id IS NULL`, per row, inside the statement** — not filtered by the caller.
/// The caller's plan is computed from a roster it read a moment ago and from a lobby answer that is
/// older still, so a player who used their claim link in between must win. Deciding it here means
/// the check and the write are one operation rather than two with a race between them.
///
/// The claim token is cleared as [`claim`] clears it: the link has done its job, and leaving it live
/// would let whoever it was sent to take a slot that now belongs to somebody.
///
/// Returns how many slots were actually claimed, which is **not** necessarily `assignments.len()` —
/// the difference is exactly the slots somebody else took first, and the caller reports it.
pub async fn claim_for_owners(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    assignments: &[(i32, i64)],
) -> Result<usize, diesel::result::Error> {
    let mut claimed = 0;

    for (slot_number, owner_id) in assignments {
        claimed += diesel::sql_query(
            "UPDATE room_slots
                SET owner_id = $3, claim_token = NULL, claimed_at = now()
              WHERE room_id = $1 AND slot_number = $2 AND owner_id IS NULL",
        )
        .bind::<SqlUuid, _>(room)
        .bind::<Integer, _>(*slot_number)
        .bind::<BigInt, _>(*owner_id)
        .execute(conn)
        .await?;
    }

    Ok(claimed)
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
            locked_at: None,
            locked_by: None,
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
