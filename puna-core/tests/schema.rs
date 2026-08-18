//! Postgres-backed tests for the migrations and the pool.
//!
//! Each test gets its OWN database, created and dropped around it, so they can run concurrently
//! and one failure cannot leave state that fails the next. That matters more from M2 onward: the
//! allocator's concurrency test wants many connections racing on a table nothing else touches.
//!
//! Skipped, not failed, when `DATABASE_URL` is unset, so `cargo test` works on a machine with no
//! Postgres. CI sets it, so the coverage is not optional there.
//!
//!   docker compose up -d
//!   DATABASE_URL=postgres://puna:puna@127.0.0.1:5433/puna cargo test

use diesel_async::RunQueryDsl;
use puna_core::db;
use tokio_postgres::NoTls;

/// A scratch database that drops itself.
pub struct TestDb {
    admin_url: String,
    name: String,
    pub pool: db::Pool,
}

impl TestDb {
    /// Create a uniquely named database, run every migration into it, and hand back a pool.
    async fn create() -> Option<Self> {
        let admin_url = std::env::var("DATABASE_URL").ok()?;
        let name = format!("puna_test_{}", uuid::Uuid::new_v4().simple());

        exec_on(&admin_url, &format!(r#"CREATE DATABASE "{name}""#)).await;

        let url = swap_database(&admin_url, &name);
        let pool = db::get_database_pool(&url, Some(puna_core::MIGRATIONS))
            .await
            .expect("migrations should apply to a fresh database");

        Some(Self {
            admin_url,
            name,
            pool,
        })
    }

    async fn drop_database(&self) {
        // FORCE terminates any lingering backend; without it a pool connection that has not yet
        // been reaped keeps the DROP waiting.
        exec_on(
            &self.admin_url,
            &format!(r#"DROP DATABASE IF EXISTS "{}" WITH (FORCE)"#, self.name),
        )
        .await;
    }
}

async fn exec_on(url: &str, sql: &str) {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .expect("failed to connect for administrative SQL");
    let handle = tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(sql)
        .await
        .expect("administrative SQL failed");
    drop(client);
    let _ = handle.await;
}

/// Replace the database component of a Postgres URL, leaving credentials and host intact.
fn swap_database(url: &str, database: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{database}"),
        None => format!("{url}/{database}"),
    }
}

/// Run `body` against a fresh database, dropping it afterwards even if the body panics.
async fn with_db<F, Fut>(body: F)
where
    F: FnOnce(db::Pool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let Some(test_db) = TestDb::create().await else {
        // Skipping reports `ok`, so a CI job that lost DATABASE_URL would stay green while
        // covering nothing. PUNA_REQUIRE_DB_TESTS turns the skip into a failure; CI sets it, so
        // the coverage cannot be dropped silently.
        assert!(
            std::env::var("PUNA_REQUIRE_DB_TESTS").is_err(),
            "PUNA_REQUIRE_DB_TESTS is set but DATABASE_URL is not: the Postgres-backed tests \
             would have been skipped"
        );
        eprintln!("DATABASE_URL unset; skipping Postgres-backed test");
        return;
    };

    let pool = test_db.pool.clone();
    let result = std::panic::AssertUnwindSafe(body(pool))
        .catch_unwind_or_run()
        .await;

    test_db.drop_database().await;
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

/// Small helper so a panicking test body still drops its database.
trait CatchUnwind: std::future::Future + Sized {
    async fn catch_unwind_or_run(self) -> Result<Self::Output, Box<dyn std::any::Any + Send>>;
}

impl<F: std::future::Future> CatchUnwind for std::panic::AssertUnwindSafe<F> {
    async fn catch_unwind_or_run(self) -> Result<F::Output, Box<dyn std::any::Any + Send>> {
        use futures_util::FutureExt;
        self.catch_unwind().await
    }
}

#[tokio::test]
async fn migrations_apply_and_report_current() {
    with_db(|pool| async move {
        // Applying twice must be a no-op, which is what makes the orchestrator safe to restart.
        db::assert_schema_current(&pool, puna_core::MIGRATIONS)
            .await
            .expect("schema should be current immediately after migrating");
    })
    .await;
}

#[tokio::test]
async fn port_reservations_are_preseeded_and_partitioned() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");

        #[derive(diesel::QueryableByName)]
        struct Row {
            #[diesel(sql_type = diesel::sql_types::Text)]
            environment: String,
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            count: i64,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            lo: i32,
            #[diesel(sql_type = diesel::sql_types::Integer)]
            hi: i32,
        }

        let rows: Vec<Row> = diesel::sql_query(
            "SELECT environment::text AS environment, count(*) AS count,
                    min(base_port) AS lo, max(base_port) AS hi
               FROM port_reservations GROUP BY environment ORDER BY environment",
        )
        .load(&mut conn)
        .await
        .expect("query");

        assert_eq!(rows.len(), 2, "both environments must be pre-seeded");
        for row in rows {
            let expected = match row.environment.as_str() {
                "dev" => puna_core::Environment::Dev,
                "prod" => puna_core::Environment::Prod,
                other => panic!("unexpected environment {other}"),
            };
            let (lo, hi) = expected.port_range();
            assert_eq!(row.lo as u16, lo, "{} low bound", row.environment);
            assert_eq!(row.hi as u16, hi, "{} high bound", row.environment);
            // One row per PAIR, not per port: (hi - lo) / 2 + 1.
            assert_eq!(
                row.count,
                ((hi - lo) / 2 + 1) as i64,
                "{} pair count",
                row.environment
            );
        }
    })
    .await;
}

#[tokio::test]
async fn room_states_match_the_database() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");

        #[derive(diesel::QueryableByName)]
        struct Label {
            #[diesel(sql_type = diesel::sql_types::Text)]
            label: String,
        }

        let rows: Vec<Label> = diesel::sql_query(
            "SELECT e.enumlabel::text AS label
               FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid
              WHERE t.typname = 'room_state'
              ORDER BY e.enumsortorder",
        )
        .load(&mut conn)
        .await
        .expect("query");

        let from_db: Vec<String> = rows.into_iter().map(|r| r.label).collect();

        // metrics::ROOM_STATES exists so the gauge can publish a zero per state at startup.
        // If a state is added to the migration and not there, the dashboard silently loses a
        // series -- which is exactly the "no data vs zero" ambiguity that list exists to avoid.
        assert_eq!(
            from_db,
            puna_core::metrics::ROOM_STATES,
            "metrics::ROOM_STATES is out of sync with the room_state enum"
        );
    })
    .await;
}
