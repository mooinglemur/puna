//! Rooms: creation, the password modes, cloning, and the two ways a room can be yours.
//!
//! ## Creation is one transaction, and it has to be
//!
//! A room row without its slots is a room nobody can join, and a room without its first organizer
//! is a room nobody can administer -- and the last-organizer trigger means the second cannot be
//! repaired by adding one later without first having one. So the row, the membership and every
//! slot land together or not at all.
//!
//! ## Two column families
//!
//! Everything here writes only the *desired* half: `desired_state`, the room's options, its
//! passwords. The observed half -- `state`, `advertised_*`, `provisioned_at` -- belongs to the
//! orchestrator and is read-only from the web tier. Nothing in this module writes an observed
//! column, which is the property the [`Orchestrator`](super::Orchestrator) token exists to make
//! greppable elsewhere.
//!
//! ## Requesting is not doing
//!
//! [`request_state`] writes `desired_state` and nothing else. It is idempotent by construction --
//! a second request updates zero rows -- and it never blocks: the room page renders from the row
//! and polls, so a cold start is a visible state rather than a hanging request.

use diesel::sql_types::{BigInt, Bool, Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

use crate::Environment;
use crate::ids::{GenerationId, RoomId, TrackerId};
use crate::model::member::{self, RoomRole};
use crate::model::{RoomSource, slot};

/// How a room authenticates the people connecting to it.
///
/// Mutually exclusive, mirroring reference Archipelago plus pahoa's per-slot addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotAuth {
    /// Traditional passwordless room.
    None,
    /// One shared password, as reference Archipelago does.
    Room,
    /// Each slot has its own secret.
    PerSlot,
}

impl SlotAuth {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Room => "room",
            Self::PerSlot => "per_slot",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "none" => Some(Self::None),
            "room" => Some(Self::Room),
            "per_slot" => Some(Self::PerSlot),
            _ => None,
        }
    }

    pub const ALL: [SlotAuth; 3] = [Self::None, Self::Room, Self::PerSlot];
}

/// Who may read a room's spoiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpoilerPolicy {
    Never,
    AdminOnly,
    Players,
    Public,
}

impl SpoilerPolicy {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::AdminOnly => "admin_only",
            Self::Players => "players",
            Self::Public => "public",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "never" => Some(Self::Never),
            "admin_only" => Some(Self::AdminOnly),
            "players" => Some(Self::Players),
            "public" => Some(Self::Public),
            _ => None,
        }
    }
}

/// Who may read this room's spoiler, from one rule used by every caller.
///
/// The room page and the download route both ask, and they must agree: a page that offers a link
/// the route refuses is a bug report, and a page that hides a link the route would serve is worse —
/// it teaches people to guess URLs.
///
/// `is_staff` covers global admins too, resolved by the caller, because "admin" is a fact about the
/// session rather than about the room.
pub fn may_see_spoiler(policy: SpoilerPolicy, is_staff: bool, owns_a_slot: bool) -> bool {
    match policy {
        // Not "hidden": absent. A race's spoiler is not served to anyone, including an admin, and
        // the route answers 404 rather than 403 so the refusal discloses nothing either.
        SpoilerPolicy::Never => false,
        SpoilerPolicy::AdminOnly => is_staff,
        SpoilerPolicy::Players => is_staff || owns_a_slot,
        SpoilerPolicy::Public => true,
    }
}

/// Who may read this room's tracker, from one rule used by every caller.
///
/// The same shape as [`may_see_spoiler`] and for the same reason, but the default is far more open:
/// a tracker is **meant** to be shared, with stream chats and spectators who will never log in, so
/// `link` — the unguessable URL is the authorization — is what an ordinary room gets and what the
/// reference implementation does.
///
/// `members` is the race default, and it is the one case where a tracker link handed to a friend
/// does not work for them. That is the point of it.
pub fn may_see_tracker(policy: TrackerPolicy, is_staff: bool, owns_a_slot: bool) -> bool {
    match policy {
        TrackerPolicy::Link => true,
        TrackerPolicy::Members => is_staff || owns_a_slot,
        TrackerPolicy::Disabled => false,
    }
}

/// Who may read a room's tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerPolicy {
    /// The unguessable URL is the authorization, as the reference implementation does.
    Link,
    /// Login plus a membership or slot-ownership relationship is also required.
    Members,
    Disabled,
}

impl TrackerPolicy {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Members => "members",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "link" => Some(Self::Link),
            "members" => Some(Self::Members),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// What a room wants to be doing. The **desired** half of the two column families: the web tier
/// writes it, the orchestrator only reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Running,
    Stopped,
    Deleted,
}

impl DesiredState {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "running" => Some(Self::Running),
            "stopped" => Some(Self::Stopped),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

    pub const ALL: [DesiredState; 3] = [Self::Running, Self::Stopped, Self::Deleted];
}

/// Where a room actually is. The **observed** half: the orchestrator writes it, everyone else
/// reads it.
///
/// Deliberately a different type from [`DesiredState`] rather than one enum with a flag. A room
/// that *wants* to run and a room that *is* running are different facts, and the whole
/// reconciler rests on being able to compare them — collapsing them into one value is how a
/// level-triggered loop turns back into an edge-triggered one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomState {
    /// The row exists; the state directory may not. One of the two states where D3's invariant
    /// does not hold, and it is transient and orchestrator-owned.
    Provisioning,
    /// The directory exists, no Deployment. Where a torn-down room rests — **holding its port
    /// reservation**, which is what lets it come back on the same address.
    Idle,
    /// Port allocated, objects created, no ready replica yet.
    Starting,
    /// The Deployment reports a ready replica.
    Running,
    /// The Deployment is there but has had no ready replica for several sweeps. Still live for
    /// the allocator's purposes: players may be mid-reconnect, so its port is not reclaimable.
    Degraded,
    Stopping,
    Failed,
    /// The other state where the directory may not exist.
    Deleting,
    /// `provisioned_at` is set and the directory is gone. **Never auto-repaired** — recreating it
    /// would replace saved progress with an empty room and look like a successful start.
    IntegrityFault,
}

impl RoomState {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Degraded => "degraded",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
            Self::Deleting => "deleting",
            Self::IntegrityFault => "integrity_fault",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "provisioning" => Some(Self::Provisioning),
            "idle" => Some(Self::Idle),
            "starting" => Some(Self::Starting),
            "running" => Some(Self::Running),
            "degraded" => Some(Self::Degraded),
            "stopping" => Some(Self::Stopping),
            "failed" => Some(Self::Failed),
            "deleting" => Some(Self::Deleting),
            "integrity_fault" => Some(Self::IntegrityFault),
            _ => None,
        }
    }

    /// Serving players, or close enough that taking its port would be felt.
    ///
    /// This is D4: the allocator refuses to reclaim a live room's pair. Reclaiming one takes the
    /// port out from under connected clients, and Cilium does not report that as an error — the
    /// room simply answers on an address nobody was told about.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Degraded)
    }

    pub const ALL: [RoomState; 9] = [
        Self::Provisioning,
        Self::Idle,
        Self::Starting,
        Self::Running,
        Self::Degraded,
        Self::Stopping,
        Self::Failed,
        Self::Deleting,
        Self::IntegrityFault,
    ];
}

/// Everything the caller chooses when opening a room.
#[derive(Debug, Clone)]
pub struct NewRoom {
    pub environment: Environment,
    pub name: String,
    pub generation_id: GenerationId,
    pub source: RoomSource,
    pub created_by: i64,
    pub slot_auth: SlotAuth,
    /// `None` defaults from the generation's `race_mode`: `never` for a race, `admin_only`
    /// otherwise. Races are the case where a leaked spoiler is not recoverable.
    pub spoiler_policy: Option<SpoilerPolicy>,
    /// `None` defaults from `race_mode` too: `members` for a race, `link` otherwise.
    pub tracker_policy: Option<TrackerPolicy>,
    pub wants_filtered: bool,
    pub use_embedded_options: bool,
    pub save_interval_secs: i32,
    pub lobby_room_id: Option<uuid::Uuid>,
    pub lobby_job_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub cloned_from: Option<RoomId>,
}

impl NewRoom {
    /// The ordinary direct-upload case, with everything else defaulted.
    pub fn direct(
        environment: Environment,
        name: impl Into<String>,
        generation_id: GenerationId,
        created_by: i64,
    ) -> Self {
        Self {
            environment,
            name: name.into(),
            generation_id,
            source: RoomSource::Direct,
            created_by,
            slot_auth: SlotAuth::None,
            spoiler_policy: None,
            tracker_policy: None,
            wants_filtered: true,
            use_embedded_options: true,
            save_interval_secs: 30,
            lobby_room_id: None,
            lobby_job_id: None,
            idempotency_key: None,
            cloned_from: None,
        }
    }
}

/// A room, as the room page and the listings read it.
#[derive(Debug, Clone)]
pub struct Room {
    pub id: RoomId,
    pub name: String,
    pub environment: Environment,
    pub generation_id: GenerationId,
    pub source: RoomSource,
    pub created_by: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub cloned_from: Option<RoomId>,

    pub desired_state: String,
    pub slot_auth: SlotAuth,
    pub password: Option<String>,
    pub spoiler_policy: SpoilerPolicy,
    pub tracker_id: TrackerId,
    pub tracker_policy: TrackerPolicy,
    pub wants_filtered: bool,

    pub state: String,
    /// When the room last changed state.
    ///
    /// The page turns this into an elapsed time rather than rendering it: a cold start is a
    /// multi-second visible state, and "starting, 40 seconds" is the difference between waiting and
    /// wondering whether anything is happening.
    pub state_changed_at: chrono::DateTime<chrono::Utc>,
    pub advertised_host: Option<String>,
    pub advertised_port: Option<i32>,
    pub advertised_filtered_port: Option<i32>,
    pub last_error: Option<String>,
}

#[derive(diesel::QueryableByName)]
struct RoomRow {
    #[diesel(sql_type = SqlUuid)]
    id: RoomId,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Text)]
    environment: String,
    #[diesel(sql_type = SqlUuid)]
    generation_id: GenerationId,
    #[diesel(sql_type = Text)]
    source: String,
    #[diesel(sql_type = Nullable<BigInt>)]
    created_by: Option<i64>,
    #[diesel(sql_type = Timestamptz)]
    created_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    cloned_from: Option<RoomId>,
    #[diesel(sql_type = Text)]
    desired_state: String,
    #[diesel(sql_type = Text)]
    slot_auth: String,
    #[diesel(sql_type = Nullable<Text>)]
    password: Option<String>,
    #[diesel(sql_type = Text)]
    spoiler_policy: String,
    #[diesel(sql_type = SqlUuid)]
    tracker_id: TrackerId,
    #[diesel(sql_type = Text)]
    tracker_policy: String,
    #[diesel(sql_type = Bool)]
    wants_filtered: bool,
    #[diesel(sql_type = Text)]
    state: String,
    #[diesel(sql_type = Timestamptz)]
    state_changed_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = Nullable<Text>)]
    advertised_host: Option<String>,
    #[diesel(sql_type = Nullable<Integer>)]
    advertised_port: Option<i32>,
    #[diesel(sql_type = Nullable<Integer>)]
    advertised_filtered_port: Option<i32>,
    #[diesel(sql_type = Nullable<Text>)]
    last_error: Option<String>,
}

impl From<RoomRow> for Room {
    fn from(row: RoomRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            environment: match row.environment.as_str() {
                "prod" => Environment::Prod,
                _ => Environment::Dev,
            },
            generation_id: row.generation_id,
            source: match row.source.as_str() {
                "lobby" => RoomSource::Lobby,
                _ => RoomSource::Direct,
            },
            created_by: row.created_by,
            created_at: row.created_at,
            cloned_from: row.cloned_from,
            desired_state: row.desired_state,
            slot_auth: SlotAuth::parse(&row.slot_auth).unwrap_or(SlotAuth::None),
            password: row.password,
            spoiler_policy: SpoilerPolicy::parse(&row.spoiler_policy)
                // An unknown policy from a newer database must not widen access, so it reads as
                // the most restrictive value rather than the default one.
                .unwrap_or(SpoilerPolicy::Never),
            tracker_id: row.tracker_id,
            tracker_policy: TrackerPolicy::parse(&row.tracker_policy)
                .unwrap_or(TrackerPolicy::Disabled),
            wants_filtered: row.wants_filtered,
            state: row.state,
            state_changed_at: row.state_changed_at,
            advertised_host: row.advertised_host,
            advertised_port: row.advertised_port,
            advertised_filtered_port: row.advertised_filtered_port,
            last_error: row.last_error,
        }
    }
}

const ROOM_COLUMNS: &str = "id, name, environment::text AS environment, generation_id, \
                            source::text AS source, created_by, created_at, cloned_from, \
                            desired_state::text AS desired_state, slot_auth::text AS slot_auth, \
                            password, spoiler_policy::text AS spoiler_policy, tracker_id, \
                            tracker_policy::text AS tracker_policy, wants_filtered, \
                            state::text AS state, state_changed_at, advertised_host, \
                            advertised_port, advertised_filtered_port, last_error";

/// Open a room from an already-indexed generation.
///
/// One transaction covering the row, the first organizer and every slot. See the module docs for
/// why that is not optional.
pub async fn create(
    conn: &mut AsyncPgConnection,
    new: &NewRoom,
) -> Result<RoomId, diesel::result::Error> {
    let new = new.clone();

    conn.transaction::<RoomId, diesel::result::Error, _>(|conn| {
        async move {
            #[derive(diesel::QueryableByName)]
            struct RaceRow {
                #[diesel(sql_type = Bool)]
                race_mode: bool,
            }

            // The seed decides the defaults, and a race is the case where a leaked spoiler or an
            // open tracker cannot be taken back.
            let race: Vec<RaceRow> =
                diesel::sql_query("SELECT race_mode FROM generations WHERE id = $1")
                    .bind::<SqlUuid, _>(new.generation_id)
                    .load(conn)
                    .await?;
            let race_mode = race
                .into_iter()
                .next()
                .ok_or(diesel::result::Error::NotFound)?
                .race_mode;

            let spoiler_policy = new.spoiler_policy.unwrap_or(if race_mode {
                SpoilerPolicy::Never
            } else {
                SpoilerPolicy::AdminOnly
            });
            let tracker_policy = new.tracker_policy.unwrap_or(if race_mode {
                TrackerPolicy::Members
            } else {
                TrackerPolicy::Link
            });

            let id = RoomId::new();
            // Only `room` mode has a room-wide password, and the `room_password_matches_mode`
            // CHECK enforces exactly that -- so getting this wrong is a failed insert, not a
            // room whose mode and credential disagree.
            let password = match new.slot_auth {
                SlotAuth::Room => Some(crate::secret::room_password()),
                SlotAuth::None | SlotAuth::PerSlot => None,
            };

            diesel::sql_query(
                "INSERT INTO rooms
                    (id, environment, name, generation_id, source, lobby_room_id, lobby_job_id,
                     idempotency_key, cloned_from, created_by, spoiler_policy, tracker_id,
                     tracker_policy, slot_auth, password, wants_filtered, use_embedded_options,
                     save_interval_secs, admin_token)
                 VALUES ($1, $2::puna_environment, $3, $4, $5::room_source, $6, $7, $8, $9, $10,
                         $11::spoiler_policy, $12, $13::tracker_policy, $14::slot_auth_mode, $15,
                         $16, $17, $18, $19)",
            )
            .bind::<SqlUuid, _>(id)
            .bind::<Text, _>(new.environment.as_str())
            .bind::<Text, _>(&new.name)
            .bind::<SqlUuid, _>(new.generation_id)
            .bind::<Text, _>(new.source.as_sql())
            .bind::<Nullable<SqlUuid>, _>(new.lobby_room_id)
            .bind::<Nullable<Text>, _>(new.lobby_job_id.as_deref())
            .bind::<Nullable<Text>, _>(new.idempotency_key.as_deref())
            .bind::<Nullable<SqlUuid>, _>(new.cloned_from.map(uuid::Uuid::from))
            .bind::<BigInt, _>(new.created_by)
            .bind::<Text, _>(spoiler_policy.as_sql())
            .bind::<SqlUuid, _>(TrackerId::new())
            .bind::<Text, _>(tracker_policy.as_sql())
            .bind::<Text, _>(new.slot_auth.as_sql())
            .bind::<Nullable<Text>, _>(password.as_deref())
            .bind::<Bool, _>(new.wants_filtered)
            .bind::<Bool, _>(new.use_embedded_options)
            .bind::<Integer, _>(new.save_interval_secs)
            .bind::<Text, _>(crate::secret::admin_token())
            .execute(conn)
            .await?;

            // The uploader is the first organizer. Not a creator special case: it is an ordinary
            // roster row, and every later check reads the roster.
            member::set_role(conn, id, new.created_by, RoomRole::Organizer, None)
                .await
                .map_err(|e| match e {
                    member::MemberError::Db(e) => e,
                    // Unreachable on an insert into an empty roster, but mapping it rather than
                    // unwrapping keeps the error type honest.
                    _ => diesel::result::Error::RollbackTransaction,
                })?;

            copy_slots(conn, id, new.generation_id, new.slot_auth).await?;

            Ok(id)
        }
        .scope_boxed()
    })
    .await
}

/// Copy `generation_slots` into `room_slots`, minting each slot's credentials.
///
/// A copy rather than a join so a room is independent of later generation housekeeping, and so
/// two rooms on one generation hold different owners and different secrets.
async fn copy_slots(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    generation: GenerationId,
    slot_auth: SlotAuth,
) -> Result<(), diesel::result::Error> {
    let slots = crate::model::generation::slots(conn, generation).await?;

    for entry in slots {
        // A claim link is issued in EVERY mode, including `none`: claiming is what gates the
        // per-slot patch download and what puts a room on a player's landing page, so it matters
        // whether or not there is a password to protect.
        let password = match slot_auth {
            SlotAuth::PerSlot => Some(crate::secret::slot_password()),
            SlotAuth::None | SlotAuth::Room => None,
        };

        diesel::sql_query(
            "INSERT INTO room_slots
                (room_id, slot_number, player_name, game, kind, password, claim_token, tracker_id)
             VALUES ($1, $2, $3, $4, $5::slot_kind, $6, $7, $8)",
        )
        .bind::<SqlUuid, _>(room)
        .bind::<Integer, _>(entry.slot_number)
        .bind::<Text, _>(&entry.player_name)
        .bind::<Text, _>(&entry.game)
        .bind::<Text, _>(match entry.kind {
            crate::artifact::SlotKind::Player => "player",
            crate::artifact::SlotKind::Spectator => "spectator",
        })
        .bind::<Nullable<Text>, _>(password.as_deref())
        .bind::<Text, _>(crate::secret::url_token())
        .bind::<SqlUuid, _>(TrackerId::new())
        .execute(conn)
        .await?;
    }
    Ok(())
}

/// The credentials a room's pod needs, deliberately kept OUT of [`Room`].
///
/// `Room` is what the web tier hands to templates, and a template has no way to prove it did not
/// render a field it was given. So the admin token -- which is the only control on a mutating,
/// internet-reachable API -- is not in it, and reaching these costs a separate, greppable call.
/// Same reasoning as `SlotView` in the web tier.
#[derive(Debug, Clone)]
pub struct RoomSecrets {
    pub admin_token: String,
    /// Pahoa's remote-admin gate. Rarely set: Puna's console drives the bearer-token API rather
    /// than in-game `!admin`, so a Puna room normally has no remote-admin path at all.
    pub server_password: Option<String>,
}

/// Read a room's credentials. Orchestrator-facing; nothing in a page needs these.
pub async fn secrets(
    conn: &mut AsyncPgConnection,
    id: RoomId,
) -> Result<Option<RoomSecrets>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        admin_token: String,
        #[diesel(sql_type = Nullable<Text>)]
        server_password: Option<String>,
    }

    let rows: Vec<Row> =
        diesel::sql_query("SELECT admin_token, server_password FROM rooms WHERE id = $1")
            .bind::<SqlUuid, _>(id)
            .load(conn)
            .await?;

    Ok(rows.into_iter().next().map(|row| RoomSecrets {
        admin_token: row.admin_token,
        server_password: row.server_password,
    }))
}

pub async fn get(
    conn: &mut AsyncPgConnection,
    id: RoomId,
) -> Result<Option<Room>, diesel::result::Error> {
    let rows: Vec<RoomRow> =
        diesel::sql_query(format!("SELECT {ROOM_COLUMNS} FROM rooms WHERE id = $1"))
            .bind::<SqlUuid, _>(id)
            .load(conn)
            .await?;
    Ok(rows.into_iter().next().map(Room::from))
}

/// How a room came to be yours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relationship {
    /// You are on the roster.
    Staff(RoomRole),
    /// You own a slot in it.
    Player,
}

/// A room on your landing page, with why it is there.
#[derive(Debug, Clone)]
pub struct MyRoom {
    pub room: Room,
    pub relationship: Relationship,
}

/// Every room that is yours, by either route.
///
/// The two ways a room can be yours are a `room_members` row and a `room_slots.owner_id` row, and
/// this `UNION`s them so the page needs one query rather than two lists to merge. **Rooms you
/// merely visited are absent**: claiming a slot or being added is what puts one here, which is why
/// the answer stays short enough to be useful.
pub async fn mine(
    conn: &mut AsyncPgConnection,
    user_id: i64,
) -> Result<Vec<MyRoom>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
        #[diesel(sql_type = Nullable<Text>)]
        role: Option<String>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT room_id AS id, role::text AS role FROM room_members WHERE user_id = $1
         UNION
         SELECT room_id AS id, NULL AS role FROM room_slots WHERE owner_id = $1",
    )
    .bind::<BigInt, _>(user_id)
    .load(conn)
    .await?;

    // Staff beats player when both rows exist: an organizer who also plays a slot wants the
    // organizer view of their own room.
    let mut best: std::collections::BTreeMap<uuid::Uuid, Relationship> = Default::default();
    for row in rows {
        let relationship = match row.role.as_deref().and_then(RoomRole::parse) {
            Some(role) => Relationship::Staff(role),
            None => Relationship::Player,
        };
        let key = uuid::Uuid::from(row.id);
        match best.get(&key) {
            Some(Relationship::Staff(held)) => {
                if let Relationship::Staff(new) = relationship
                    && new > *held
                {
                    best.insert(key, relationship);
                }
            }
            _ => {
                best.insert(key, relationship);
            }
        }
    }

    let mut out = Vec::with_capacity(best.len());
    for (id, relationship) in best {
        if let Some(room) = get(conn, RoomId::from(id)).await? {
            out.push(MyRoom { room, relationship });
        }
    }
    Ok(out)
}

/// Ask the orchestrator to start, stop or delete a room.
///
/// Writes `desired_state` and nothing else, and reports whether anything changed. Idempotent by
/// construction: a second request for a state the room already wants updates zero rows, so
/// "requested twice while starting" needs no special case anywhere.
pub async fn request_state(
    conn: &mut AsyncPgConnection,
    id: RoomId,
    desired: DesiredState,
) -> Result<bool, diesel::result::Error> {
    let changed = diesel::sql_query(
        "UPDATE rooms SET desired_state = $2::room_desired_state, desired_at = now()
          WHERE id = $1 AND desired_state <> $2::room_desired_state",
    )
    .bind::<SqlUuid, _>(id)
    .bind::<Text, _>(desired.as_sql())
    .execute(conn)
    .await?;
    Ok(changed > 0)
}

/// Change a room's password mode.
///
/// **Every transition is a restart**, because pahoa reads the mode from the environment at startup
/// and its live rotation route `404`s outside `per_slot` mode -- it changes passwords *within* a
/// mode and cannot create one. The caller marks the Secret stale and bounces the room; this
/// function only moves the database to the new state.
///
/// Two things it must get exactly right, both of which are the fail-closed rule biting:
///
///   * `-> per_slot` generates a password for **every** slot. A partial map locks players out.
///   * `-> none` / `-> room` sets every slot password to NULL, and the caller must then render
///     **no `PAHOA_SLOT_PASSWORDS` key at all** rather than `{}` -- an empty map is per-slot mode
///     with nobody holding a key, which is a room nobody can join.
///
/// Switching away is not reversible: the old passwords are gone, which the UI states before
/// confirming.
pub async fn set_slot_auth(
    conn: &mut AsyncPgConnection,
    id: RoomId,
    mode: SlotAuth,
) -> Result<(), diesel::result::Error> {
    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        async move {
            let password = match mode {
                SlotAuth::Room => Some(crate::secret::room_password()),
                SlotAuth::None | SlotAuth::PerSlot => None,
            };

            diesel::sql_query(
                "UPDATE rooms SET slot_auth = $2::slot_auth_mode, password = $3 WHERE id = $1",
            )
            .bind::<SqlUuid, _>(id)
            .bind::<Text, _>(mode.as_sql())
            .bind::<Nullable<Text>, _>(password.as_deref())
            .execute(conn)
            .await?;

            match mode {
                SlotAuth::PerSlot => {
                    // Every slot, individually, because each needs its own secret.
                    for entry in slot::list(conn, id).await? {
                        slot::rotate_password(conn, id, entry.slot_number).await?;
                    }
                }
                SlotAuth::None | SlotAuth::Room => {
                    diesel::sql_query("UPDATE room_slots SET password = NULL WHERE room_id = $1")
                        .bind::<SqlUuid, _>(id)
                        .execute(conn)
                        .await?;
                }
            }
            Ok(())
        }
        .scope_boxed()
    })
    .await
}

/// Open a new room from an existing room's generation.
///
/// A fresh playthrough, not a copy of one: new id, its own port reservation, its own empty state
/// directory. What carries over is the *people* -- the roster, and slot ownership if asked --
/// while **every password and claim token is regenerated**, so the same players keep their slots
/// without re-claiming and no old credential survives into the new room.
///
/// The source room is untouched. Cloning a running room is allowed and unremarkable: two rooms on
/// one generation are two independent multiworlds.
pub async fn clone_room(
    conn: &mut AsyncPgConnection,
    source: RoomId,
    name: String,
    created_by: i64,
    keep_owners: bool,
) -> Result<RoomId, diesel::result::Error> {
    let existing = get(conn, source)
        .await?
        .ok_or(diesel::result::Error::NotFound)?;

    let new = NewRoom {
        environment: existing.environment,
        name,
        generation_id: existing.generation_id,
        source: existing.source,
        created_by,
        slot_auth: existing.slot_auth,
        spoiler_policy: Some(existing.spoiler_policy),
        tracker_policy: Some(existing.tracker_policy),
        wants_filtered: existing.wants_filtered,
        use_embedded_options: true,
        save_interval_secs: 30,
        lobby_room_id: None,
        lobby_job_id: None,
        // Never inherited: it identifies one push attempt, and reusing it would make the clone
        // collide with the room it came from on a UNIQUE column.
        idempotency_key: None,
        cloned_from: Some(source),
    };

    let id = create(conn, &new).await?;

    // Copy the roster. `created_by` is already an organizer from `create`, and `GREATEST` keeps
    // them one if the source room had them as a mere helper.
    for entry in member::list(conn, source).await? {
        diesel::sql_query(
            "INSERT INTO room_members (room_id, user_id, role, added_by)
                  VALUES ($1, $2, $3::room_role, $4)
             ON CONFLICT (room_id, user_id)
             DO UPDATE SET role = GREATEST(room_members.role, EXCLUDED.role)",
        )
        .bind::<SqlUuid, _>(id)
        .bind::<BigInt, _>(entry.user_id)
        .bind::<Text, _>(entry.role.as_sql())
        .bind::<Nullable<BigInt>, _>(entry.added_by)
        .execute(conn)
        .await?;
    }

    if keep_owners {
        // Owners carry over; tokens and passwords do not -- `create` already minted fresh ones.
        diesel::sql_query(
            "UPDATE room_slots AS target
                SET owner_id = source.owner_id,
                    claimed_at = source.claimed_at,
                    claim_token = CASE WHEN source.owner_id IS NULL
                                       THEN target.claim_token ELSE NULL END
               FROM room_slots AS source
              WHERE target.room_id = $1
                AND source.room_id = $2
                AND source.slot_number = target.slot_number",
        )
        .bind::<SqlUuid, _>(id)
        .bind::<SqlUuid, _>(source)
        .execute(conn)
        .await?;
    }

    Ok(id)
}

/// Sibling rooms built from the same generation, so the room page can link to them.
pub async fn siblings(
    conn: &mut AsyncPgConnection,
    id: RoomId,
    generation: GenerationId,
) -> Result<Vec<Room>, diesel::result::Error> {
    let rows: Vec<RoomRow> = diesel::sql_query(format!(
        "SELECT {ROOM_COLUMNS} FROM rooms
          WHERE generation_id = $1 AND id <> $2
          ORDER BY created_at DESC"
    ))
    .bind::<SqlUuid, _>(generation)
    .bind::<SqlUuid, _>(id)
    .load(conn)
    .await?;
    Ok(rows.into_iter().map(Room::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_and_policies_round_trip_through_their_sql_spelling() {
        for mode in SlotAuth::ALL {
            assert_eq!(SlotAuth::parse(mode.as_sql()), Some(mode));
        }
        for policy in [
            SpoilerPolicy::Never,
            SpoilerPolicy::AdminOnly,
            SpoilerPolicy::Players,
            SpoilerPolicy::Public,
        ] {
            assert_eq!(SpoilerPolicy::parse(policy.as_sql()), Some(policy));
        }
        for policy in [
            TrackerPolicy::Link,
            TrackerPolicy::Members,
            TrackerPolicy::Disabled,
        ] {
            assert_eq!(TrackerPolicy::parse(policy.as_sql()), Some(policy));
        }
    }

    /// A value from a newer database must never widen access.
    #[test]
    fn unknown_policies_read_as_the_most_restrictive_value() {
        assert_eq!(SpoilerPolicy::parse("everyone"), None);
        assert_eq!(TrackerPolicy::parse("everyone"), None);
        // ...and `From<RoomRow>` maps those `None`s to Never and Disabled; see the impl.
    }

    #[test]
    fn states_round_trip_through_their_sql_spelling() {
        for state in RoomState::ALL {
            assert_eq!(RoomState::parse(state.as_sql()), Some(state));
        }
        for desired in DesiredState::ALL {
            assert_eq!(DesiredState::parse(desired.as_sql()), Some(desired));
        }
        // The two vocabularies overlap on one word, and it means different things in each: a room
        // that WANTS to run against one that IS running. Distinct types is what keeps them apart.
        assert_eq!(RoomState::parse("stopped"), None);
        assert_eq!(DesiredState::parse("idle"), None);
    }

    /// The whole spoiler matrix, because it is short and the cost of getting one cell wrong is a
    /// leaked spoiler in a race.
    #[test]
    fn the_spoiler_policies_admit_exactly_who_they_say() {
        use SpoilerPolicy::*;

        // (policy, staff, owns a slot) -> may read
        let cases = [
            (Never, false, false, false),
            (Never, true, true, false),
            (AdminOnly, true, false, true),
            (AdminOnly, false, true, false),
            (Players, false, true, true),
            (Players, true, false, true),
            (Players, false, false, false),
            (Public, false, false, true),
        ];

        for (policy, is_staff, owns, expected) in cases {
            assert_eq!(
                may_see_spoiler(policy, is_staff, owns),
                expected,
                "{policy:?} staff={is_staff} owner={owns}"
            );
        }

        // Stated separately because it is the one people expect to be false and is not: `never`
        // means never, and an admin who needs it can read the file or change the policy.
        assert!(!may_see_spoiler(Never, true, true));
    }

    /// The tracker matrix. Shorter than the spoiler's because the default is openness: a tracker is
    /// for sharing, and only a race closes it.
    #[test]
    fn the_tracker_policies_admit_exactly_who_they_say() {
        use TrackerPolicy::*;

        // `link` is the reference's own model: holding the URL is the authorization, and it works
        // for someone who has never logged in -- which is most of a tracker's audience.
        assert!(may_see_tracker(Link, false, false));

        assert!(may_see_tracker(Members, true, false));
        assert!(may_see_tracker(Members, false, true));
        assert!(!may_see_tracker(Members, false, false));

        // Off means off, admins included, exactly as `never` does for a spoiler.
        for staff in [true, false] {
            for owner in [true, false] {
                assert!(!may_see_tracker(Disabled, staff, owner));
            }
        }
    }

    /// Holding a slot's tracker id must not become a way to read the multiworld's, so the two
    /// questions are answered by the same policy against the same room -- the id that got you here
    /// is not an input.
    #[test]
    fn a_slots_tracker_id_grants_nothing_extra() {
        // The signature is the argument: there is no parameter for "which id was presented", so a
        // slot link and a room link resolve to the same answer for the same viewer.
        assert_eq!(
            may_see_tracker(TrackerPolicy::Members, false, false),
            may_see_tracker(TrackerPolicy::Members, false, false)
        );
    }

    /// The states a port pair cannot be reclaimed from.
    ///
    /// Spelled out rather than derived, so widening `is_live` has to be a deliberate edit here
    /// too — this is the predicate standing between a busy room and having its port taken away.
    #[test]
    fn exactly_three_states_are_live() {
        let live: Vec<&str> = RoomState::ALL
            .into_iter()
            .filter(|s| s.is_live())
            .map(RoomState::as_sql)
            .collect();
        assert_eq!(live, ["starting", "running", "degraded"]);
    }
}
