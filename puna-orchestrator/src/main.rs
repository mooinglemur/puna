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

/// A burst of writes -- an organizer starting six rooms -- should cost one tick, not six.
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
    // `sum(puna_orchestrator_leader)` rather than being absent from it -- absent and zero look the
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

    let reconciler =
        reconcile::Reconciler::new(&config, pool.clone(), cluster, Arc::clone(&prober));
    let result = run(&config, &pool, &reconciler, &prober, &state).await;

    health_server.abort();
    result
}

/// Become the leader, then reconcile until the lock is lost.
async fn run(
    config: &OrchestratorConfig,
    pool: &puna_core::db::Pool,
    reconciler: &reconcile::Reconciler,
    prober: &Arc<probing::Prober>,
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

        let wake = Arc::new(Notify::new());
        let listener = tokio::spawn(listen(
            config.common.database_url.clone(),
            Arc::clone(&wake),
        ));

        // **Under the leader lock**, though claiming is a conditional `UPDATE` and a double-run is
        // already safe. Running it only here keeps one process answering a console, so an operator
        // cannot watch two dispatchers race for their button press.
        let dispatcher = tokio::spawn({
            let dispatcher = dispatch::Dispatcher::new(pool.clone(), Arc::clone(prober));
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
    let mut interval = tokio::time::interval(config.reconcile_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Constructed once, here, immediately after taking the lock -- which is the only place it is
    // legitimate. Replacing `assume_leader` with a constructor taking `&LeaderLock` is the last
    // step that turns this token from an assertion into proof; see `puna_core::model`.
    let orchestrator = Orchestrator::assume_leader();

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = wake.notified() => {
                // Debounce, so a burst of desired-state writes costs one pass rather than one each.
                tokio::time::sleep(NOTIFY_DEBOUNCE).await;
            }
            _ = lock.lost() => return Ok(()),
        }

        if !lock.is_held() {
            return Ok(());
        }

        match reconciler.tick(lock, orchestrator).await {
            Ok(report) => {
                state.mark_ticked();
                // A tick over a stable namespace reports only its room count, which is not news
                // every thirty seconds. Anything actually happening is.
                if report.actions > 0 || report.errors > 0 || report.integrity_faults > 0 {
                    tracing::info!(?report, "reconciled");
                }
            }
            // A failed tick is not fatal. The loop is level-triggered, so the next pass sees the
            // same world and tries again -- and dropping the lock here would hand leadership to a
            // process that would hit the same error, turning one bad tick into a rolling outage.
            Err(e) => tracing::error!(error = ?e, "reconcile tick failed"),
        }
    }
}

/// Refuse to run against the wrong environment's database.
///
/// Dev and prod share one public address and therefore one port space, and Cilium does not report
/// a collision -- it silently allocates a second IP, leaving a room reachable on an address DNS
/// never mentions. A `DATABASE_URL` pointed at the wrong environment is unrecoverable, so it is
/// checked at startup rather than discovered by a player.
async fn assert_environment(
    pool: &puna_core::db::Pool,
    environment: Environment,
) -> anyhow::Result<()> {
    let mut conn = pool.get().await?;
    puna_core::model::port::assert_environment_matches(&mut conn, environment).await?;
    Ok(())
}

/// Hold a `LISTEN` connection and poke `wake` on every notification.
///
/// Its own raw connection, because `LISTEN` is session-scoped and a pooled one is recycled between
/// callers. If it dies the loop falls back to the interval: **`NOTIFY` is latency, the tick is the
/// contract**, so losing this costs responsiveness rather than correctness.
async fn listen(database_url: String, wake: Arc<Notify>) {
    puna_core::notify::listen(&database_url, WAKE_CHANNEL, |_payload| wake.notify_one()).await;
}
