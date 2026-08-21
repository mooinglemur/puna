//! What every room is actually running, and the one way to change it.
//!
//! ## Why this is a read model rather than a view on `Room`
//!
//! Every column here is *observed* — written onto the row by the orchestrator from the reads its
//! reconcile tick already makes — because **the web tier cannot see the cluster**. It holds no
//! ServiceAccount token, which is the point of the two-binary split, so an admin page that wanted
//! to ask Kubernetes what a pod is running could not. Instead the orchestrator writes what it sees
//! and this reads the row.
//!
//! ## Drift is defined once, here
//!
//! Two questions have to agree: *which rooms does the table flag* and *which rooms does "redeploy
//! everything drifted" act on*. Written twice — once in a template condition and once in a `WHERE`
//! clause — they would drift apart themselves, and the failure is silent in the worst direction: a
//! button that acts on a set the operator was never shown. So [`FleetRoom::drift`] is the single
//! definition, the table renders it, and the bulk action filters on it in Rust rather than in SQL.

use chrono::{DateTime, Utc};
use diesel::sql_types::{Array, BigInt, Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::Environment;

use crate::ids::RoomId;

/// Why a room disagrees with the spec it would render to now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    /// Running an image other than the fleet's configured one — the ordinary case after a
    /// `PUNA_PAHOA_IMAGE` bump, which deliberately does not disturb rooms that are already up.
    Image,
    /// The rest of the spec would render differently: a `slot_auth` change, a log level, a slot
    /// added to a per-slot room. Computed on the sweep's hourly lane, so it can lag by up to an
    /// hour — which is why the two are distinguished rather than collapsed into one flag.
    Spec,
}

impl Drift {
    pub fn as_str(self) -> &'static str {
        match self {
            Drift::Image => "image",
            Drift::Spec => "spec",
        }
    }

    /// The whole phrase, rather than a word the template completes.
    ///
    /// `askama.toml` sets `whitespace = "suppress"`, so `{{ kind }} drift` in markup renders
    /// `imagedrift` -- the space is adjacent to a tag and is stripped. Keeping the full label on
    /// this side sidesteps that and makes the wording something a test can assert on.
    pub fn label(self) -> &'static str {
        match self {
            Drift::Image => "image drift",
            Drift::Spec => "spec drift",
        }
    }
}

/// Which half of the fleet a query is asking about.
///
/// **The split is on `desired_state`, not on `state`**, and that is the whole reason it is stable
/// enough to hang a page on. A room whose Deployment vanished reads `idle` for a few seconds while
/// the orchestrator puts it back — splitting on the observed state would move it into the "should
/// be down" table and out again while somebody was looking at it. Whether a room *should* be down
/// is a wish somebody expressed, and it only changes when they change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Rooms that should be up: `desired_state = 'running'`. A `failed` room is here, because it is
    /// trying to run and failing, which is the table an operator wants it in.
    Active,
    /// Rooms somebody stopped or closed. Unbounded and growing, which is why the page loads them
    /// separately.
    Resting,
    /// Both. What a bulk action scans, so it can never act on a set narrower than it claims.
    All,
}

impl Scope {
    /// The `WHERE` fragment, as a complete predicate so a caller cannot forget the `AND`.
    ///
    /// Qualified `r.` to match the one query that interpolates it. `deleted` is in neither named
    /// scope: a room on its way out is not something to restart and not something to list as
    /// resting.
    fn predicate(self) -> &'static str {
        match self {
            Scope::Active => "AND r.desired_state = 'running'",
            Scope::Resting => "AND r.desired_state IN ('stopped', 'closed')",
            Scope::All => "",
        }
    }
}

#[derive(Debug, Clone, diesel::QueryableByName)]
pub struct FleetRoom {
    #[diesel(sql_type = SqlUuid)]
    pub id: RoomId,
    #[diesel(sql_type = Text)]
    pub name: String,
    #[diesel(sql_type = Text)]
    pub state: String,
    #[diesel(sql_type = Text)]
    pub desired_state: String,
    /// Who opened the room. `None` where the account is gone or the row predates the column.
    #[diesel(sql_type = Nullable<BigInt>)]
    pub created_by: Option<i64>,
    /// Their Discord username at last login. `None` for an id with no `users` row; a stand-in for
    /// one that has never logged in, which [`crate::model::user::is_placeholder`] recognizes.
    #[diesel(sql_type = Nullable<Text>)]
    pub created_by_name: Option<String>,
    /// What the cluster says the pod is running. `None` for a room with no Deployment, and also
    /// for one whose container could not be identified by name.
    #[diesel(sql_type = Nullable<Text>)]
    pub running_image: Option<String>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub deployment_created_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub process_started_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = Nullable<Integer>)]
    pub clients_connected: Option<i32>,
    #[diesel(sql_type = Nullable<Text>)]
    pub spec_hash: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub desired_spec_hash: Option<String>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub redeploy_requested_at: Option<DateTime<Utc>>,
}

impl FleetRoom {
    /// Whether this room is running something other than what it would render to now.
    ///
    /// **Only a room with a Deployment can drift.** An idle room is not running anything, so it has
    /// nothing to disagree with — it picks up the current spec whenever it next starts, which is
    /// the behavior that makes an image bump safe to land at any hour.
    pub fn drift(&self, fleet_image: Option<&str>) -> Option<Drift> {
        let running = self.running_image.as_deref()?;

        // The image first: it is exact, it updates every tick, and it is the one an operator
        // changed on purpose. `None` for the fleet image means the orchestrator has not published
        // yet, and an unknown target is not a disagreement.
        if let Some(configured) = fleet_image
            && running != configured
        {
            return Some(Drift::Image);
        }

        // Then the rest of the spec, where both halves are known. `desired_spec_hash` is `None`
        // until the hourly lane has computed one, and "not computed" is never "changed".
        if let (Some(want), Some(have)) = (&self.desired_spec_hash, &self.spec_hash)
            && want != have
        {
            return Some(Drift::Spec);
        }

        None
    }

    /// How long the current spec has been in force, and how long *this* pahoa has been serving —
    /// the second only when it is meaningfully younger.
    ///
    /// They diverge when Kubernetes moved the pod or the container restarted in place. Either way
    /// the room reloaded its save and every client reconnected, which is a thing an organizer
    /// noticed and could not otherwise account for. Below the threshold the gap is just the pod
    /// scheduling and pahoa restoring the save, which every healthy room has and nobody wants a
    /// column full of.
    pub fn restarted_since_deploy(&self) -> Option<DateTime<Utc>> {
        let (deployed, started) = (self.deployment_created_at?, self.process_started_at?);
        (started - deployed > chrono::TimeDelta::seconds(60)).then_some(started)
    }
}

pub struct Overview {
    /// The image the orchestrator is configured with. `None` before it has published one, which on
    /// a fresh environment means it has not completed a tick yet.
    pub pahoa_image: Option<String>,
    pub rooms: Vec<FleetRoom>,
    /// How many rooms the scope left out because somebody stopped or closed them.
    ///
    /// Counted even when they are not loaded, because the collapsed heading has to say how much is
    /// behind it — "Stopped and closed rooms" with no number is a control nobody opens, and this
    /// list grows without bound.
    pub resting: i64,
}

impl Overview {
    pub fn drifted(&self) -> impl Iterator<Item = &FleetRoom> {
        self.rooms
            .iter()
            .filter(|room| room.drift(self.pahoa_image.as_deref()).is_some())
    }
}

pub async fn overview(
    conn: &mut AsyncPgConnection,
    environment: Environment,
    scope: Scope,
) -> Result<Overview, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Configured {
        #[diesel(sql_type = Text)]
        pahoa_image: String,
    }

    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }

    let configured: Vec<Configured> =
        diesel::sql_query("SELECT pahoa_image FROM fleet WHERE environment = $1::puna_environment")
            .bind::<Text, _>(environment.as_str())
            .load(conn)
            .await?;

    // `LEFT JOIN`, never an inner one: `created_by` is nullable and the FK is not cascading, so an
    // inner join would silently drop every room whose creator is unknown -- which on an admin page
    // is the difference between "no such room" and "nobody knows who made it".
    //
    // The scope is a `&'static str` from an enum, not a caller's string. Nothing user-supplied
    // reaches this text; the environment is still bound.
    let rooms: Vec<FleetRoom> = diesel::sql_query(format!(
        "SELECT r.id, r.name, r.state::text AS state, r.desired_state::text AS desired_state,
                r.created_by, u.username AS created_by_name, r.running_image,
                r.deployment_created_at, r.process_started_at, r.clients_connected,
                r.spec_hash, r.desired_spec_hash, r.redeploy_requested_at
           FROM rooms r
           LEFT JOIN users u ON u.id = r.created_by
          WHERE r.environment = $1::puna_environment
                {}
          ORDER BY (r.running_image IS NULL), r.name",
        scope.predicate()
    ))
    .bind::<Text, _>(environment.as_str())
    .load(conn)
    .await?;

    // Always counted, whatever the scope: the main page needs the number to label a section it is
    // deliberately not loading.
    let resting: Vec<Count> = diesel::sql_query(
        "SELECT count(*) AS n FROM rooms
          WHERE environment = $1::puna_environment
            AND desired_state IN ('stopped', 'closed')",
    )
    .bind::<Text, _>(environment.as_str())
    .load(conn)
    .await?;

    Ok(Overview {
        pahoa_image: configured.into_iter().next().map(|c| c.pahoa_image),
        rooms,
        // `into_iter().next()`, not `.first()`: diesel's `FirstDsl` is in scope and shadows the
        // slice method, and the resulting error is about `Table` rather than about this line.
        resting: resting.into_iter().next().map_or(0, |c| c.n),
    })
}

/// Ask for these rooms to be restarted onto their current spec.
///
/// Returns how many rows it actually marked. Idempotent by construction: a room that already has a
/// pending request keeps the **original** timestamp, so pressing the button twice cannot push a
/// room to the back of the queue the orchestrator drains in request order.
pub async fn request_redeploy(
    conn: &mut AsyncPgConnection,
    rooms: &[RoomId],
) -> Result<usize, diesel::result::Error> {
    if rooms.is_empty() {
        return Ok(0);
    }
    let marked = diesel::sql_query(
        "UPDATE rooms
            SET redeploy_requested_at = now()
          WHERE id = ANY($1)
            AND redeploy_requested_at IS NULL",
    )
    .bind::<Array<SqlUuid>, _>(rooms.to_vec())
    .execute(conn)
    .await?;
    Ok(marked)
}
