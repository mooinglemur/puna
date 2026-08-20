//! The reconcile tick.
//!
//! **The sweep IS the reconcile loop.** One level-triggered pass that reads the world, diffs it
//! against the desired state, and applies -- rather than an edge-triggered worker plus a reconciler
//! to catch the edges it missed. Two loops over one room is the shape that produces bugs nobody can
//! reproduce, so there is one.
//!
//! Being level-triggered is what makes everything else cheap: re-running a tick is a no-op,
//! "requested twice while starting" is not a special case, and a lost `NOTIFY` costs latency rather
//! than correctness. **`NOTIFY` is latency; the tick is the contract.**
//!
//! ```text
//! list the cluster (3 calls)  ->  load the rooms  ->  plan()  ->  execute each Step
//!                                                                 then the filesystem sweeps
//! ```
//!
//! The first three are [`crate::plan`], which is pure; the fourth is [`crate::steps`]. What stays
//! here is the part that is neither: reading the world, and the two filesystem checks where **a
//! failed read must not be mistaken for an answer** — a `readdir` that fails is not "every room's
//! directory is missing".
//!
//! ## Actions are applied one at a time
//!
//! The design allows eight concurrently. It is sequential here because the action list is short by
//! construction: a healthy room plans **no action at all**, so a namespace of three hundred running
//! rooms produces an empty list and a tick that is three list calls and one query. Concurrency is
//! worth adding when a measurement says a tick is too slow, not before — and every action takes a
//! per-room advisory lock, so the change is safe to make later.

use std::sync::Arc;
use std::time::Duration;

use puna_core::db::Pool;
use puna_core::ids::RoomId;
use puna_core::model::Orchestrator;
use puna_core::model::room::{DesiredState, RoomState};
use puna_core::{Environment, OrchestratorConfig};

use crate::cluster::ClusterApi;
use crate::plan::{self, RoomView};
use crate::probing::Prober;
use crate::spec::Site;
use crate::steps::{self, Outcome};
use crate::storage::{self, Layout};
use crate::sweep::Sweeper;

/// What one tick did, for the log line and the metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickReport {
    /// Rooms the planner considered.
    pub rooms: usize,
    pub actions: usize,
    pub skipped_locked: usize,
    pub errors: usize,
    /// Directories with no row. **Counted, never removed** -- see `report_orphan_dirs`.
    pub orphan_dirs: usize,
    pub integrity_faults: usize,
    /// Deployments removed because their room no longer exists, on their second sighting.
    pub orphans_deleted: usize,
    /// Orphans on their first strike. A number that stays above zero means the rule is not
    /// converging, which is worth seeing.
    pub orphans_pending: usize,
    pub secrets_refreshed: usize,
    pub trash_removed: usize,
    /// Live rooms asked how they are. See [`crate::probing`].
    pub probed: usize,
    pub probe_answers: usize,
    /// Rooms skipped because they asked to be left alone. Worth reporting rather than hiding: a
    /// number that stays above zero means something is exhausting a room's auth rate limit.
    pub probe_rate_limited: usize,
}

/// Abandoned provisioning attempts older than this are swept.
const TEMP_DIR_MAX_AGE: Duration = Duration::from_secs(3600);

/// Everything the tick needs, assembled once at startup.
pub struct Reconciler {
    pool: Pool,
    layout: Layout,
    site: Site,
    cluster: Arc<dyn ClusterApi>,
    environment: Environment,
    advertise_host: String,
    pahoa_image: String,
    sweeper: Sweeper,
    prober: Arc<Prober>,
}

impl Reconciler {
    /// The prober is shared with [`crate::dispatch::Dispatcher`] rather than built here: a stop
    /// and a console command must reach a room the same way, and two `Prober`s would also mean two
    /// rate-limit backoff tables — so a `429` seen by one would not stop the other walking into it.
    pub fn new(
        config: &OrchestratorConfig,
        pool: Pool,
        cluster: Arc<dyn ClusterApi>,
        prober: Arc<Prober>,
    ) -> Self {
        Self {
            pool,
            layout: Layout::new(&config.common.data_dir),
            site: Site {
                namespace: config.namespace.clone(),
                lb_ip: config.lb_ip.clone(),
                lb_sharing_key: config.lb_sharing_key.clone(),
                tls_secret: config.room_tls_secret.clone(),
                data_pvc: config.data_pvc.clone(),
            },
            cluster,
            environment: config.common.environment,
            advertise_host: config.common.advertise_host.clone(),
            pahoa_image: config.pahoa_image.clone(),
            sweeper: Sweeper::new(config.trash_retention),
            prober,
        }
    }

    /// One pass.
    ///
    /// Takes the [`LeaderLock`](crate::leader::LeaderLock) by reference rather than trusting a flag:
    /// holding one is what authorizes writing an observed column, and passing it in makes that
    /// visible at the call site rather than assumed inside.
    pub async fn tick(
        &self,
        lock: &crate::leader::LeaderLock,
        orchestrator: Orchestrator,
    ) -> anyhow::Result<TickReport> {
        let _timer = puna_core::metrics::RECONCILE_SECONDS.start_timer();
        let mut report = TickReport::default();

        // A leader whose session died must stop rather than finish the pass: another process may
        // already have taken the lock, and the second half of this tick would be a second actor.
        if !lock.is_held() {
            anyhow::bail!("the leader lock was lost; refusing to reconcile");
        }

        // Three calls regardless of room count, served from the watch cache. Read before the rooms
        // are loaded so a room created in between reads as "not there yet" rather than as vanished --
        // which the planner's grace period covers either way.
        let snapshot = match self.cluster.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                puna_core::metrics::RECONCILE_ERRORS
                    .with_label_values(&["snapshot"])
                    .inc();
                // Without a view of the cluster there is nothing honest to decide: every live room
                // would read as vanished. Give up on this pass and keep the filesystem sweeps for
                // the next one, which is 30 seconds away.
                return Err(anyhow::anyhow!("could not list the cluster: {e}"));
            }
        };

        let mut conn = self.pool.get().await?;
        let mut views = load_views(&mut conn, self.environment).await?;
        attach_desired_spec_hashes(&mut conn, &mut views, &self.pahoa_image).await;
        publish_room_states(&views);
        report.rooms = views.len();
        drop(conn);

        let actions = plan::plan(&views, &snapshot, chrono::Utc::now());
        report.actions = actions.len();

        let context = steps::Context {
            pool: &self.pool,
            cluster: self.cluster.as_ref(),
            layout: &self.layout,
            site: &self.site,
            environment: self.environment,
            advertise_host: &self.advertise_host,
            orchestrator,
            pahoa_image: &self.pahoa_image,
            probe: self.prober.probe(),
            room_route: self.prober.route(),
            probe_timeout: self.prober.timeout(),
        };

        for action in &actions {
            // Checked between actions, not just at the top: a pass over three hundred rooms can
            // outlive a lock, and a step taken after losing it is a second actor's step.
            if !lock.is_held() {
                anyhow::bail!("the leader lock was lost mid-tick; stopping");
            }

            match steps::execute(&context, action).await {
                Ok(Outcome::Done) => {}
                Ok(Outcome::SkippedLocked) => report.skipped_locked += 1,
                Err(e) => {
                    report.errors += 1;
                    puna_core::metrics::RECONCILE_ERRORS
                        .with_label_values(&["step"])
                        .inc();
                    // One room's failure is not the tick's: the loop is level-triggered, so the next
                    // pass sees the same world and tries again.
                    tracing::error!(
                        room = %action.room,
                        step = ?action.step,
                        error = ?e,
                        "step failed"
                    );
                }
            }
        }

        let mut conn = self.pool.get().await?;
        report.integrity_faults =
            detect_integrity_faults(&mut conn, &self.layout, orchestrator).await?;
        report.orphan_dirs = report_orphan_dirs(&mut conn, &self.layout).await?;

        // Everything that is about the world rather than one room: objects nothing owns, Secrets
        // that have drifted, the LRU touch, and -- once an hour -- the trash.
        let swept = self
            .sweeper
            .run(
                &mut conn,
                &crate::sweep::World {
                    cluster: self.cluster.as_ref(),
                    snapshot: &snapshot,
                    layout: &self.layout,
                    environment: self.environment,
                    orchestrator,
                },
            )
            .await;
        report.orphans_deleted = swept.orphans_deleted;
        report.orphans_pending = swept.orphans_pending;
        report.secrets_refreshed = swept.secrets_refreshed;
        report.trash_removed = swept.trash_removed;

        // **Last, and never able to fail the tick.** A room that will not answer its admin API may
        // still be serving a multiworld perfectly, so this only refreshes numbers -- it moves no
        // room's state and returns no error. Put after the sweep so a slow room cannot delay
        // anything that actually converges the world.
        self.prober.publish_capabilities();
        let probed = self.prober.run(&mut conn, self.environment).await;
        report.probed = probed.probed;
        report.probe_answers = probed.answered;
        report.probe_rate_limited = probed.rate_limited;

        match storage::sweep_temp_dirs(&self.layout, TEMP_DIR_MAX_AGE) {
            Ok(0) => {}
            Ok(n) => tracing::info!(removed = n, "swept abandoned provisioning directories"),
            Err(e) => tracing::warn!(error = ?e, "sweeping temp directories failed"),
        }

        Ok(report)
    }
}

/// Every room in this environment, as the planner reads them.
///
/// A room whose `state` or `desired_state` does not parse is **left out**, not defaulted: those
/// values come from a database that may be newer than this binary, and acting on a state this code
/// does not understand is worse than leaving the room alone and saying so.
async fn load_views(
    conn: &mut diesel_async::AsyncPgConnection,
    environment: Environment,
) -> Result<Vec<RoomView>, diesel::result::Error> {
    use diesel::sql_types::{Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
        #[diesel(sql_type = Integer)]
        lock_key: i32,
        #[diesel(sql_type = Text)]
        state: String,
        #[diesel(sql_type = Text)]
        desired_state: String,
        #[diesel(sql_type = Nullable<Text>)]
        spec_hash: Option<String>,
        #[diesel(sql_type = Timestamptz)]
        state_changed_at: chrono::DateTime<chrono::Utc>,
        #[diesel(sql_type = Nullable<Timestamptz>)]
        retry_after: Option<chrono::DateTime<chrono::Utc>>,
        #[diesel(sql_type = Integer)]
        not_ready_sweeps: i32,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT id, lock_key, state::text AS state, desired_state::text AS desired_state,
                spec_hash, state_changed_at, retry_after, not_ready_sweeps
           FROM rooms
          WHERE environment = $1::puna_environment
          ORDER BY created_at",
    )
    .bind::<Text, _>(environment.as_str())
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let Some(state) = RoomState::parse(&row.state) else {
                tracing::error!(
                    room = %row.id,
                    state = %row.state,
                    "unknown room state; leaving this room alone. Is the database newer than this \
                     orchestrator?"
                );
                return None;
            };
            let Some(desired) = DesiredState::parse(&row.desired_state) else {
                tracing::error!(
                    room = %row.id,
                    desired = %row.desired_state,
                    "unknown desired state; leaving this room alone"
                );
                return None;
            };

            Some(RoomView {
                id: row.id,
                lock_key: row.lock_key,
                state,
                desired,
                spec_hash: row.spec_hash,
                // Filled in afterwards, for the few rooms it can change a decision for. See
                // `attach_desired_spec_hashes`.
                desired_spec_hash: None,
                state_changed_at: row.state_changed_at,
                retry_after: row.retry_after,
                not_ready_sweeps: row.not_ready_sweeps,
            })
        })
        .collect())
}

/// Fill in [`RoomView::desired_spec_hash`] for the rooms whose decision it can change.
///
/// **Only rooms in `failed` that are still waiting**, and the narrowness is the design rather than
/// an optimization. Rendering a spec costs a room's row, its secrets, its slot list and its
/// reservation — four queries — so doing it for every room on every tick would put a per-room cost
/// on every pass to answer a question exactly one state asks. A room whose backoff has already
/// expired is skipped too: it is about to be retried regardless, so the answer could not change
/// anything.
///
/// A room this leaves at `None` is a room the planner will decide about on its timer alone, which
/// is the behavior that predates this and is always safe.
async fn attach_desired_spec_hashes(
    conn: &mut diesel_async::AsyncPgConnection,
    views: &mut [RoomView],
    pahoa_image: &str,
) {
    let now = chrono::Utc::now();

    for view in views.iter_mut() {
        let waiting = view.state == RoomState::Failed
            && view.desired == DesiredState::Running
            // `None` counts as waiting: a failure with no backoff recorded waits for a person, and
            // an operator changing the image *is* that person.
            && view.retry_after.is_none_or(|after| after > now);

        if waiting {
            view.desired_spec_hash = steps::desired_spec_hash(conn, pahoa_image, view.id).await;
        }
    }
}

/// `puna_rooms{state}`, from the same read the planner used.
///
/// Every state is reset first, so a state that has just emptied publishes a zero rather than keeping
/// its last value forever -- a gauge that only ever goes up is worse than no gauge.
fn publish_room_states(views: &[RoomView]) {
    for state in puna_core::metrics::ROOM_STATES {
        puna_core::metrics::ROOMS.with_label_values(&[state]).set(0);
    }
    for view in views {
        puna_core::metrics::ROOMS
            .with_label_values(&[view.state.as_sql()])
            .inc();
    }
}

/// Count room directories with no row, and name them in the log.
///
/// **Reported, never deleted.** A directory with no row is either a bug or a database restored from
/// an older backup, and in the second case the directory holds the only copy of a player's progress
/// -- deleting it would destroy exactly the state that could repair the room.
/// `PUNA_ORPHAN_DELETE_AFTER` exists in the design and defaults to disabled for this reason.
async fn report_orphan_dirs(
    conn: &mut diesel_async::AsyncPgConnection,
    layout: &Layout,
) -> anyhow::Result<usize> {
    use diesel::sql_types::Uuid as SqlUuid;
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
    }

    let on_disk = storage::list_room_dirs(layout)?;
    if on_disk.is_empty() {
        puna_core::metrics::ORPHAN_DIRECTORIES.set(0);
        return Ok(0);
    }

    let known: std::collections::HashSet<RoomId> = diesel::sql_query("SELECT id FROM rooms")
        .load::<Row>(conn)
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect();

    let orphans: Vec<RoomId> = on_disk
        .into_iter()
        .filter(|id| !known.contains(id))
        .collect();

    if !orphans.is_empty() {
        tracing::warn!(
            count = orphans.len(),
            rooms = ?orphans.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "room directories with no database row. NOT removed: if this database was restored \
             from a backup, these hold the only copy of that progress."
        );
    }
    puna_core::metrics::ORPHAN_DIRECTORIES.set(orphans.len() as i64);
    Ok(orphans.len())
}

/// Rooms whose row claims a directory that is not there.
///
/// **Never auto-repaired.** Recreating the directory would replace a player's progress with an empty
/// room and look like a successful start, which is the one failure mode worth a loud, terminal state
/// instead of a retry.
///
/// Deliberately not part of [`crate::plan`]: it is a filesystem property, and handing the planner a
/// third view of the world whose *absence* -- a failed `readdir` -- would read as every room being
/// faulted at once is not a trade worth making.
async fn detect_integrity_faults(
    conn: &mut diesel_async::AsyncPgConnection,
    layout: &Layout,
    _orchestrator: Orchestrator,
) -> Result<usize, diesel::result::Error> {
    use diesel::sql_types::Uuid as SqlUuid;
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT id FROM rooms
          WHERE provisioned_at IS NOT NULL
            AND state NOT IN ('deleting', 'integrity_fault')
            AND desired_state <> 'deleted'",
    )
    .load(conn)
    .await?;

    let mut faults = 0;
    for row in rows {
        if storage::room_exists(layout, row.id) {
            continue;
        }

        faults += 1;
        tracing::error!(
            room = %row.id,
            "INTEGRITY FAULT: provisioned_at is set but the room directory is missing. This is \
             not auto-repaired -- recreating it would replace saved progress with an empty room."
        );

        diesel::sql_query(
            "UPDATE rooms SET state = 'integrity_fault', state_changed_at = now() WHERE id = $1",
        )
        .bind::<SqlUuid, _>(row.id)
        .execute(conn)
        .await?;

        diesel::sql_query(
            "INSERT INTO room_events (room_id, actor, kind) VALUES ($1, 'reconcile', 'integrity_fault')",
        )
        .bind::<SqlUuid, _>(row.id)
        .execute(conn)
        .await?;
    }

    puna_core::metrics::INTEGRITY_FAULTS.set(faults as i64);
    Ok(faults)
}
