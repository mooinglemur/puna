//! `/healthz`, `/readyz` and `/metrics` on `[::]:9090`.
//!
//! axum rather than Rocket for three endpoints: the orchestrator has no business acquiring a
//! figment, a config file or a secret key, and it serves no user-facing page.
//!
//! ## The two probes answer different questions, on purpose
//!
//! `/healthz` is "this process is alive". It is deliberately true for a **parked** replica -- one
//! that lost the leader election and is waiting -- because restarting it would achieve nothing and
//! a rollout would then crashloop the incoming pod while the outgoing one still held the lock.
//!
//! `/readyz` is "this process is leading and reconciling", which is `leader && last tick within
//! three intervals`. A leader whose loop wedged reports not-ready while still reporting alive,
//! which is the state worth alerting on and the one a naive single probe hides.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;

/// Shared between the reconcile loop and the probes.
#[derive(Debug)]
pub struct State {
    leader: AtomicBool,
    /// Unix seconds of the last successful tick. Zero means "never", which reads as not-ready.
    last_tick: AtomicU64,
    /// The reconcile interval, so readiness can be expressed in ticks rather than a second
    /// constant that would drift away from the loop's.
    interval_secs: AtomicU64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            leader: AtomicBool::new(false),
            last_tick: AtomicU64::new(0),
            interval_secs: AtomicU64::new(30),
        }
    }
}

impl State {
    pub fn set_leader(&self, leading: bool) {
        self.leader.store(leading, Ordering::Relaxed);
        puna_core::metrics::set_leader(leading);
    }

    pub fn set_interval(&self, interval: Duration) {
        self.interval_secs
            .store(interval.as_secs().max(1), Ordering::Relaxed);
    }

    pub fn mark_ticked(&self) {
        self.last_tick.store(now_secs(), Ordering::Relaxed);
    }

    fn is_ready(&self) -> bool {
        if !self.leader.load(Ordering::Relaxed) {
            return false;
        }
        let last = self.last_tick.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let budget = self.interval_secs.load(Ordering::Relaxed).saturating_mul(3);
        now_secs().saturating_sub(last) <= budget
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub async fn serve(state: Arc<State>) {
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get({
                let state = Arc::clone(&state);
                move || {
                    let state = Arc::clone(&state);
                    async move {
                        if state.is_ready() {
                            (StatusCode::OK, "ready")
                        } else {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                "not leading, or not ticking",
                            )
                        }
                    }
                }
            }),
        )
        // Unauthenticated, unlike the web tier's: this listener is a ClusterIP Service with no
        // route from outside the cluster, and Prometheus has no way to hold a session.
        .route("/metrics", get(|| async { puna_core::metrics::gather() }));

    let listener = match tokio::net::TcpListener::bind("[::]:9090").await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(error = %e, "could not bind the health listener");
            return;
        }
    };

    tracing::info!("health endpoints on [::]:9090");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "health server stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parked_replica_is_alive_but_not_ready() {
        let state = State::default();
        state.set_leader(false);
        state.mark_ticked();
        assert!(
            !state.is_ready(),
            "not leading means not ready, however recently it ticked"
        );
    }

    #[test]
    fn a_leader_that_has_not_ticked_yet_is_not_ready() {
        let state = State::default();
        state.set_leader(true);
        assert!(!state.is_ready(), "no tick yet");
        state.mark_ticked();
        assert!(state.is_ready());
    }

    /// The case a single liveness probe would hide: leading, alive, and not doing anything.
    #[test]
    fn a_wedged_leader_reports_not_ready() {
        let state = State::default();
        state.set_leader(true);
        state.set_interval(Duration::from_secs(30));

        // Three intervals is the budget, so 91 seconds ago is one second past it.
        state
            .last_tick
            .store(now_secs().saturating_sub(91), Ordering::Relaxed);
        assert!(!state.is_ready());

        state
            .last_tick
            .store(now_secs().saturating_sub(89), Ordering::Relaxed);
        assert!(state.is_ready());
    }
}
