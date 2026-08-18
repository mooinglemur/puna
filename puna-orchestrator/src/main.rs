//! The orchestrator. Singleton, holds the Kubernetes credential, takes no inbound internet traffic.
//!
//! M0 scope: prove the dependency set links and boots. The leader advisory lock, the reconcile
//! tick and the command dispatcher arrive at M6.

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

    tracing::info!(version = puna_core::VERSION, "starting");
    Ok(())
}
