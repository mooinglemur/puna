//! Asking every live room how it is, once a tick.
//!
//! This is the only thing that can answer "how many clients, how long idle" — the Kubernetes view
//! stops at "the pod is ready", which a room with nobody in it satisfies just as well as a busy one.
//!
//! ## Why the orchestrator polls rather than Prometheus scraping each room
//!
//! Settled at M11. pahoa serves `/admin/v1/metrics` bearer-gated and this cluster discovers
//! ServiceMonitors from every namespace, so a ServiceMonitor per room would work — and was rejected
//! for three reasons, the first deciding. **Prometheus config churn would be driven by player
//! behavior**: every ServiceMonitor add or delete regenerates the whole scrape config, and Puna's
//! rooms churn precisely because the design is that they idle out and come back on a URL hit. That
//! makes reload pressure a function of how many people opened a room today, over a range that
//! allows 2500 of them.
//!
//! The second reason is this module's own hazard, so it is worth stating here: pahoa rate-limits
//! authentication failures to **10 a minute per room, and the lockout applies to the correct token
//! too** — deliberately, so it cannot be used as an oracle. A second credential-holder scraping
//! twice a minute could lock the orchestrator out of its own rooms, and it would present as "the
//! console stopped working" with nothing pointing at monitoring.
//!
//! ## What that costs, stated rather than discovered
//!
//! Room numbers go dark when the orchestrator is down, where a ServiceMonitor would keep scraping.
//! Acceptable: these are diagnostics, and a downed orchestrator is already the thing being
//! investigated.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use diesel::sql_types::{Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use futures_util::stream::{self, StreamExt};
use puna_core::Environment;
use puna_core::ids::RoomId;
use puna_core::probe::{ProbeError, RoomProbe, RoomStatus};
use puna_core::room::{RoomEndpoint, Route};

/// How many rooms are probed at once.
///
/// Bounded because a probe is a network round trip with a timeout, and a namespace of two hundred
/// rooms probed serially would take longer than the tick that started it. Bounded *low* because the
/// point is to finish, not to be fast: this runs beside the reconcile pass, and a burst of
/// connections is the one way a diagnostic could disturb the thing it is diagnosing.
const CONCURRENCY: usize = 8;

/// How long a rate-limited room is left alone when it did not say.
///
/// pahoa's limiter is per minute, so a minute is the honest default for a `429` with no
/// `Retry-After` — and erring long is right here, because the failure mode of erring short is
/// extending the lockout that is already in force.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProbeReport {
    pub probed: usize,
    pub answered: usize,
    pub rate_limited: usize,
    /// Rooms skipped because they are still inside a `Retry-After` a room asked for.
    pub backing_off: usize,
}

/// One live room, and what it takes to reach it.
struct Target {
    id: RoomId,
    base_port: u16,
    admin_token: String,
}

pub struct Prober {
    probe: Box<dyn RoomProbe>,
    route: Route,
    advertise_host: String,
    timeout: Duration,
    /// **Do not touch this room before this instant.** The rate-limit hazard in the module docs,
    /// made structural: a `429` parks the room here and the next tick skips it, so a reconciler
    /// cannot spend its own lockout window re-triggering it.
    ///
    /// In memory rather than on the row: it is advice measured in seconds, and a restarted
    /// orchestrator probing once more is a smaller cost than a column that has to be maintained.
    quiet_until: Mutex<HashMap<RoomId, DateTime<Utc>>>,
}

impl Prober {
    pub fn new(
        probe: Box<dyn RoomProbe>,
        route: Route,
        advertise_host: String,
        timeout: Duration,
    ) -> Self {
        Self {
            probe,
            route,
            advertise_host,
            timeout,
            quiet_until: Mutex::new(HashMap::new()),
        }
    }

    /// Lent to the step context, so a stop dials rooms exactly as the probe pass does.
    pub fn probe(&self) -> &dyn RoomProbe {
        self.probe.as_ref()
    }

    pub fn route(&self) -> &Route {
        &self.route
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn endpoint(&self, room: RoomId, base_port: u16) -> RoomEndpoint {
        RoomEndpoint {
            room,
            base_port,
            advertise_host: self.advertise_host.clone(),
            route: self.route.clone(),
            timeout: self.timeout,
        }
    }

    /// Probe every live room and write what came back.
    pub async fn run(&self, conn: &mut AsyncPgConnection, environment: Environment) -> ProbeReport {
        let mut report = ProbeReport::default();

        let targets = match live_rooms(conn, environment).await {
            Ok(targets) => targets,
            Err(e) => {
                tracing::warn!(error = ?e, "could not list live rooms; skipping the probe pass");
                return report;
            }
        };

        // **Reconcile the published series against the live set, every tick.** A `GaugeVec` keyed
        // by room keeps a series forever unless it is removed, so without this every room that ever
        // ran would leave one behind asserting its last client count -- and a stale gauge reads as a
        // live room, which is worse than no metric. Done here rather than on each transition
        // because there are four ways to stop being live and a hook per path is one somebody
        // forgets. Rooms that are live but backing off keep their series: they are still rooms.
        let live: std::collections::HashSet<String> =
            targets.iter().map(|t| t.id.to_string()).collect();
        puna_core::metrics::retain_rooms(&live);

        let now = Utc::now();
        let (ready, waiting): (Vec<_>, Vec<_>) =
            targets.into_iter().partition(|t| self.may_probe(t.id, now));
        report.backing_off = waiting.len();
        report.probed = ready.len();

        // Concurrent, but the RESULTS are applied serially: they share one connection, and a
        // per-room connection would be a pool the size of the room count for work that is not
        // urgent.
        let answers: Vec<(RoomId, Result<RoomStatus, ProbeError>)> = stream::iter(ready)
            .map(|target| async move {
                let endpoint = self.endpoint(target.id, target.base_port);
                let answer = self.probe.status(&endpoint, &target.admin_token).await;
                (target.id, answer)
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;

        for (room, answer) in answers {
            match answer {
                Ok(status) => {
                    report.answered += 1;
                    self.clear_backoff(room);
                    if let Err(e) = record(conn, room, &status, self.probe_kind()).await {
                        tracing::warn!(%room, error = ?e, "could not record a probe result");
                    }
                    puna_core::metrics::publish_room(&room.to_string(), &status);
                }

                Err(e)
                    if matches!(
                        e,
                        ProbeError::Room(puna_core::room::RoomError::RateLimited { .. })
                    ) =>
                {
                    report.rate_limited += 1;
                    let wait = e.retry_after().unwrap_or(DEFAULT_BACKOFF);
                    self.back_off(room, wait);
                    tracing::warn!(
                        %room,
                        wait_seconds = wait.as_secs(),
                        "the room is rate limiting the admin API; not probing it again until the \
                         window passes. If this persists, something else is authenticating against \
                         this room with a stale token."
                    );
                }

                Err(e) => {
                    // A probe failure is not a room failure. A room that will not answer its admin
                    // API may still be serving a multiworld perfectly, so this NEVER moves the
                    // room's state -- it only leaves the numbers unrefreshed, which the `probed_at`
                    // stamp makes visible.
                    tracing::debug!(%room, error = %e, "a room did not answer the probe");
                }
            }
        }

        report
    }

    /// Publish what this probe can do, once at startup.
    ///
    /// The family has existed since M9 with nothing writing it. It is worth writing because the
    /// degraded mode is otherwise invisible: under the TCP fallback the console is hidden and the
    /// numbers are blank, which looks like a quiet room rather than a room Puna cannot talk to.
    pub fn publish_capabilities(&self) {
        puna_core::metrics::publish_probe_capabilities(&self.probe.capabilities());
    }

    fn probe_kind(&self) -> &'static str {
        if self.probe.capabilities().status {
            "https"
        } else {
            "tcp"
        }
    }

    fn may_probe(&self, room: RoomId, now: DateTime<Utc>) -> bool {
        self.quiet_until
            .lock()
            .ok()
            .and_then(|q| q.get(&room).copied())
            .is_none_or(|until| until <= now)
    }

    fn back_off(&self, room: RoomId, wait: Duration) {
        let until =
            Utc::now() + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::minutes(1));
        if let Ok(mut quiet) = self.quiet_until.lock() {
            quiet.insert(room, until);
        }
    }

    fn clear_backoff(&self, room: RoomId) {
        if let Ok(mut quiet) = self.quiet_until.lock() {
            quiet.remove(&room);
        }
    }
}

/// Rooms worth asking: up, with a port, and with a token to ask with.
///
/// `starting` is deliberately absent. A room that has not reached ready has nothing to report and
/// would answer with a connection refused, which is noise rather than information — readiness is
/// the Deployment's job and the planner already reads it.
async fn live_rooms(
    conn: &mut AsyncPgConnection,
    environment: Environment,
) -> Result<Vec<Target>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
        #[diesel(sql_type = Integer)]
        base_port: i32,
        #[diesel(sql_type = Text)]
        admin_token: String,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT r.id, p.base_port, r.admin_token
           FROM rooms r
           JOIN port_reservations p ON p.room_id = r.id
          WHERE r.environment = $1::puna_environment
            AND r.state IN ('running', 'degraded')",
    )
    .bind::<Text, _>(environment.as_str())
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Target {
                id: row.id,
                base_port: u16::try_from(row.base_port).ok()?,
                admin_token: row.admin_token,
            })
        })
        .collect())
}

/// Write what the room said onto its row.
///
/// **`None` is written as `NULL`, never as zero**, which is the whole contract of these columns: a
/// probe that cannot tell must not be indistinguishable from a room with nobody in it. That is why
/// this takes `Option`s straight through rather than defaulting them.
async fn record(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    status: &RoomStatus,
    kind: &'static str,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "UPDATE rooms
            SET clients_connected = $2,
                last_activity_at = $3,
                probed_at = now(),
                probe_kind = $4
          WHERE id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Nullable<Integer>, _>(
        status
            .net
            .clients_connected
            .and_then(|n| i32::try_from(n).ok()),
    )
    .bind::<Nullable<Timestamptz>, _>(status.activity.last_client_message_at)
    .bind::<Text, _>(kind)
    .execute(conn)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use puna_core::probe::{ProbeCapabilities, TcpProbe};

    fn prober(probe: Box<dyn RoomProbe>) -> Prober {
        Prober::new(
            probe,
            Route::Public,
            "mw.example".into(),
            Duration::from_secs(1),
        )
    }

    /// The rate-limit guard, which is the thing this module exists to get right: a room that said
    /// "wait" is not asked again until it has waited.
    #[test]
    fn a_rate_limited_room_is_left_alone_until_its_window_passes() {
        let prober = prober(Box::new(TcpProbe));
        let room = RoomId::new();
        let now = Utc::now();

        assert!(prober.may_probe(room, now), "nothing said to wait yet");

        prober.back_off(room, Duration::from_secs(60));
        assert!(!prober.may_probe(room, now), "the window is open");
        assert!(
            prober.may_probe(room, now + chrono::Duration::seconds(61)),
            "and it expires rather than parking the room forever"
        );

        // A room that answers clears its own backoff, so one 429 does not outlive the condition.
        prober.clear_backoff(room);
        assert!(prober.may_probe(room, now));
    }

    /// The recorded `probe_kind` follows what the probe can actually do, so a room stuck on an old
    /// image is visible as `tcp` on its row rather than being reported as a full status that is
    /// entirely null.
    #[test]
    fn the_recorded_kind_reflects_the_probe_in_use() {
        assert_eq!(prober(Box::new(TcpProbe)).probe_kind(), "tcp");
        assert_eq!(
            prober(Box::new(puna_core::probe::HttpsProbe)).probe_kind(),
            "https"
        );

        // The mapping is on capability rather than on a name, so a future probe that can report
        // status is not silently filed as the fallback.
        assert!(!TcpProbe.capabilities().status);
        assert_eq!(
            puna_core::probe::HttpsProbe.capabilities(),
            ProbeCapabilities {
                status: true,
                commands: true,
                graceful_shutdown: true
            }
        );
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::storage::Layout;
    use crate::testdb::{self, NewRoom};
    use puna_core::probe::{ActivityStatus, NetStatus};

    async fn bind(conn: &mut AsyncPgConnection, room: RoomId, base_port: i32) {
        diesel::sql_query(
            "UPDATE port_reservations SET room_id = $1
              WHERE environment = 'dev' AND base_port = $2",
        )
        .bind::<SqlUuid, _>(room)
        .bind::<Integer, _>(base_port)
        .execute(conn)
        .await
        .expect("bind a pair");
    }

    #[derive(diesel::QueryableByName)]
    struct Observed {
        #[diesel(sql_type = Nullable<Integer>)]
        clients_connected: Option<i32>,
        #[diesel(sql_type = Nullable<Timestamptz>)]
        last_activity_at: Option<DateTime<Utc>>,
        #[diesel(sql_type = Nullable<Timestamptz>)]
        probed_at: Option<DateTime<Utc>>,
        #[diesel(sql_type = Nullable<Text>)]
        probe_kind: Option<String>,
    }

    async fn observed(conn: &mut AsyncPgConnection, room: RoomId) -> Observed {
        diesel::sql_query(
            "SELECT clients_connected, last_activity_at, probed_at, probe_kind
               FROM rooms WHERE id = $1",
        )
        .bind::<SqlUuid, _>(room)
        .load::<Observed>(conn)
        .await
        .expect("read the room")
        .into_iter()
        .next()
        .expect("the room")
    }

    /// **The contract these columns exist for: `None` writes NULL, never zero.** A probe that
    /// cannot tell must not be indistinguishable from a room nobody is in — which is exactly what a
    /// `COALESCE(..., 0)` or a defaulted struct field would produce, and it would look like a real
    /// reading forever after.
    #[tokio::test]
    async fn a_probe_that_cannot_tell_writes_null_rather_than_zero() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 4).await;
            let room = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "running",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;

            // What the TCP fallback produces: reachable, and nothing else known.
            record(&mut conn, room, &RoomStatus::default(), "tcp")
                .await
                .expect("record");

            let row = observed(&mut conn, room).await;
            assert_eq!(row.clients_connected, None, "not Some(0)");
            assert_eq!(row.last_activity_at, None, "never, not the epoch");
            assert_eq!(row.probe_kind.as_deref(), Some("tcp"));
            assert!(
                row.probed_at.is_some(),
                "the attempt is stamped even when it learned nothing -- that is how an operator \
                 tells 'no data' from 'not asked'"
            );

            // And a real answer lands as numbers.
            let spoke_at = Utc::now() - chrono::Duration::minutes(5);
            record(
                &mut conn,
                room,
                &RoomStatus {
                    net: NetStatus {
                        clients_connected: Some(3),
                        ..Default::default()
                    },
                    activity: ActivityStatus {
                        last_client_message_at: Some(spoke_at),
                        idle_seconds: Some(300),
                    },
                    ..Default::default()
                },
                "https",
            )
            .await
            .expect("record");

            let row = observed(&mut conn, room).await;
            assert_eq!(row.clients_connected, Some(3));
            assert_eq!(row.probe_kind.as_deref(), Some("https"));
            assert!(row.last_activity_at.is_some());
        })
        .await;
    }

    /// Only rooms worth asking, and only ones that can be asked.
    ///
    /// `starting` is excluded because a room that has not reached ready answers a connection
    /// refused, which is noise; a room with no reservation is excluded because there is no port to
    /// dial. Both would otherwise produce a failed probe every tick forever.
    #[tokio::test]
    async fn only_live_rooms_with_a_port_are_probed() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 4).await;

            let running = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "running",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;
            let degraded = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "degraded",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;
            let starting = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "starting",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;
            let portless = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "running",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;

            bind(&mut conn, running, 40000).await;
            bind(&mut conn, degraded, 40002).await;
            bind(&mut conn, starting, 40004).await;
            // `portless` deliberately gets none.

            let targets = live_rooms(&mut conn, Environment::Dev)
                .await
                .expect("list live rooms");
            let ids: Vec<RoomId> = targets.iter().map(|t| t.id).collect();

            assert!(ids.contains(&running));
            assert!(ids.contains(&degraded), "a degraded room is worth asking");
            assert!(!ids.contains(&starting), "it is not up yet");
            assert!(!ids.contains(&portless), "there is no port to dial");

            // The token travels with the target, so the caller never re-queries for it.
            assert!(targets.iter().all(|t| !t.admin_token.is_empty()));
        })
        .await;
    }
}
