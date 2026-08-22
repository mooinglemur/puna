//! A scratch database per test, plus the fixtures the route tests need.
//!
//! ## Why this exists a third time
//!
//! `puna-core/tests/common` is an integration-test module and **a binary crate cannot reach one**;
//! `puna-orchestrator/src/testdb.rs` is the same harness for the same reason, and its fixtures want
//! rooms with ports, uids and spec hashes, which nothing here has an opinion about. Sharing would
//! mean moving it into `puna-core` behind a feature and having that crate dev-depend on itself to
//! compile its own tests — more machinery than the sixty lines it saves.
//!
//! ## What it is for
//!
//! One thing, and it is the piece of the console with the most reasoning behind it and the least
//! coverage otherwise: [`crate::routes::console::prepare_slot_credential`], which decides whether a
//! credential change can be made at all and what the operator is told about it. Every branch of
//! that decision is a statement about a room's state, so it cannot be asserted without one.
//!
//! Each test gets its own database, created and dropped around it, so a failing test cannot leave
//! state that fails the next one. Skipped when `DATABASE_URL` is unset; `PUNA_REQUIRE_DB_TESTS=1`
//! turns the skip into a failure, which is what CI sets so the coverage cannot go missing quietly.

use diesel::sql_types::{Integer, Nullable, Text, Uuid as SqlUuid};
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
        let name = format!("puna_web_test_{}", uuid::Uuid::new_v4().simple());

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

/// Run `body` against a fresh database, dropping it afterwards **even if the body panics**.
///
/// The `catch_unwind` is what makes that true: without it a failing assertion leaves the database
/// behind, and a few hundred test runs later somebody is wondering why Postgres is full.
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

/// Somebody to act as, since every write here records who did it.
pub const ACTOR: i64 = 4_931_000_000_000_000_001;

/// Idempotent, because a test that builds several rooms wants the same actor for all of them —
/// and a fixture that can only be called once constrains tests for no reason anybody would guess.
pub async fn insert_user(conn: &mut AsyncPgConnection, id: i64) {
    diesel::sql_query(
        "INSERT INTO users (id, username) VALUES ($1, 'staff') ON CONFLICT (id) DO NOTHING",
    )
    .bind::<diesel::sql_types::BigInt, _>(id)
    .execute(conn)
    .await
    .expect("insert user");
}

/// A generation row. **No directory**, unlike the orchestrator's fixture: nothing in the web tier's
/// credential path reads a seed, and creating one would be a file this test does not clean up.
pub async fn insert_generation(conn: &mut AsyncPgConnection) -> GenerationId {
    let id = GenerationId::new();
    let sha = puna_core::hash::sha256_hex(id.to_string().as_bytes());

    diesel::sql_query(
        "INSERT INTO generations (id, sha256, size_bytes, seed_name, slots, locations)
         VALUES ($1, decode($2, 'hex'), 1, 'seed', 2, 1)",
    )
    .bind::<SqlUuid, _>(id)
    .bind::<Text, _>(&sha)
    .execute(conn)
    .await
    .expect("insert generation");
    id
}

/// A room in a given observed state and password mode.
///
/// `state` is what these tests are about: the credential rule branches on it, and a fixture that
/// could only produce one state would assert a third of the decision.
pub async fn insert_room(
    conn: &mut AsyncPgConnection,
    generation: GenerationId,
    state: &str,
    slot_auth: &str,
) -> RoomId {
    let id = RoomId::new();
    diesel::sql_query(
        "INSERT INTO rooms (id, environment, name, generation_id, source, spoiler_policy,
                            tracker_id, tracker_policy, admin_token, state, desired_state,
                            slot_auth, password, secret_synced_at)
         VALUES ($1, 'dev', 'test room', $2, 'direct', 'never', $3, 'link', $4,
                 $5::room_state, 'running', $6::slot_auth_mode,
                 CASE WHEN $6 = 'room' THEN 'a-room-password' ELSE NULL END,
                 -- Set, so a test can assert that a credential change CLEARS it. Left NULL the
                 -- assertion would pass without the code doing anything.
                 now())",
    )
    .bind::<SqlUuid, _>(id)
    .bind::<SqlUuid, _>(generation)
    .bind::<SqlUuid, _>(TrackerId::new())
    .bind::<Text, _>("t".repeat(52))
    .bind::<Text, _>(state)
    .bind::<Text, _>(slot_auth)
    .execute(conn)
    .await
    .expect("insert room");
    id
}

pub async fn insert_slot(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    slot_number: i32,
    password: Option<&str>,
) {
    diesel::sql_query(
        "INSERT INTO room_slots (room_id, slot_number, player_name, game, kind, password,
                                 tracker_id)
         VALUES ($1, $2, 'player', 'A Link to the Past', 'player', $3, $4)",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Integer, _>(slot_number)
    .bind::<Nullable<Text>, _>(password)
    .bind::<SqlUuid, _>(TrackerId::new())
    .execute(conn)
    .await
    .expect("insert slot");
}

/// Whether the room's Secret is marked as needing a re-apply.
///
/// `secret_synced_at IS NULL` is the contract M9 defined and M19 gave its first producer: it means
/// "this Secret no longer matches the database". Every credential change has to set it, or the
/// change lives only in a running pod's memory and lapses at the next restart.
pub async fn secret_is_stale(conn: &mut AsyncPgConnection, room: RoomId) -> bool {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        stale: bool,
    }

    let rows: Vec<Row> =
        diesel::sql_query("SELECT secret_synced_at IS NULL AS stale FROM rooms WHERE id = $1")
            .bind::<SqlUuid, _>(room)
            .load(conn)
            .await
            .expect("read secret_synced_at");

    rows.into_iter().next().expect("the room exists").stale
}
