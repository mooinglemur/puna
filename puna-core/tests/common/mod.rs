//! Shared harness for the Postgres-backed test binaries.
//!
//! Each test gets its OWN database, created and dropped around it. That is what lets the
//! allocator's concurrency test race 64 connections against a table nothing else is touching,
//! and it means a failing test cannot leave state that fails the next one.
//!
//! Skipped, not failed, when `DATABASE_URL` is unset, so `cargo test` works without Postgres.
//! `PUNA_REQUIRE_DB_TESTS=1` turns the skip into a failure; CI sets it, so the coverage cannot be
//! dropped silently by a job that loses its service definition.

// This module is compiled separately into EVERY integration-test binary, and each one uses a
// different subset -- `schema.rs` needs none of the fixture builders. Without this, a helper used
// by one binary is dead code in the other and fails `clippy -D warnings`.
#![allow(dead_code)]

use diesel::sql_types::{Text, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use puna_core::db;
use puna_core::ids::{GenerationId, RoomId, TrackerId};
use tokio_postgres::NoTls;

pub struct TestDb {
    admin_url: String,
    name: String,
    pub pool: db::Pool,
}

impl TestDb {
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
        // FORCE terminates lingering backends; without it a pool connection not yet reaped keeps
        // the DROP waiting.
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

fn swap_database(url: &str, database: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{database}"),
        None => format!("{url}/{database}"),
    }
}

/// Run `body` against a fresh database, dropping it afterwards even if the body panics.
pub async fn with_db<F, Fut>(body: F)
where
    F: FnOnce(db::Pool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let Some(test_db) = TestDb::create().await else {
        assert!(
            std::env::var("PUNA_REQUIRE_DB_TESTS").is_err(),
            "PUNA_REQUIRE_DB_TESTS is set but DATABASE_URL is not: the Postgres-backed tests \
             would have been skipped"
        );
        eprintln!("DATABASE_URL unset; skipping Postgres-backed test");
        return;
    };

    let pool = test_db.pool.clone();
    let result = {
        use futures_util::FutureExt;
        std::panic::AssertUnwindSafe(body(pool))
            .catch_unwind()
            .await
    };

    test_db.drop_database().await;
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

/// Insert a generation so rooms have something to reference.
pub async fn insert_generation(conn: &mut AsyncPgConnection) -> GenerationId {
    let id = GenerationId::new();
    diesel::sql_query(
        "INSERT INTO generations (id, sha256, size_bytes, seed_name, slots, locations)
         VALUES ($1, decode(md5(random()::text), 'hex'), 1, 'seed', 1, 1)",
    )
    .bind::<SqlUuid, _>(id)
    .execute(conn)
    .await
    .expect("insert generation");
    id
}

/// Insert a room in a given observed state.
///
/// `state` decides whether the allocator may reclaim this room's port: `starting`, `running` and
/// `degraded` are live and must be protected.
pub async fn insert_room(
    conn: &mut AsyncPgConnection,
    generation: GenerationId,
    state: &str,
) -> RoomId {
    let id = RoomId::new();
    diesel::sql_query(
        "INSERT INTO rooms (id, environment, name, generation_id, source, spoiler_policy,
                            tracker_id, tracker_policy, admin_token, state)
         VALUES ($1, 'dev', 'test room', $2, 'direct', 'never', $3, 'link', 'token',
                 $4::room_state)",
    )
    .bind::<SqlUuid, _>(id)
    .bind::<SqlUuid, _>(generation)
    .bind::<SqlUuid, _>(TrackerId::new())
    .bind::<Text, _>(state)
    .execute(conn)
    .await
    .expect("insert room");
    id
}

/// Shrink an environment's range to its lowest `pairs` reservations.
///
/// Exhaustion and LRU behaviour are only observable near the end of a range, and deleting rows is
/// a far cheaper way to get there than allocating 2500 times.
pub async fn shrink_range(conn: &mut AsyncPgConnection, environment: &str, pairs: i64) {
    diesel::sql_query(
        "DELETE FROM port_reservations
          WHERE environment = $1::puna_environment
            AND base_port NOT IN (
                SELECT base_port FROM port_reservations
                 WHERE environment = $1::puna_environment
                 ORDER BY base_port ASC LIMIT $2)",
    )
    .bind::<Text, _>(environment)
    .bind::<diesel::sql_types::BigInt, _>(pairs)
    .execute(conn)
    .await
    .expect("shrink range");
}
