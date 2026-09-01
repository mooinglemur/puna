//! An unauthenticated `/metrics` on `[::]:9090`, alongside the public Rocket listener.
//!
//! ## Why a second listener rather than the route that already exists
//!
//! `puna-web` already serves `/metrics` on its public port behind an [`AdminSession`] guard, and
//! that route stays: it is how a human admin reads the registry in a browser. But an
//! `AdminSession` is a **Discord session cookie and nothing else**: there is no bearer-token or
//! basic-auth form of it, so a Prometheus `ServiceMonitor` has no credential it could present.
//!
//! That is not a hypothetical. The lobby hit the same wall from the other side: its `/metrics`
//! is admin-guarded too, and an anonymous scrape returned 403 while Prometheus reported
//! `TargetDown` at 100% from the day it was deployed until a `basicAuth` block was added. The
//! lobby *could* add one because its session layer accepts `Authorization: Basic admin:$TOKEN`.
//! Puna's does not, so there is nothing to configure and the tier would simply stay unscraped.
//!
//! ## Why unauthenticated is correct here rather than a shortcut
//!
//! This listener is on its own port, published only as a ClusterIP with no HTTPRoute in front of
//! it, so it is unreachable from outside the cluster. That is exactly the reasoning
//! `puna-orchestrator`'s `health.rs` already records for its own `/metrics`, in its own words:
//! *"Prometheus has no way to hold a session."* This is the same decision applied to the tier that
//! needed it second, not a new one, which is why the port number matches rather than being
//! chosen afresh.
//!
//! The NetworkPolicy that lands with the manifests narrows it further, to the monitoring namespace.
//!
//! ## Both roles, deliberately
//!
//! `PUNA_ROLE=tracker` does not mount the admin-guarded route at all (it is in the `Role::Web`
//! branch), so for the tracker tier this listener is the *only* way its metrics leave the process.
//! Starting it for both roles is what makes the two Deployments symmetrical to scrape.
//!
//! ## A failure to bind is not fatal
//!
//! Losing metrics is worth a loud error and nothing more; it must never take down a tier that is
//! otherwise serving players. The orchestrator makes the same call for the same reason.

use axum::Router;
use axum::routing::get;

/// The port. Hardcoded to match `puna-orchestrator`'s, deliberately: one number to remember, one
/// `ServiceMonitor` shape across all three Deployments, and nothing a manifest could set to a value
/// the scrape config disagrees with.
pub const PORT: u16 = 9090;

/// Serve until the process exits. Spawn it; it never returns in the healthy case.
pub async fn serve() {
    let app = Router::new().route("/metrics", get(|| async { puna_core::metrics::gather() }));

    let listener = match tokio::net::TcpListener::bind(("::", PORT)).await {
        Ok(listener) => listener,
        Err(e) => {
            // Not a panic and not an exit. A tier that cannot export metrics is degraded; a tier
            // that refuses to start is an outage.
            tracing::error!(error = %e, port = PORT, "metrics listener failed to bind");
            return;
        }
    };

    tracing::info!(port = PORT, "metrics on [::]:{PORT}");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "metrics listener stopped");
    }
}
