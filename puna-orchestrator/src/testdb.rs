//! A scratch database per test, plus the fixtures the step tests need.
//!
//! ## Why this is not the harness in `puna-core/tests/common`
//!
//! That one lives in an integration-test module, and **a binary crate cannot reach one**: the
//! orchestrator's tests are `#[cfg(test)]` modules inside the binary, so they link the crate's own
//! source rather than a library. Sharing it would mean moving it into `puna-core` behind a feature
//! and having `puna-core` dev-depend on itself to compile its own tests: more machinery than the
//! seventy lines it would save, and the fixtures differ anyway: these want rooms with ports, uids
//! and spec hashes, which no `puna-core` test has an opinion about.
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
        let name = format!("puna_orch_test_{}", uuid::Uuid::new_v4().simple());

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

/// A generation row **and its directory on disk**, consistently.
///
/// Both, because provisioning reads the one named by the other: a fixture that wrote only the row
/// would make every provision test fail on a missing seed, and one that wrote only the directory
/// would make them fail on a foreign key. The sha is what ties them together.
pub async fn insert_generation(
    conn: &mut AsyncPgConnection,
    layout: &crate::storage::Layout,
    slots: i32,
) -> GenerationId {
    let id = GenerationId::new();
    let sha = puna_core::hash::sha256_hex(id.to_string().as_bytes());

    let dir = layout.generation(&sha);
    std::fs::create_dir_all(&dir).expect("generation directory");
    std::fs::write(dir.join("seed.archipelago"), b"a seed").expect("seed");

    diesel::sql_query(
        "INSERT INTO generations (id, sha256, size_bytes, seed_name, slots, locations)
         VALUES ($1, decode($2, 'hex'), 1, 'seed', $3, 1)",
    )
    .bind::<SqlUuid, _>(id)
    .bind::<Text, _>(&sha)
    .bind::<Integer, _>(slots)
    .execute(conn)
    .await
    .expect("insert generation");
    id
}

/// How a fixture room should start out.
pub struct NewRoom<'a> {
    pub state: &'a str,
    pub desired: &'a str,
    pub slot_auth: &'a str,
}

impl Default for NewRoom<'_> {
    fn default() -> Self {
        Self {
            state: "provisioning",
            desired: "stopped",
            slot_auth: "none",
        }
    }
}

pub async fn insert_room(
    conn: &mut AsyncPgConnection,
    generation: GenerationId,
    new: NewRoom<'_>,
) -> RoomId {
    let id = RoomId::new();
    diesel::sql_query(
        "INSERT INTO rooms (id, environment, name, generation_id, source, spoiler_policy,
                            tracker_id, tracker_policy, admin_token, state, desired_state,
                            slot_auth, password)
         VALUES ($1, 'dev', 'test room', $2, 'direct', 'never', $3, 'link', $4,
                 $5::room_state, $6::room_desired_state, $7::slot_auth_mode,
                 CASE WHEN $7 = 'room' THEN 'a-room-password' ELSE NULL END)",
    )
    .bind::<SqlUuid, _>(id)
    .bind::<SqlUuid, _>(generation)
    .bind::<SqlUuid, _>(TrackerId::new())
    .bind::<Text, _>("t".repeat(52))
    .bind::<Text, _>(new.state)
    .bind::<Text, _>(new.desired)
    .bind::<Text, _>(new.slot_auth)
    .execute(conn)
    .await
    .expect("insert room");
    id
}

/// Add a slot, with or without a password.
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

/// The observed columns a step is supposed to have written.
#[derive(Debug, diesel::QueryableByName)]
pub struct Observed {
    #[diesel(sql_type = Text)]
    pub state: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub deployment_uid: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub spec_hash: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub advertised_host: Option<String>,
    #[diesel(sql_type = Nullable<Integer>)]
    pub advertised_port: Option<i32>,
    #[diesel(sql_type = Nullable<Integer>)]
    pub advertised_filtered_port: Option<i32>,
    #[diesel(sql_type = Integer)]
    pub failure_count: i32,
    #[diesel(sql_type = Integer)]
    pub not_ready_sweeps: i32,
    #[diesel(sql_type = Nullable<Text>)]
    pub last_error: Option<String>,
    #[diesel(sql_type = Nullable<diesel::sql_types::Timestamptz>)]
    pub retry_after: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = Nullable<diesel::sql_types::Timestamptz>)]
    pub provisioned_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn observed(conn: &mut AsyncPgConnection, room: RoomId) -> Option<Observed> {
    let rows: Vec<Observed> = diesel::sql_query(
        "SELECT state::text AS state, deployment_uid, spec_hash, advertised_host, advertised_port,
                advertised_filtered_port, failure_count, not_ready_sweeps, last_error, retry_after,
                provisioned_at
           FROM rooms WHERE id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await
    .expect("read observed");
    rows.into_iter().next()
}

/// The pair a room holds, and whether the reservation still points at it.
pub async fn reservation(conn: &mut AsyncPgConnection, room: RoomId) -> Option<i32> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Integer)]
        base_port: i32,
    }
    let rows: Vec<Row> =
        diesel::sql_query("SELECT base_port FROM port_reservations WHERE room_id = $1")
            .bind::<SqlUuid, _>(room)
            .load(conn)
            .await
            .expect("read reservation");
    rows.into_iter().next().map(|row| row.base_port)
}

/// Shrink an environment's range to its lowest `pairs` reservations.
///
/// Exhaustion and reclaim behavior are only observable near the end of a range, and deleting rows
/// is a far cheaper way to get there than allocating two and a half thousand times.
pub async fn shrink_range(conn: &mut AsyncPgConnection, pairs: i64) {
    diesel::sql_query(
        "DELETE FROM port_reservations
          WHERE environment = 'dev'
            AND base_port NOT IN (
                SELECT base_port FROM port_reservations
                 WHERE environment = 'dev' ORDER BY base_port ASC LIMIT $1)",
    )
    .bind::<diesel::sql_types::BigInt, _>(pairs)
    .execute(conn)
    .await
    .expect("shrink range");
}

/// A room's advisory-lock key, which every step takes before writing.
pub async fn lock_key(conn: &mut AsyncPgConnection, room: RoomId) -> i32 {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Integer)]
        lock_key: i32,
    }
    let rows: Vec<Row> = diesel::sql_query("SELECT lock_key FROM rooms WHERE id = $1")
        .bind::<SqlUuid, _>(room)
        .load(conn)
        .await
        .expect("read lock key");
    rows.into_iter().next().expect("the room exists").lock_key
}

/// The kinds of event recorded against a room, newest first.
pub async fn event_kinds(conn: &mut AsyncPgConnection, room: RoomId) -> Vec<String> {
    puna_core::model::event::recent(conn, room, 20)
        .await
        .expect("read events")
        .into_iter()
        .map(|e| e.kind)
        .collect()
}
