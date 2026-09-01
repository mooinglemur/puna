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

mod common;

use common::with_db;
use diesel_async::RunQueryDsl;
use puna_core::db;

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
            // **The invariant is agreement, not a particular pair of numbers.** The range is the
            // deployment's to choose, so asserting literals here would only re-state whatever this
            // database happens to hold. What must be true is that the recorded range and the rows
            // that exist describe the same thing: a reservation outside the recorded range is
            // unallocatable-but-present, and a recorded range wider than the rows is capacity the
            // allocator will never find.
            let (lo, hi) = common::port_range(&mut conn, &row.environment).await;
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
        // series, which is exactly the "no data vs zero" ambiguity that list exists to avoid.
        assert_eq!(
            from_db,
            puna_core::metrics::ROOM_STATES,
            "metrics::ROOM_STATES is out of sync with the room_state enum"
        );
    })
    .await;
}
