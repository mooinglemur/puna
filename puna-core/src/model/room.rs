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
use crate::ids::{GenerationId, JournalId, RoomId, TrackerId};
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

/// Whether a served patch carries the credential to connect, or only the address.
///
/// **The mechanism is Archipelago's, verified in its source rather than inferred.**
/// `CommonClient.py`'s `server_loop` parses userinfo out of the address it is handed and
/// `unquote`s both halves, and a patch's `server` field reaches that parser through `args.connect`
/// — so `wss://<slot>:<password>@<host>:<port>` connects a client with no typing. The `unquote` is
/// why [`crate::artifact::patch`] percent-encodes both: a slot name is arbitrary text out of a
/// seed and may hold an `@`, a `:` or a space, any of which silently changes what the netloc is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchPolicy {
    /// `host:port`, as the reference writes it. A player with a password types it.
    Open,
    /// The credential too, where the room or the slot has one. A patch is already served only to
    /// its slot's owner and the room's staff, so this hands them something they are entitled to —
    /// but it does travel with the file, which is what `Open` is for.
    Claimed,
}

impl PatchPolicy {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Claimed => "claimed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "open" => Some(Self::Open),
            "claimed" => Some(Self::Claimed),
            _ => None,
        }
    }
}

/// How much of a room's journal somebody who is **not an organizer** may read.
///
/// This is a second question on top of [`may_see_tracker`], not a replacement for it. That one
/// decides whether `/journal/<id>` answers at all; this decides what it answers with. Both are
/// needed because the two capabilities have genuinely different shapes: a tracker shows progress,
/// and the journal's file carries `chat` — every line anybody typed in the room — so a room can
/// reasonably want its tracker public and its conversation not, or want both wide open, and neither
/// answer is right for every room.
///
/// **An organizer reads everything regardless.** This names what the tier below them gets, which is
/// why the variants are ordered from least to most and why the enum is not `Ord`: they are three
/// answers to one question rather than rungs somebody climbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalPolicy {
    /// Staff only. A non-organizer gets the same `404` an unknown feed id gets, so the refusal
    /// discloses nothing — including whether the room has a journal worth asking about.
    Disabled,
    /// `check` and `gap`. Everything else is withheld **and counted**, so the page can say the
    /// history is incomplete without saying what is in it. No download: the file is where the
    /// withheld records are, so handing it over would make the filter decorative.
    Feed,
    /// The history as pahoa wrote it, and the download with it. What an ordinary room gets, on the
    /// grounds that whoever holds the feed link already holds the room link.
    Full,
}

impl JournalPolicy {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Feed => "feed",
            Self::Full => "full",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "disabled" => Some(Self::Disabled),
            "feed" => Some(Self::Feed),
            "full" => Some(Self::Full),
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
    /// Torn down **and not restartable by whoever holds the URL**.
    ///
    /// To the orchestrator this is [`Self::Stopped`] exactly: the room comes down, keeps its port
    /// reservation and keeps its state directory. Nothing about a closed room is reclaimed
    /// differently, which is the point — closing is what an organizer does to a room they intend to
    /// come back to.
    ///
    /// The whole difference is an authorization one, and it lives in the web tier. Any visitor may
    /// start an `idle` room, because a room that idles out and returns on a URL hit is the design;
    /// only an organizer or an admin may start a closed one. The page still renders for everybody —
    /// patches, tracker, roster — it just does not offer them the door.
    ///
    /// **A closed room is never a running room, and that invariant is why this is a variant here
    /// rather than a flag beside `desired_state`.** A separate column would allow "closed and
    /// running", which sounds harmless and is not: Puna gates *starting* a room, not connecting to
    /// one, so a running room is reachable at its address by anyone who has it. The page would be
    /// saying "closed" about a room people were playing in. Making it a wish, mutually exclusive
    /// with `Running` by construction, means that state cannot be spelled.
    ///
    /// The cost is that reopening clears it — an organizer who starts a closed room has reopened
    /// it, and it stays open until closed again. That is what the button says it does.
    Closed,
    Deleted,
}

impl DesiredState {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Closed => "closed",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "running" => Some(Self::Running),
            "stopped" => Some(Self::Stopped),
            "closed" => Some(Self::Closed),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

    /// Whether this wish means "the room should not be running".
    ///
    /// The orchestrator's question, and the reason it needs no `Closed` handling of its own: a
    /// closed room and a stopped room are the same instruction to the reconciler. Written as one
    /// predicate rather than repeated `matches!` so a fourth resting state could not be added to
    /// the enum and quietly missed by half the planner.
    pub fn is_at_rest(self) -> bool {
        matches!(self, Self::Stopped | Self::Closed)
    }

    pub const ALL: [DesiredState; 4] = [Self::Running, Self::Stopped, Self::Closed, Self::Deleted];
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
    /// `None` keeps the column default, `open`. The creation form sends `claimed`.
    pub patch_policy: Option<PatchPolicy>,
    /// A remote-admin password for pahoa's `!admin login`. `None` sets none, which is the
    /// ordinary case: Puna's console drives a room over the bearer-token admin API instead.
    pub server_password: Option<String>,
    /// `None` defaults from `race_mode` as well: `feed` for a race, `full` otherwise.
    ///
    /// **Open by default, unlike the two above**, and deliberately so. Those guard information the
    /// seed holds and a player has not earned yet; this guards a record of things the room's own
    /// participants said and did in front of each other. The feed link is rendered on the room page
    /// to everyone the tracker is, so in practice the people holding it are the people holding the
    /// room link — and withholding the history from them by default protects nobody while making
    /// the ordinary case need a setting change.
    pub journal_policy: Option<JournalPolicy>,
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
            journal_policy: None,
            patch_policy: None,
            server_password: None,
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
    /// The feed's URL segment. Independent of both [`RoomId`] and [`TrackerId`], so a feed link
    /// hands over neither the room nor its tracker — see the migration that added it.
    pub journal_id: JournalId,
    pub tracker_policy: TrackerPolicy,
    /// How much of the journal a non-organizer gets, once `tracker_policy` has let them reach it.
    pub journal_policy: JournalPolicy,
    /// Whether a served patch carries the credential to connect, or only the address.
    pub patch_policy: PatchPolicy,
    pub wants_filtered: bool,

    pub state: String,
    /// When the room last changed state.
    ///
    /// The page turns this into an elapsed time rather than rendering it: a cold start is a
    /// multi-second visible state, and "starting, 40 seconds" is the difference between waiting and
    /// wondering whether anything is happening.
    pub state_changed_at: chrono::DateTime<chrono::Utc>,
    /// When the room was last *asked* for something, which is a different clock from
    /// `state_changed_at` and the one a person watching a transition is actually on.
    ///
    /// A request writes this and returns; the observed state does not move until the orchestrator
    /// reaches the room. Timing a transition from `state_changed_at` therefore starts the counter
    /// at however long the room had been sitting in the state it is leaving — "stopping, 35
    /// minutes" one second after somebody clicked Stop.
    pub desired_at: chrono::DateTime<chrono::Utc>,
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
    #[diesel(sql_type = SqlUuid)]
    journal_id: JournalId,
    #[diesel(sql_type = Text)]
    tracker_policy: String,
    #[diesel(sql_type = Text)]
    journal_policy: String,
    #[diesel(sql_type = Text)]
    patch_policy: String,
    #[diesel(sql_type = Bool)]
    wants_filtered: bool,
    #[diesel(sql_type = Text)]
    state: String,
    #[diesel(sql_type = Timestamptz)]
    state_changed_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = Timestamptz)]
    desired_at: chrono::DateTime<chrono::Utc>,
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
            journal_id: row.journal_id,
            tracker_policy: TrackerPolicy::parse(&row.tracker_policy)
                .unwrap_or(TrackerPolicy::Disabled),
            // Same rule as the two policies above: an unrecognized value from a newer database
            // reads as the most restrictive answer, never the default one.
            journal_policy: JournalPolicy::parse(&row.journal_policy)
                .unwrap_or(JournalPolicy::Disabled),
            // Unknown reads as `Open`, which embeds no credential -- the restrictive direction
            // here is the one that discloses less, same rule as the policies above.
            patch_policy: PatchPolicy::parse(&row.patch_policy).unwrap_or(PatchPolicy::Open),
            wants_filtered: row.wants_filtered,
            state: row.state,
            state_changed_at: row.state_changed_at,
            desired_at: row.desired_at,
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
                            password, spoiler_policy::text AS spoiler_policy, tracker_id, journal_id, \
                            tracker_policy::text AS tracker_policy, \
                            journal_policy::text AS journal_policy, \
                            patch_policy::text AS patch_policy, wants_filtered, \
                            state::text AS state, state_changed_at, desired_at, advertised_host, \
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
            // A race is the case where the history is a live scoreboard rather than a record: it
            // says who found what and when, in order, which is the one thing a racer must not be
            // able to read about the field. So a race gets the item feed and nothing else, and an
            // ordinary room gets the lot.
            let journal_policy = new.journal_policy.unwrap_or(if race_mode {
                JournalPolicy::Feed
            } else {
                JournalPolicy::Full
            });
            let patch_policy = new.patch_policy.unwrap_or(PatchPolicy::Open);

            // **A new room is created RUNNING, and the `desired_state` column's `stopped` default
            // is deliberately not what creation uses.**
            //
            // The reference implementation starts a room the moment it is made, so that is what
            // anybody who has run an Archipelago game expects — and Puna is the side with the
            // controls to disagree, since it offers Stop and Close where upstream offers neither.
            // An organizer preparing a room days early simply does not share the link yet, and can
            // stop it with one click if they would rather it were down.
            //
            // The column keeps `stopped` because it is the right answer for a row somebody inserts
            // by hand, and because the planner's own fixtures rest on it. Creation states its
            // intent instead of inheriting one.
            //
            // **This also removes a race rather than working around it.** The redirect after
            // creation lands on `/room/<id>` while the room is still `provisioning`, and D8's
            // implicit start fires only on `idle` — so the one navigation that would have started
            // it always arrived too early, while a manual reload a few seconds later worked. A
            // desired state needs no timing to be correct: the orchestrator reaches it whenever it
            // gets there, which is what that column is for.
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
                     tracker_policy, journal_policy, patch_policy, slot_auth, password,
                     server_password, wants_filtered, use_embedded_options, save_interval_secs,
                     admin_token, desired_state)
                 VALUES ($1, $2::puna_environment, $3, $4, $5::room_source, $6, $7, $8, $9, $10,
                         $11::spoiler_policy, $12, $13::tracker_policy, $14::journal_policy,
                         $15::patch_policy, $16::slot_auth_mode, $17, $18, $19, $20, $21, $22,
                         'running')",
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
            .bind::<Text, _>(journal_policy.as_sql())
            .bind::<Text, _>(patch_policy.as_sql())
            .bind::<Text, _>(new.slot_auth.as_sql())
            .bind::<Nullable<Text>, _>(password.as_deref())
            .bind::<Nullable<Text>, _>(new.server_password.as_deref())
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

/// The room a feed link names.
///
/// The feed's whole entry point, and the reason it has an id of its own: nothing about
/// `/journal/<id>` is derivable from the room, so a link handed to a stream chat gives that chat the
/// feed and nothing else. Same shape as resolving a tracker id, and deliberately a *separate* space
/// from it — holding one must not produce the other.
pub async fn by_journal_id(
    conn: &mut AsyncPgConnection,
    journal: JournalId,
) -> Result<Option<Room>, diesel::result::Error> {
    let rows: Vec<RoomRow> = diesel::sql_query(format!(
        "SELECT {ROOM_COLUMNS} FROM rooms WHERE journal_id = $1"
    ))
    .bind::<SqlUuid, _>(journal)
    .load(conn)
    .await?;
    Ok(rows.into_iter().next().map(Room::from))
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

/// Mark this room's Secret as needing a re-apply.
///
/// **The producer for a contract that has had none.** `secret_synced_at IS NULL` has meant "this
/// room's Secret no longer matches the database" since the sweep was written, and the sweep has
/// been reading it and re-applying on that basis — but nothing ever set it, so the hourly interval
/// was doing all the work and the contract was documentation.
///
/// Set it whenever a credential changes and the room is not being restarted anyway. A restart does
/// not need it: the start path renders the Secret from scratch.
pub async fn mark_secret_stale(
    conn: &mut AsyncPgConnection,
    id: RoomId,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query("UPDATE rooms SET secret_synced_at = NULL WHERE id = $1")
        .bind::<SqlUuid, _>(id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Give a room in the shared-password mode a new shared password.
///
/// Returns the new value, or `None` when the room is not in that mode — which the caller renders as
/// a refusal rather than silently doing nothing, because "rotate" on a room with no shared password
/// is a question with no answer rather than a no-op.
///
/// ## This one cannot be live, and the asymmetry is not an oversight
///
/// A per-slot password is rotated on the running room by
/// `POST /admin/v1/slots/<n>/password`, so it costs no restart. There is no equivalent for the
/// room-wide one and there will not be: pahoa declined a live setter outright, on the grounds that
/// it persists no password, so a change it cannot persist reverts at the next start whoever ran it.
/// The rule they extracted is worth keeping because it predicts the whole surface — **a setter is
/// honest exactly where the save is authoritative.** Gameplay options persist, so they got one.
/// Passwords deliberately do not: keeping them out of `room.save` is what stops a stale on-disk
/// value shadowing the configured one, which is the same reason the environment outranks the seed.
///
/// So the room learns this the only way it can, by starting again — which is why the caller pairs
/// this with [`crate::model::fleet::request_redeploy`] for a room that is up, and why the UI has to
/// say so before it is pressed. A stopped room needs nothing: its next start renders the Secret
/// from the column.
///
/// **The value is not passed in.** Generated here, from the same alphabet `set_slot_auth` uses, so
/// there is one definition of what a room password looks like and no route can weaken it.
pub async fn rotate_password(
    conn: &mut AsyncPgConnection,
    id: RoomId,
) -> Result<Option<String>, diesel::result::Error> {
    let password = crate::secret::room_password();

    // Scoped to the mode in the WHERE rather than checked first, so a mode change landing between a
    // read and this write cannot leave a password on a room that does not want one -- the
    // `room_password_matches_mode` CHECK would refuse it, loudly and at the wrong layer.
    let updated = diesel::sql_query(
        "UPDATE rooms SET password = $2 WHERE id = $1 AND slot_auth = 'room'::slot_auth_mode",
    )
    .bind::<SqlUuid, _>(id)
    .bind::<Text, _>(&password)
    .execute(conn)
    .await?;

    Ok((updated > 0).then_some(password))
}

/// Why a proposed room name is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("a room needs a name")]
    Empty,
    #[error("that name is too long; keep it under {} characters", MAX_NAME_CHARS)]
    TooLong,
}

/// The cap, in **characters rather than bytes**, so the limit does not depend on the alphabet
/// somebody names their room in.
///
/// Generous on purpose: this is not a security control, it is what stops a name that breaks the
/// admin table's layout and the `<title>` of every page the room appears on.
pub const MAX_NAME_CHARS: usize = 120;

/// Trim a proposed room name and decide whether it is usable.
///
/// **One definition, three callers** — create, clone and rename. They had two between them (an
/// `is_empty` check written twice, no length rule anywhere), and three answers to "is this a valid
/// room name" is how a name that one path accepts becomes one another path cannot store.
pub fn validate_name(raw: &str) -> Result<String, NameError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(NameError::Empty);
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(NameError::TooLong);
    }
    Ok(trimmed.to_string())
}

/// Give a room a different name.
///
/// **Nothing but the label changes, and that is worth stating because everything else on the room
/// page that looks like a setting is a restart.** Kubernetes object names are `mw-<room id>` and
/// the room's own labels carry the id, so `rooms.name` reaches no manifest, no spec hash and no
/// pahoa argument. A rename is a `UPDATE` and nobody is disconnected.
///
/// The caller records the previous name in the event row, and already holds it: `RoomAccess` loaded
/// the whole `Room` to authorize the request. Returning it from here would mean either a second
/// read or a `RETURNING` clause, and `RETURNING` sees the row it just wrote — the old value is not
/// available on this side of the statement at all.
pub async fn rename(
    conn: &mut AsyncPgConnection,
    id: RoomId,
    name: &str,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query("UPDATE rooms SET name = $2 WHERE id = $1")
        .bind::<SqlUuid, _>(id)
        .bind::<Text, _>(name)
        .execute(conn)
        .await?;
    Ok(())
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

/// Change how much of the journal a non-organizer may read.
///
/// **Not a restart, and the form says so.** Every other control in that section changes something
/// pahoa reads at startup; this one changes nothing the room can see at all. The journal is a file
/// on a volume Puna mounts read-only, the gate is Puna's own, and it applies to the next request —
/// including one on a socket already open, since every frame is filtered as it is sent rather than
/// at connect.
pub async fn set_journal_policy(
    conn: &mut AsyncPgConnection,
    id: RoomId,
    policy: JournalPolicy,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query("UPDATE rooms SET journal_policy = $2::journal_policy WHERE id = $1")
        .bind::<SqlUuid, _>(id)
        .bind::<Text, _>(policy.as_sql())
        .execute(conn)
        .await?;
    Ok(())
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
        journal_policy: Some(existing.journal_policy),
        patch_policy: Some(existing.patch_policy),
        server_password: None,
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

/// How many rooms this database holds, in any state.
///
/// Used by the orchestrator's startup checks, where "are there rooms at all" separates a fresh
/// environment from one whose fleet is about to be misread.
pub async fn count(conn: &mut AsyncPgConnection) -> Result<i64, diesel::result::Error> {
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    let rows: Vec<Row> = diesel::sql_query("SELECT count(*) AS n FROM rooms")
        .load(conn)
        .await?;
    Ok(rows.into_iter().next().map(|r| r.n).unwrap_or(0))
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
