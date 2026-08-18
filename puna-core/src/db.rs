//! Database pool, TLS setup, migrations and per-query instrumentation.
//!
//! Vendored from `Archipelago-lobby/common/src/db.rs` rather than depended on, so Puna stays a
//! standalone repository. Behavior is deliberately the same; the deviations are marked DEVIATION
//! below and each one is a bug or a footgun in the original that Puna's shape would hit.

use std::sync::{Arc, LazyLock};
use std::time::Instant;

use diesel::connection::Instrumentation;
use diesel::{ConnectionError, ConnectionResult};
use diesel_async::AsyncPgConnection;
use diesel_async::async_connection_wrapper::AsyncConnectionWrapper;
use diesel_async::pooled_connection::deadpool::Pool as DieselPool;
use diesel_async::pooled_connection::{AsyncDieselConnectionManager, ManagerConfig};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use prometheus::{HistogramOpts, HistogramVec};
use rustls::Error as TLSError;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring;
use rustls::pki_types::{ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

pub type Pool = DieselPool<AsyncPgConnection>;

/// Per-query timing, exported as `diesel_query_seconds`.
///
/// Registered by `crate::metrics`, not here, so there is exactly one registry and the two
/// binaries cannot drift on metric names.
// DEVIATION: `std::sync::LazyLock` instead of `once_cell::sync::Lazy`. Stable since 1.80 and
// Puna's toolchain is 1.95, so the dependency bought nothing.
pub static QUERY_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new("diesel_query_seconds", "SQL query duration").buckets(vec![
            0.000005, 0.00001, 0.00005, 0.0001, 0.0005, 0.001, 0.005, 0.01, 0.1, 1.0,
        ]),
        &["query"],
    )
    .expect("failed to create query histogram")
});

#[derive(Default)]
pub struct DbInstrumentation {
    query_start: Option<Instant>,
}

impl Instrumentation for DbInstrumentation {
    fn on_connection_event(&mut self, event: diesel::connection::InstrumentationEvent<'_>) {
        match event {
            diesel::connection::InstrumentationEvent::StartQuery { .. } => {
                self.query_start = Some(Instant::now());
            }
            diesel::connection::InstrumentationEvent::FinishQuery { query, .. } => {
                let Some(query_start) = self.query_start else {
                    return;
                };
                let elapsed = query_start.elapsed();
                let query = query.to_string().replace('\n', " ");
                let query = query.split("--").next().unwrap_or_default().trim();
                QUERY_HISTOGRAM
                    .with_label_values(&[query])
                    .observe(elapsed.as_secs_f64());
                // DEVIATION: DEBUG, not INFO. The lobby logs every query at INFO, which at
                // Puna's reconcile cadence would bury everything else in the container log.
                tracing::debug!(%query, elapsed_us = elapsed.as_micros(), "query finished");
            }
            _ => {}
        };
    }
}

/// Accepts any server certificate.
///
/// INHERITED WEAKNESS, kept deliberately so the vendored behavior matches the lobby's. The
/// Postgres connection is pod-to-pod inside the cluster and CNPG serves a certificate signed by
/// its own generated CA, which nothing here is configured to trust. The honest fix is to mount
/// the CNPG cluster CA and verify against it; until then this trades server authentication for
/// not shipping a CA bundle, and the traffic is still encrypted.
#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer,
        _intermediates: &[rustls::pki_types::CertificateDer],
        _server_name: &ServerName,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TLSError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TLSError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TLSError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

/// Install ring as the process-wide rustls provider.
///
/// DEVIATION: the lobby calls `.expect("Failed to set ring as crypto provider")`.
/// `install_default` returns `Err` when a default is ALREADY installed, so that panics if
/// anything built a TLS config first -- and in Puna something will: reqwest in the web tier,
/// kube in the orchestrator, and every test that builds a second pool. Since `ring` is the only
/// provider feature enabled (see the workspace Cargo.toml), rustls would auto-install it anyway;
/// this call just makes the choice explicit and early. An already-installed provider is success.
fn ensure_crypto_provider() {
    if ring::default_provider().install_default().is_err() {
        tracing::debug!("rustls crypto provider was already installed");
    }
}

#[tracing::instrument(skip_all)]
fn establish_connection(config: &str) -> BoxFuture<'_, ConnectionResult<AsyncPgConnection>> {
    let fut = async {
        let rustls_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();

        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(rustls_config);
        let (client, conn) = tokio_postgres::connect(config, tls)
            .await
            .map_err(|e| ConnectionError::BadConnection(e.to_string()))?;
        tokio::spawn(async move {
            // DEVIATION: tracing, not eprintln!. Puna installs a subscriber, so this reaches the
            // container log with context instead of being an orphaned line on stderr.
            if let Err(e) = conn.await {
                tracing::error!(error = %e, "database connection closed with an error");
            }
        });
        AsyncPgConnection::try_from(client).await
    };
    fut.boxed()
}

/// Build the pool, optionally running migrations first.
///
/// DEVIATION: `migrations` is an `Option`, and that is the orchestrator/web split. The
/// orchestrator passes `Some(MIGRATIONS)` -- it is a singleton holding a leader lock, so it is
/// the natural migrator. The web tier passes `None` and calls [`assert_schema_current`] instead,
/// failing readiness rather than serving against a schema it does not understand. The lobby runs
/// migrations from every replica, which is a race it has been lucky with.
pub async fn get_database_pool(
    db_url: &str,
    migrations: Option<EmbeddedMigrations>,
) -> anyhow::Result<Pool> {
    ensure_crypto_provider();

    // DEVIATION: tolerate an existing instrumentation hook. The lobby `.expect()`s here, which
    // panics the second time a pool is built -- fine for a process that builds one, fatal for a
    // test suite that builds one per test.
    let _ = diesel::connection::set_default_instrumentation(|| {
        Some(Box::new(DbInstrumentation::default()))
    });

    let mut config = ManagerConfig::default();
    config.custom_setup = Box::new(establish_connection);

    let mgr = AsyncDieselConnectionManager::<AsyncPgConnection>::new_with_config(db_url, config);
    let pool = DieselPool::builder(mgr)
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build database pool: {e}"))?;

    if let Some(migrations) = migrations {
        run_migrations(&pool, migrations).await?;
    }

    Ok(pool)
}

/// Apply any pending migrations. Blocking work, so it runs on the blocking pool.
// DEVIATION: errors propagate. The lobby `.unwrap()`s inside the spawned task, which turns a
// failed migration into a panic in a worker thread rather than a startup error naming the cause.
async fn run_migrations(pool: &Pool, migrations: EmbeddedMigrations) -> anyhow::Result<()> {
    let connection = pool
        .get()
        .await
        .map_err(|e| anyhow::anyhow!("failed to get a connection to run migrations: {e}"))?;

    let mut wrapper: AsyncConnectionWrapper<
        deadpool::managed::Object<AsyncDieselConnectionManager<AsyncPgConnection>>,
    > = AsyncConnectionWrapper::from(connection);

    let applied = tokio::task::spawn_blocking(move || {
        wrapper
            .run_pending_migrations(migrations)
            .map(|versions| versions.iter().map(ToString::to_string).collect::<Vec<_>>())
            .map_err(|e| anyhow::anyhow!("migration failed: {e}"))
    })
    .await??;

    if applied.is_empty() {
        tracing::info!("schema already current");
    } else {
        tracing::info!(?applied, "applied migrations");
    }
    Ok(())
}

/// Fail unless every embedded migration has been applied.
///
/// The web tier's readiness gate. A web pod rolled out ahead of the orchestrator stays NotReady
/// instead of serving reads against columns that do not exist yet.
pub async fn assert_schema_current(
    pool: &Pool,
    migrations: EmbeddedMigrations,
) -> anyhow::Result<()> {
    let connection = pool
        .get()
        .await
        .map_err(|e| anyhow::anyhow!("failed to get a connection to check the schema: {e}"))?;

    let mut wrapper: AsyncConnectionWrapper<
        deadpool::managed::Object<AsyncDieselConnectionManager<AsyncPgConnection>>,
    > = AsyncConnectionWrapper::from(connection);

    let pending = tokio::task::spawn_blocking(move || {
        wrapper
            .has_pending_migration(migrations)
            .map_err(|e| anyhow::anyhow!("failed to check for pending migrations: {e}"))
    })
    .await??;

    if pending {
        anyhow::bail!(
            "database schema is not current: migrations are pending. The orchestrator applies \
             them; this process will stay unready until it has."
        );
    }
    Ok(())
}
