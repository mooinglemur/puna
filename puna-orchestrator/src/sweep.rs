//! The parts of a tick that are about the world rather than about one room.
//!
//! Everything here is cleanup or bookkeeping: objects nothing owns, credentials that have drifted,
//! directories nothing references, and the numbers that let somebody see all of it from a dashboard.
//! None of it is on a room's critical path, which is why a failure in any of it is logged and
//! stepped over rather than failing the tick.
//!
//! ## Deleting is the dangerous half, so almost nothing here deletes
//!
//! Three rules, each learned from a different way this can go wrong:
//!
//!   * **Orphaned Deployments take two strikes and two minutes.** The row is always committed
//!     before the Deployment is created, so an orphan is real — *except* on a fresh leader's first
//!     tick, which can read a stale list. Requiring the same object on two consecutive ticks makes
//!     that impossible to act on.
//!   * **Only objects whose room has no row at all are collected.** An object whose owner is gone
//!     but whose room still exists is *reported*, not deleted, because that shape also describes a
//!     start in flight: the Secret is applied unowned before the Deployment exists, and deleting it
//!     there would break the room the sweep was trying to tidy up after.
//!   * **Directories are never deleted, only counted.** A room directory with no row is either a
//!     bug or a database restored from a backup, and in the second case it holds the only copy of
//!     that progress.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use diesel::sql_types::{Text, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use puna_core::Environment;
use puna_core::ids::RoomId;
use puna_core::model::{Orchestrator, port, room, slot};

use crate::cluster::{ClusterApi, ClusterSnapshot, OwnerRef, SecretSpec};
use crate::spec;
use crate::storage::{self, Layout};

/// An orphan must be seen on two consecutive ticks **and** be older than this.
///
/// The age is what covers the gap the two-strike rule cannot: a Deployment created between one
/// tick's list and the next's could otherwise be an orphan twice over before its room's row is
/// visible to a stale read.
const ORPHAN_MIN_AGE: Duration = Duration::from_secs(120);

/// A room's Secret is re-applied at least this often even when nothing has changed.
///
/// The contract is `secret_synced_at IS NULL` meaning "needs a re-apply" — set by whatever changes
/// a credential — and this interval is the backstop for a writer that forgot, not the mechanism.
const SECRET_REFRESH: chrono::TimeDelta = chrono::TimeDelta::hours(1);

/// How often the expensive lane runs.
const SLOW_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    pub orphans_deleted: usize,
    pub orphans_pending: usize,
    pub secrets_refreshed: usize,
    pub trash_removed: usize,
}

/// What the sweep needs from the tick that made it.
///
/// A struct rather than five parameters, and it borrows rather than owning: the tick already holds
/// every one of these, and the snapshot in particular is the whole world.
pub struct World<'a> {
    pub cluster: &'a dyn ClusterApi,
    pub snapshot: &'a ClusterSnapshot,
    pub layout: &'a Layout,
    pub environment: Environment,
    pub orchestrator: Orchestrator,
}

/// The sweep's memory between ticks.
pub struct Sweeper {
    /// Object names seen orphaned on the previous tick. The second strike.
    seen_orphans: Mutex<HashSet<String>>,
    last_slow: Mutex<Option<Instant>>,
    trash_retention: Duration,
}

impl Sweeper {
    pub fn new(trash_retention: Duration) -> Self {
        Self {
            seen_orphans: Mutex::new(HashSet::new()),
            last_slow: Mutex::new(None),
            trash_retention,
        }
    }

    /// The per-tick lane.
    pub async fn run(&self, conn: &mut AsyncPgConnection, world: &World<'_>) -> SweepReport {
        let World {
            cluster,
            snapshot,
            layout,
            environment,
            orchestrator,
        } = *world;
        let mut report = SweepReport::default();

        let known = match room_ids(conn).await {
            Ok(known) => known,
            Err(e) => {
                // Without the row set, everything looks orphaned. Doing nothing is the only safe
                // reading of a failed read.
                tracing::warn!(error = ?e, "could not read the room list; skipping the sweep");
                return report;
            }
        };

        let (deleted, pending) = self.collect_orphans(cluster, snapshot, &known).await;
        report.orphans_deleted = deleted;
        report.orphans_pending = pending;

        // LRU means "least recently used", and a running room is being used. Without this the order
        // degrades to "least recently started", which would reclaim a busy room's port ahead of an
        // idle one that happened to start later.
        if let Err(e) = port::touch_live_rooms(&orchestrator, conn, environment).await {
            tracing::warn!(error = ?e, "could not touch live rooms' activity");
        }

        report.secrets_refreshed = self.refresh_secrets(conn, cluster, snapshot).await;
        publish_port_stats(conn, environment).await;

        if self.slow_lane_due() {
            report.trash_removed = self.slow_lane(conn, layout).await;
        }

        report
    }

    /// Deployments Puna manages whose room does not exist.
    ///
    /// Returns `(deleted, pending)` — pending being the ones on their first strike, which is worth
    /// reporting because a number that stays above zero means the rule is not converging.
    async fn collect_orphans(
        &self,
        cluster: &dyn ClusterApi,
        snapshot: &ClusterSnapshot,
        known: &HashSet<RoomId>,
    ) -> (usize, usize) {
        let now = Utc::now();
        let orphans: Vec<&crate::cluster::RoomDeployment> = snapshot
            .deployments
            .iter()
            .filter(|deployment| {
                deployment
                    .room_id
                    .is_none_or(|room_id| !known.contains(&room_id))
            })
            .collect();

        let (mut deleted, mut pending) = (0, 0);
        let previously: HashSet<String> = self
            .seen_orphans
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default();

        let mut still_orphaned = HashSet::new();
        for deployment in orphans {
            still_orphaned.insert(deployment.name.clone());

            let old_enough = (now - deployment.created_at)
                .to_std()
                .is_ok_and(|age| age >= ORPHAN_MIN_AGE);

            if !previously.contains(&deployment.name) || !old_enough {
                pending += 1;
                tracing::info!(
                    deployment = %deployment.name,
                    room = ?deployment.room_id,
                    old_enough,
                    "a Deployment with no room; waiting for a second sighting before removing it"
                );
                continue;
            }

            tracing::warn!(
                deployment = %deployment.name,
                room = ?deployment.room_id,
                "removing a Deployment whose room no longer exists"
            );
            match cluster.delete_deployment(&deployment.name).await {
                Ok(()) => deleted += 1,
                Err(e) => tracing::error!(
                    deployment = %deployment.name,
                    error = ?e,
                    "could not remove an orphaned Deployment"
                ),
            }
        }

        report_unowned(snapshot, known);

        if let Ok(mut seen) = self.seen_orphans.lock() {
            *seen = still_orphaned;
        }
        (deleted, pending)
    }

    /// Re-apply the Secret for rooms that asked for it, or have not had one in a while.
    async fn refresh_secrets(
        &self,
        conn: &mut AsyncPgConnection,
        cluster: &dyn ClusterApi,
        snapshot: &ClusterSnapshot,
    ) -> usize {
        let stale = match stale_secret_rooms(conn).await {
            Ok(rooms) => rooms,
            Err(e) => {
                tracing::warn!(error = ?e, "could not list rooms with stale Secrets");
                return 0;
            }
        };

        let mut refreshed = 0;
        for room_id in stale {
            // Only for rooms whose Deployment exists: the Secret of a room that is not running is
            // written by the next start, and applying one now would leave an unowned object behind
            // for the sweep above to puzzle over.
            let Some(deployment) = snapshot.deployment(room_id) else {
                continue;
            };

            let Ok(Some(room)) = room::get(conn, room_id).await else {
                continue;
            };
            let Ok(Some(secrets)) = room::secrets(conn, room_id).await else {
                continue;
            };
            let Ok(slots) = slot::list(conn, room_id).await else {
                continue;
            };

            // The same fail-closed builder the start path uses. A room whose slot passwords have
            // gone incomplete keeps the Secret it has rather than being handed one that locks a
            // player out.
            let data = match spec::secret::build(&room, &secrets, &slots) {
                Ok(data) => data,
                Err(e) => {
                    tracing::error!(
                        room = %room_id,
                        error = %e,
                        "refusing to refresh this room's Secret; the old one is left in place"
                    );
                    continue;
                }
            };

            let spec = SecretSpec {
                room_id,
                data,
                owner: Some(OwnerRef {
                    name: deployment.name.clone(),
                    uid: deployment.uid.clone(),
                }),
            };
            match cluster.apply_secret(&spec).await {
                Ok(()) => {
                    if mark_secret_synced(conn, room_id).await.is_ok() {
                        refreshed += 1;
                    }
                }
                Err(e) => tracing::warn!(room = %room_id, error = ?e, "could not refresh a Secret"),
            }
        }
        refreshed
    }

    fn slow_lane_due(&self) -> bool {
        let mut last = match self.last_slow.lock() {
            Ok(last) => last,
            Err(_) => return false,
        };
        match *last {
            Some(at) if at.elapsed() < SLOW_INTERVAL => false,
            _ => {
                *last = Some(Instant::now());
                true
            }
        }
    }

    /// The hourly lane: expired trash, and a count of generations nothing references.
    async fn slow_lane(&self, conn: &mut AsyncPgConnection, layout: &Layout) -> usize {
        let removed = match storage::sweep_trash(layout, self.trash_retention) {
            Ok(removed) => {
                if removed > 0 {
                    tracing::info!(removed, "removed expired room directories from the trash");
                }
                removed
            }
            Err(e) => {
                tracing::warn!(error = ?e, "sweeping the trash failed");
                0
            }
        };

        // Counted, never removed. A generation is content-addressed and shared, so reclaiming one
        // is an admin action with a listing in front of it -- not something a sweep decides.
        match reclaimable_generations(conn, layout).await {
            Ok(0) => {}
            Ok(count) => tracing::info!(
                count,
                "generation directories no room references; reclaimable by an administrator"
            ),
            Err(e) => tracing::warn!(error = ?e, "could not count reclaimable generations"),
        }

        removed
    }
}

/// Services and Secrets whose owner is gone but whose room still exists.
///
/// **Reported, not deleted.** That shape is also what a start in flight looks like: §7 applies the
/// Secret unowned before the Deployment exists, so deleting on this signal would break the room the
/// sweep is tidying up after. What it means when the room is *not* starting is that an
/// ownerReference was written without a uid, which is a bug worth an error line.
fn report_unowned(snapshot: &ClusterSnapshot, known: &HashSet<RoomId>) {
    let live: HashSet<&str> = snapshot
        .deployments
        .iter()
        .map(|deployment| deployment.uid.as_str())
        .collect();

    let unowned_services = snapshot.services.iter().filter(|service| {
        service
            .owner_uid
            .as_deref()
            .is_none_or(|uid| !live.contains(uid))
    });
    for service in unowned_services {
        tracing::error!(
            service = %service.name,
            room = ?service.room_id,
            known_room = service.room_id.is_some_and(|id| known.contains(&id)),
            "a Service whose owning Deployment is gone. If this room is not starting, its \
             ownerReference was written without a uid and nothing will ever collect it."
        );
    }
}

async fn room_ids(conn: &mut AsyncPgConnection) -> Result<HashSet<RoomId>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
    }
    let rows: Vec<Row> = diesel::sql_query("SELECT id FROM rooms").load(conn).await?;
    Ok(rows.into_iter().map(|row| row.id).collect())
}

/// Rooms whose Secret is stale: never synced, or synced long enough ago to be worth refreshing.
///
/// `secret_synced_at IS NULL` is the **contract**, not an initial state: whatever changes a
/// credential nulls it, and this picks the room up on the next tick.
async fn stale_secret_rooms(
    conn: &mut AsyncPgConnection,
) -> Result<Vec<RoomId>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT id FROM rooms
          WHERE state IN ('starting', 'running', 'degraded')
            AND (secret_synced_at IS NULL OR secret_synced_at < now() - $1::interval)",
    )
    .bind::<Text, _>(format!("{} seconds", SECRET_REFRESH.num_seconds()))
    .load(conn)
    .await?;

    Ok(rows.into_iter().map(|row| row.id).collect())
}

async fn mark_secret_synced(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query("UPDATE rooms SET secret_synced_at = now() WHERE id = $1")
        .bind::<SqlUuid, _>(room)
        .execute(conn)
        .await?;
    Ok(())
}

/// Generation directories on disk that no `generations` row names.
async fn reclaimable_generations(
    conn: &mut AsyncPgConnection,
    layout: &Layout,
) -> anyhow::Result<usize> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        sha: String,
    }

    let on_disk = storage::list_generation_dirs(layout)?;
    if on_disk.is_empty() {
        return Ok(0);
    }

    let known: HashSet<String> =
        diesel::sql_query("SELECT encode(sha256, 'hex') AS sha FROM generations")
            .load::<Row>(conn)
            .await?
            .into_iter()
            .map(|row| row.sha)
            .collect();

    Ok(on_disk.iter().filter(|sha| !known.contains(*sha)).count())
}

/// The port gauges, from one aggregate query.
async fn publish_port_stats(conn: &mut AsyncPgConnection, environment: Environment) {
    match port::stats(conn, environment).await {
        Ok(stats) => {
            puna_core::metrics::PORTS_TOTAL.set(stats.total);
            puna_core::metrics::PORTS_BOUND.set(stats.bound);
            puna_core::metrics::PORTS_QUARANTINED.set(stats.quarantined);
        }
        Err(e) => tracing::warn!(error = ?e, "could not read port statistics"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{RoomDeployment, RoomService};

    fn deployment(name: &str, room: Option<RoomId>, age: Duration) -> RoomDeployment {
        RoomDeployment {
            name: name.to_string(),
            uid: format!("uid-{name}"),
            room_id: room,
            spec_hash: Some("hash".into()),
            replicas: 1,
            ready_replicas: 1,
            created_at: Utc::now() - chrono::TimeDelta::from_std(age).expect("an age"),
        }
    }

    /// The two-strike rule, without a cluster or a database: an orphan is not acted on until it has
    /// been an orphan twice, which is what makes a fresh leader's stale first read harmless.
    #[tokio::test]
    async fn an_orphan_survives_its_first_sighting() {
        let cluster = crate::cluster::fake::FakeCluster::new();
        let sweeper = Sweeper::new(Duration::from_secs(60));
        let gone = RoomId::new();
        let snapshot = ClusterSnapshot {
            deployments: vec![deployment("mw-gone", Some(gone), Duration::from_secs(600))],
            ..Default::default()
        };
        let known = HashSet::new();

        let (deleted, pending) = sweeper.collect_orphans(&cluster, &snapshot, &known).await;
        assert_eq!((deleted, pending), (0, 1), "first sighting");

        let (deleted, pending) = sweeper.collect_orphans(&cluster, &snapshot, &known).await;
        assert_eq!((deleted, pending), (1, 0), "second sighting");
        assert_eq!(cluster.ops(), [crate::cluster::fake::Op::DeleteDeployment]);
    }

    /// The age rule covers what the strike rule cannot: an object created between two lists could
    /// otherwise be an orphan twice before the row that explains it is visible.
    #[tokio::test]
    async fn a_young_orphan_is_never_deleted_however_often_it_is_seen() {
        let cluster = crate::cluster::fake::FakeCluster::new();
        let sweeper = Sweeper::new(Duration::from_secs(60));
        let snapshot = ClusterSnapshot {
            deployments: vec![deployment("mw-new", None, Duration::from_secs(5))],
            ..Default::default()
        };
        let known = HashSet::new();

        for _ in 0..5 {
            let (deleted, pending) = sweeper.collect_orphans(&cluster, &snapshot, &known).await;
            assert_eq!((deleted, pending), (0, 1));
        }
        assert!(cluster.ops().is_empty(), "nothing was deleted");
    }

    /// A room that exists is not an orphan, however old its Deployment is.
    #[tokio::test]
    async fn a_deployment_whose_room_exists_is_left_alone() {
        let cluster = crate::cluster::fake::FakeCluster::new();
        let sweeper = Sweeper::new(Duration::from_secs(60));
        let room = RoomId::new();
        let snapshot = ClusterSnapshot {
            deployments: vec![deployment(
                "mw-live",
                Some(room),
                Duration::from_secs(86_400),
            )],
            services: vec![RoomService {
                name: "mw-live".into(),
                room_id: Some(room),
                ingress_ip: Some("38.246.56.121".into()),
                owner_uid: Some("uid-mw-live".into()),
            }],
            ..Default::default()
        };
        let known = HashSet::from([room]);

        for _ in 0..3 {
            assert_eq!(
                sweeper.collect_orphans(&cluster, &snapshot, &known).await,
                (0, 0)
            );
        }
        assert!(cluster.ops().is_empty());
    }

    /// The slow lane runs once and then not again for an hour, so an expensive `readdir` cannot end
    /// up on every thirty-second tick.
    #[test]
    fn the_slow_lane_runs_once_an_hour() {
        let sweeper = Sweeper::new(Duration::from_secs(60));
        assert!(sweeper.slow_lane_due(), "the first tick runs it");
        assert!(!sweeper.slow_lane_due());
        assert!(!sweeper.slow_lane_due());
    }
}
