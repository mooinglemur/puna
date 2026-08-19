//! The orchestrator. Singleton, holds the Kubernetes credential, takes no inbound internet traffic.
//!
//! M6 scope: the leader advisory lock, the reconcile tick, `LISTEN`/`NOTIFY`, the health server,
//! the Secret builder and room provisioning. **No Kubernetes call exists yet** -- `ClusterApi`,
//! its in-memory fake and the rest of the state machine land at M7, which is why a room here gets
//! as far as `idle` and stops there.

mod health;
mod leader;
mod reconcile;
mod spec;
mod storage;

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
    puna_core::metrics::init();

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

    let layout = storage::Layout::new(&config.common.data_dir);
    let result = run(&config, &pool, &layout, &state).await;

    health_server.abort();
    result
}

/// Become the leader, then reconcile until the lock is lost.
async fn run(
    config: &OrchestratorConfig,
    pool: &puna_core::db::Pool,
    layout: &storage::Layout,
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

        let outcome = reconcile_until_lost(&lock, pool, layout, state, config, &wake).await;

        listener.abort();
        state.set_leader(false);
        outcome?;
        tracing::warn!("lost the orchestrator lock; re-electing");
    }
}

async fn reconcile_until_lost(
    lock: &leader::LeaderLock,
    pool: &puna_core::db::Pool,
    layout: &storage::Layout,
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

        match reconcile::tick(lock, pool, layout, orchestrator).await {
            Ok(report) => {
                state.mark_ticked();
                if report != reconcile::TickReport::default() {
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
    use futures_util::StreamExt;

    loop {
        match puna_core::db::raw_connection_with_notifications(&database_url).await {
            Ok((client, mut notifications)) => {
                if let Err(e) = client
                    .batch_execute(&format!("LISTEN {WAKE_CHANNEL}"))
                    .await
                {
                    tracing::warn!(error = %e, "LISTEN failed; falling back to the tick");
                } else {
                    tracing::info!(channel = WAKE_CHANNEL, "listening for wake-ups");
                    while let Some(message) = notifications.next().await {
                        if matches!(message, tokio_postgres::AsyncMessage::Notification(_)) {
                            wake.notify_one();
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not open a LISTEN connection"),
        }

        tracing::warn!("LISTEN connection lost; the reconcile interval still applies");
        tokio::time::sleep(LEADER_RETRY).await;
    }
}
