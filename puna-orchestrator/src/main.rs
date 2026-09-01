//! The orchestrator. Singleton, holds the Kubernetes credential, takes no inbound internet traffic.
//!
//! One pass reads the world and applies the difference:
//!
//! ```text
//! leader lock -> snapshot the cluster -> load the rooms -> plan() -> execute -> sweep
//! ```
//!
//! [`plan`] is pure and decides; [`steps`] carries a decision out against the database and the
//! cluster; [`apply`] knows the one safe ordering for a room's objects; [`sweep`] handles what is
//! about the world rather than one room. [`cluster::ClusterApi`] is the only way any of it reaches
//! Kubernetes, which is what lets the whole lifecycle be tested against `cluster::fake`.

mod apply;
mod cluster;
mod dispatch;
mod health;
mod leader;
mod plan;
mod probing;
mod reconcile;
mod spec;
mod steps;
mod storage;
mod sweep;
#[cfg(test)]
mod testdb;

use std::sync::Arc;
use std::time::Duration;

use puna_core::model::Orchestrator;
use puna_core::{Environment, OrchestratorConfig};
use tokio::sync::Notify;

/// How long a process that lost the leader election waits before trying again.
///
/// Short enough that a rollout hands over in seconds, long enough that a parked replica is not
/// polling the database for nothing.
const LEADER_RETRY: Duration = Duration::from_secs(5);

/// The channel the web tier pokes when it writes `desired_state`.
const WAKE_CHANNEL: &str = "puna_wake";

/// A burst of writes (an organizer starting six rooms) should cost one tick, not six.
const NOTIFY_DEBOUNCE: Duration = Duration::from_millis(200);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version") {
        println!("puna-orchestrator {}", puna_core::VERSION);
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,puna_orchestrator=debug,puna_core=debug".into()),
        )
        .init();

    let config = OrchestratorConfig::from_env()?;
    // This process owns nearly the whole registry: everything about rooms, ports, reconciliation
    // and the cluster is computed here and nowhere else.
    puna_core::metrics::init(puna_core::metrics::Component::Orchestrator);

    tracing::info!(
        version = puna_core::VERSION,
        environment = config.common.environment.as_str(),
        namespace = %config.namespace,
        data_dir = %config.common.data_dir.display(),
        "starting"
    );

    // The orchestrator owns migrations: it is a singleton, so it is the only process that can run
    // them without racing itself. The web tier calls `assert_schema_current` instead and fails
    // readiness rather than serving against a schema it does not understand.
    let pool =
        puna_core::db::get_database_pool(&config.common.database_url, Some(puna_core::MIGRATIONS))
            .await?;

    let state = Arc::new(health::State::default());
    state.set_interval(config.reconcile_interval);
    // Published immediately, as a zero, so a parked replica shows up in
    // `sum(puna_orchestrator_leader)` rather than being absent from it: absent and zero look the
    // same in a graph and mean very different things.
    state.set_leader(false);

    let health_server = tokio::spawn(health::serve(Arc::clone(&state)));

    // Before the lock, so a missing kubeconfig or ServiceAccount fails at startup rather than on the
    // first room somebody tries to start. The orchestrator cannot do its job without this.
    let site = spec::Site {
        namespace: config.namespace.clone(),
        lb_ip: config.lb_ip.clone(),
        lb_sharing_key: config.lb_sharing_key.clone(),
        tls_secret: config.room_tls_secret.clone(),
        data_pvc: config.data_pvc.clone(),
        naming: crate::spec::Naming::from_config(&config),
    };
    let cluster: Arc<dyn cluster::ClusterApi> =
        Arc::new(cluster::kube::KubeCluster::connect(site).await?);

    // One prober, two users: the reconcile tick (probing, graceful stop) and the console
    // dispatcher. Sharing it is what keeps a `429` seen by one from being walked into by the other.
    let prober = Arc::new(probing::Prober::new(
        config.room_probe.build(),
        config.room_route.clone(),
        config.common.advertise_host.clone(),
        config.room_probe_timeout,
    ));

    let reconciler = reconcile::Reconciler::new(
        &config,
        pool.clone(),
        Arc::clone(&cluster),
        Arc::clone(&prober),
    );
    let result = run(&config, &pool, &reconciler, &prober, &cluster, &state).await;

    health_server.abort();
    result
}

/// Become the leader, then reconcile until the lock is lost.
async fn run(
    config: &OrchestratorConfig,
    pool: &puna_core::db::Pool,
    reconciler: &reconcile::Reconciler,
    prober: &Arc<probing::Prober>,
    cluster: &Arc<dyn cluster::ClusterApi>,
    state: &Arc<health::State>,
) -> anyhow::Result<()> {
    loop {
        let Some(lock) = leader::acquire(&config.common.database_url).await? else {
            // Not an error: during a rollout the outgoing pod still holds it. Alive but not ready,
            // so nothing restarts this process for doing exactly what it should.
            state.set_leader(false);
            tracing::info!("another process holds the orchestrator lock; waiting");
            tokio::time::sleep(LEADER_RETRY).await;
            continue;
        };

        state.set_leader(true);
        tracing::info!("acquired the orchestrator lock");

        // Before a single port is allocated. See the function's own note: this is the one mistake
        // in the system that cannot be undone.
        assert_environment(pool, config.common.environment).await?;
        assert_room_label_resolves(pool, reconciler).await?;
        // And in the same breath, because it is the same concern: record the range this deployment
        // owns and make the reservation rows match it. Held here rather than in the tick because
        // nothing may allocate before the database knows which ports are legitimate.
        assert_port_range(pool, config).await?;

        let wake = Arc::new(Notify::new());
        let listener = tokio::spawn(listen(
            config.common.database_url.clone(),
            Arc::clone(&wake),
        ));

        // **Under the leader lock**, though claiming is a conditional `UPDATE` and a double-run is
        // already safe. Running it only here keeps one process answering a console, so an operator
        // cannot watch two dispatchers race for their button press.
        let dispatcher = tokio::spawn({
            let dispatcher =
                dispatch::Dispatcher::new(pool.clone(), Arc::clone(prober), Arc::clone(cluster));
            let url = config.common.database_url.clone();
            async move { dispatcher.run(url).await }
        });

        let outcome = reconcile_until_lost(&lock, reconciler, state, config, &wake).await;

        dispatcher.abort();
        listener.abort();
        state.set_leader(false);
        outcome?;
        tracing::warn!("lost the orchestrator lock; re-electing");
    }
}

async fn reconcile_until_lost(
    lock: &leader::LeaderLock,
    reconciler: &reconcile::Reconciler,
    state: &Arc<health::State>,
    config: &OrchestratorConfig,
    wake: &Arc<Notify>,
) -> anyhow::Result<()> {
    // Constructed once, here, immediately after taking the lock, which is the only place it is
    // legitimate. Replacing `assume_leader` with a constructor taking `&LeaderLock` is the last
    // step that turns this token from an assertion into proof; see `puna_core::model`.
    let orchestrator = Orchestrator::assume_leader();

    // --- TWO CADENCES, ONE LOOP -----------------------------------------------------------------
    // A full pass every `reconcile_interval`, and a short convergence pass in between **while a
    // room is mid-transition**. A restart crosses two passes (one stops the room, one starts it)
    // so at the full interval alone that gap is most of a room's downtime, and none of it is the
    // pod. The convergence pass plans and applies exactly as the full one does; what it skips is
    // everything about the fleet rather than about the room in flight.
    //
    // **`last_reconcile`, NOT the last tick of any kind.** Stamping every pass would let a run of
    // convergence passes push the deadline forward indefinitely, so a fleet-wide restart (which
    // converges continuously for as long as it takes) would starve the sweep, the probe and the
    // hourly lane for the whole rollout. Only a full pass moves this.
    let mut last_reconcile = tokio::time::Instant::now() - config.reconcile_interval;

    loop {
        let now = tokio::time::Instant::now();
        let due = last_reconcile + config.reconcile_interval;
        let kind = if now >= due {
            plan::TickKind::Reconcile
        } else {
            plan::TickKind::Converge
        };

        if !lock.is_held() {
            return Ok(());
        }

        let report = match reconciler.tick(lock, orchestrator, kind).await {
            Ok(report) => {
                // **Readiness tracks FULL passes only.** `/readyz` means the whole contract is
                // being met, and a convergence pass meets part of it, so counting one would let a
                // loop that had somehow stopped doing full passes report itself healthy while the
                // sweep, the probe and the hourly lane were all silently stopped. It cannot
                // false-fail: the scheduler caps the next wake at the full pass's own deadline, so
                // a full pass always lands inside the interval readiness measures in.
                if kind == plan::TickKind::Reconcile {
                    state.mark_ticked();
                }
                // A pass over a stable namespace reports only its room count, which is not news.
                // Anything actually happening is, and a convergence pass that plans nothing is
                // the quiet case this cadence exists to produce, so it stays quiet.
                if report.actions > 0 || report.errors > 0 || report.integrity_faults > 0 {
                    tracing::info!(?report, "reconciled");
                }
                Some(report)
            }
            // A failed tick is not fatal. The loop is level-triggered, so the next pass sees the
            // same world and tries again, and dropping the lock here would hand leadership to a
            // process that would hit the same error, turning one bad tick into a rolling outage.
            Err(e) => {
                tracing::error!(error = ?e, "reconcile tick failed");
                None
            }
        };

        if kind == plan::TickKind::Reconcile {
            last_reconcile = now;
        }

        // A failed pass tells us nothing about the world, so it neither claims convergence is
        // needed nor claims it is finished: fall back to the ordinary cadence and read again.
        let converging = report.is_some_and(|r| r.wants_convergence());
        let next_full = last_reconcile + config.reconcile_interval;
        let next = if converging {
            // `min`, so a long run of convergence cannot push the full pass past its interval:
            // the same starvation `last_reconcile` prevents, arriving through the scheduler
            // instead of through the stamp.
            next_full.min(tokio::time::Instant::now() + config.converge_interval)
        } else {
            next_full
        };

        tokio::select! {
            _ = tokio::time::sleep_until(next) => {}
            _ = wake.notified() => {
                // Debounce, so a burst of desired-state writes costs one pass rather than one each.
                tokio::time::sleep(NOTIFY_DEBOUNCE).await;
            }
            _ = lock.lost() => return Ok(()),
        }
    }
}

/// Refuse to run against the wrong environment's database.
///
/// Dev and prod share one public address and therefore one port space, and Cilium does not report
/// a collision to Puna: a room requesting a specific address is REFUSED one on conflict, so it
/// never starts and no counter moves. A `DATABASE_URL` pointed at the wrong environment is
/// unrecoverable, so it is
/// checked at startup rather than discovered by a player.
async fn assert_environment(
    pool: &puna_core::db::Pool,
    environment: Environment,
) -> anyhow::Result<()> {
    let mut conn = pool.get().await?;
    puna_core::model::port::assert_environment_matches(&mut conn, environment).await?;
    Ok(())
}

/// Refuse to start if the configured room label does not recognize the objects already running.
///
/// **The failure this exists for is a fleet deletion carried out by the garbage collector.** The
/// room label is identity: every object is read back through it to answer *which room is this*, and
/// an object that does not answer is, by the sweep's definition, an orphan. Change the key and every
/// existing Deployment stops resolving at once: two strikes and two minutes later they are all
/// removed, with players still connected, by the one code path in the system whose job is deleting
/// things nobody owns.
///
/// (It would not even succeed. The label is the Deployment's `spec.selector`, which Kubernetes will
/// not let you update, so the recreate that followed would fail too. The point is that the deletion
/// happens first.)
///
/// The signature is specific rather than paranoid: refuse only when there are managed Deployments,
/// **none** of them resolve, and at least one room row exists. A genuine orphan or two is ordinary
/// and the sweep handles it; *all* of them unresolvable while rooms exist means the key moved.
async fn assert_room_label_resolves(
    pool: &puna_core::db::Pool,
    reconciler: &reconcile::Reconciler,
) -> anyhow::Result<()> {
    let deployments = reconciler.cluster().list_deployments().await?;
    if deployments.is_empty() || deployments.iter().any(|d| d.room_id.is_some()) {
        return Ok(());
    }

    let mut conn = pool.get().await?;
    let rooms = puna_core::model::room::count(&mut conn).await?;
    anyhow::ensure!(
        rooms == 0,
        "none of the {} in the cluster {} a room id under the configured label key, but this \
         database has {}. Refusing to start: every one of them would be treated as an orphan and \
         deleted. If the room label key was just changed, change it back: it is the Deployment's \
         immutable selector, so moving it needs every room recreated deliberately rather than by \
         restarting with a new value.",
        puna_core::text::count(deployments.len(), "room Deployment"),
        puna_core::text::plural(deployments.len(), "carries", "carry"),
        puna_core::text::count(rooms, "room"),
    );
    Ok(())
}

/// Write the configured port range into the database and reconcile the reservation rows to it.
///
/// The range is a property of the deployment's network rather than of Puna, so it arrives as
/// configuration, but the database is what enforces it, both through the trigger on
/// `port_reservations` and by simply not having rows for ports outside it. This is what puts the
/// configured value there.
async fn assert_port_range(
    pool: &puna_core::db::Pool,
    config: &OrchestratorConfig,
) -> anyhow::Result<()> {
    let mut conn = pool.get().await?;
    // Legitimate here for the same reason as in `reconcile`: the lock is held, and this runs before
    // anything else can touch a reservation.
    let orchestrator = Orchestrator::assume_leader();

    // **Strictly after `assert_environment` above.** That guard reads foreign reservations bound to
    // rooms to catch a `DATABASE_URL` pointed at the wrong environment; this deletes foreign rows.
    // In the other order the cleanup would erase the evidence and the guard would pass on a
    // database it should have refused.
    puna_core::model::port::forget_foreign_environment(
        &orchestrator,
        &mut conn,
        config.common.environment,
    )
    .await?;

    puna_core::model::port::ensure_range(
        &orchestrator,
        &mut conn,
        config.common.environment,
        config.port_range,
    )
    .await
}

/// Hold a `LISTEN` connection and poke `wake` on every notification.
///
/// Its own raw connection, because `LISTEN` is session-scoped and a pooled one is recycled between
/// callers. If it dies the loop falls back to the interval: **`NOTIFY` is latency, the tick is the
/// contract**, so losing this costs responsiveness rather than correctness.
async fn listen(database_url: String, wake: Arc<Notify>) {
    puna_core::notify::listen(&database_url, WAKE_CHANNEL, |_payload| wake.notify_one()).await;
}
