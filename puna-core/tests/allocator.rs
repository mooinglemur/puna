//! Postgres-backed tests for the port allocator.
//!
//! This is the code whose bugs are least recoverable: a port handed to two rooms, or taken from a
//! live one, produces a room that is unreachable rather than an error, because Cilium does not
//! report a sharing-key collision. So the properties are asserted against a real database rather
//! than reasoned about.

mod common;

use common::{insert_generation, insert_room, shrink_range, with_db};
use puna_core::Environment;
use puna_core::model::Orchestrator;
use puna_core::model::port::{self, AllocError};

const DEV: Environment = Environment::Dev;

#[tokio::test]
async fn path_1_a_room_returns_to_its_own_port() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let room = insert_room(&mut conn, generation, "idle").await;

        let first = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("first allocation");

        // Torn down and started again. The reservation outlives the Service, which is the whole
        // reason this state lives in Postgres rather than being derived from the cluster.
        let second = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("second allocation");

        assert_eq!(first, second, "a room must come back on its own port");
    })
    .await;
}

#[tokio::test]
async fn path_2_never_allocated_ports_come_first_and_are_distinct() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        let mut seen = Vec::new();
        for _ in 0..5 {
            let room = insert_room(&mut conn, generation, "idle").await;
            seen.push(
                port::allocate_pair(&orch, &mut conn, DEV, room)
                    .await
                    .expect("allocation"),
            );
        }

        let (lo, _) = DEV.port_range();
        // '-infinity' sorts first and ties break on base_port, so a fresh range hands out its
        // lowest pairs in order. Asserting the exact sequence pins the ordering the LRU rule and
        // the never-allocated rule share.
        assert_eq!(seen, vec![lo, lo + 2, lo + 4, lo + 6, lo + 8]);
    })
    .await;
}

#[tokio::test]
async fn path_3_lru_reclaims_the_oldest_idle_room() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        // Three pairs, three idle rooms: the range is now full.
        shrink_range(&mut conn, "dev", 3).await;
        let mut rooms = Vec::new();
        for _ in 0..3 {
            let room = insert_room(&mut conn, generation, "idle").await;
            port::allocate_pair(&orch, &mut conn, DEV, room)
                .await
                .expect("allocation");
            rooms.push(room);
        }

        // The first room allocated is the least recently active, so it is the victim.
        let victim = rooms[0];
        let victim_port = port::allocate_pair(&orch, &mut conn, DEV, victim)
            .await
            .expect("victim still holds its port");
        // Re-allocating touched last_activity, so make it the oldest again explicitly rather
        // than depending on the order of the loop above.
        set_last_activity_ancient(&mut conn, victim_port).await;

        let newcomer = insert_room(&mut conn, generation, "idle").await;
        let taken = port::allocate_pair(&orch, &mut conn, DEV, newcomer)
            .await
            .expect("LRU reclaim should succeed");

        assert_eq!(
            taken, victim_port,
            "the oldest idle room's pair is reclaimed"
        );

        // The victim keeps its row and its on-disk state; only the binding is gone. Losing a
        // port must never mean losing a room.
        assert!(
            room_still_exists(&mut conn, victim).await,
            "reclaim must not delete the victim room"
        );
    })
    .await;
}

#[tokio::test]
async fn a_live_room_is_never_reclaimed_and_exhaustion_is_loud() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        // Four pairs, four rooms, all of them serving players.
        shrink_range(&mut conn, "dev", 4).await;
        for state in ["starting", "running", "degraded", "running"] {
            let room = insert_room(&mut conn, generation, state).await;
            port::allocate_pair(&orch, &mut conn, DEV, room)
                .await
                .expect("allocation");
        }

        let newcomer = insert_room(&mut conn, generation, "idle").await;
        let result = port::allocate_pair(&orch, &mut conn, DEV, newcomer).await;

        // THE DESIGN DOC'S ORDERING WOULD HAVE STOLEN ONE OF THESE. Failing loudly is correct:
        // taking a port from a room with connected clients drops those players, and Cilium
        // reports nothing, so the room would look healthy while being unreachable.
        assert!(
            matches!(result, Err(AllocError::Exhausted { .. })),
            "expected Exhausted, got {result:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn an_unbound_pair_is_always_preferred_over_reclaiming() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let (lo, _) = DEV.port_range();

        shrink_range(&mut conn, "dev", 2).await;

        // Bind the LOW pair to an idle room, then age it so LRU ordering alone would pick it.
        let holder = insert_room(&mut conn, generation, "idle").await;
        let held = port::allocate_pair(&orch, &mut conn, DEV, holder)
            .await
            .expect("allocation");
        assert_eq!(held, lo);
        set_last_activity_ancient(&mut conn, held).await;

        // The remaining pair is unbound but has a NEWER last_activity, so a single-statement
        // allocator ordering only on last_activity would reclaim from `holder` instead.
        let newcomer = insert_room(&mut conn, generation, "idle").await;
        let got = port::allocate_pair(&orch, &mut conn, DEV, newcomer)
            .await
            .expect("allocation");

        assert_ne!(
            got, held,
            "an unbound pair must be taken before any bound pair is reclaimed"
        );
        assert!(
            port::allocate_pair(&orch, &mut conn, DEV, holder)
                .await
                .unwrap()
                == held,
            "the holder must keep its pair while an unbound one was available"
        );
    })
    .await;
}

#[tokio::test]
async fn sixty_four_concurrent_allocations_get_distinct_ports() {
    with_db(|pool| async move {
        let generation = {
            let mut conn = pool.get().await.expect("connection");
            insert_generation(&mut conn).await
        };

        let mut rooms = Vec::new();
        {
            let mut conn = pool.get().await.expect("connection");
            for _ in 0..64 {
                rooms.push(insert_room(&mut conn, generation, "idle").await);
            }
        }

        // SKIP LOCKED is what makes this work under READ COMMITTED: N allocators take N distinct
        // rows with no retries and no serialization failures. A bug here is two rooms on one
        // port, which Cilium resolves by silently allocating a second IP.
        let mut tasks = Vec::new();
        for room in rooms {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                let orch = Orchestrator::assume_leader();
                let mut conn = pool.get().await.expect("connection");
                port::allocate_pair(&orch, &mut conn, DEV, room).await
            }));
        }

        let mut ports = Vec::new();
        for task in tasks {
            ports.push(
                task.await
                    .expect("task")
                    .expect("allocation must not error"),
            );
        }

        let unique: std::collections::HashSet<_> = ports.iter().copied().collect();
        assert_eq!(
            unique.len(),
            64,
            "every allocation must get a distinct pair"
        );

        // Pairs are adjacent, so no allocated base may be one above another allocated base.
        for p in &ports {
            assert!(
                !unique.contains(&(p.wrapping_sub(1))),
                "pair {p} overlaps the pair below it"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn quarantine_holds_a_pair_out_then_releases_it() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        shrink_range(&mut conn, "dev", 2).await;
        let (lo, _) = DEV.port_range();

        let room = insert_room(&mut conn, generation, "idle").await;
        let first = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("allocation");
        assert_eq!(first, lo);

        // Simulates the ingress-IP read-back finding the Service on an unexpected address.
        let until = chrono::Utc::now() + chrono::Duration::hours(1);
        port::quarantine(&orch, &mut conn, DEV, first, until)
            .await
            .expect("quarantine");

        let reallocated = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("should move to the other pair");
        assert_ne!(reallocated, first, "a quarantined pair must not be reused");

        // Expiring the quarantine returns it to circulation.
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        port::quarantine(&orch, &mut conn, DEV, first, past)
            .await
            .expect("expire quarantine");

        let other = insert_room(&mut conn, generation, "idle").await;
        let recovered = port::allocate_pair(&orch, &mut conn, DEV, other)
            .await
            .expect("expired quarantine should be allocatable");
        assert_eq!(recovered, first);
    })
    .await;
}

#[tokio::test]
async fn release_unbinds_without_disturbing_lru_order() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        let room = insert_room(&mut conn, generation, "idle").await;
        let held = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("allocation");

        port::release(&orch, &mut conn, room)
            .await
            .expect("release");

        let stats = port::stats(&mut conn, DEV).await.expect("stats");
        assert_eq!(stats.bound, 0, "release must unbind the pair");

        // last_activity is deliberately NOT reset, so a just-released port sorts LAST and the
        // room lands back on it rather than on someone else's.
        let again = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("reallocation");
        assert_ne!(
            again, held,
            "a released pair keeps its LRU position, so a fresh pair is preferred"
        );
    })
    .await;
}

#[tokio::test]
async fn environment_guard_rejects_the_wrong_database() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");

        // A database with nothing bound is ambiguous and must be accepted by both.
        port::assert_environment_matches(&mut conn, Environment::Dev)
            .await
            .expect("empty database is valid for dev");
        port::assert_environment_matches(&mut conn, Environment::Prod)
            .await
            .expect("empty database is valid for prod");

        let generation = insert_generation(&mut conn).await;
        let room = insert_room(&mut conn, generation, "idle").await;
        port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("allocation");

        port::assert_environment_matches(&mut conn, Environment::Dev)
            .await
            .expect("dev bindings are valid for a dev process");

        // The failure this exists to catch: a prod process pointed at the dev database would
        // allocate from the wrong half of a shared port space.
        let err = port::assert_environment_matches(&mut conn, Environment::Prod)
            .await
            .expect_err("a prod process must refuse a dev database");
        assert!(
            err.to_string().contains("Refusing to start"),
            "error should explain the refusal, got: {err}"
        );
    })
    .await;
}

#[tokio::test]
async fn touch_live_rooms_only_moves_live_ones() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        let idle = insert_room(&mut conn, generation, "idle").await;
        let running = insert_room(&mut conn, generation, "running").await;
        let idle_port = port::allocate_pair(&orch, &mut conn, DEV, idle)
            .await
            .expect("allocation");
        port::allocate_pair(&orch, &mut conn, DEV, running)
            .await
            .expect("allocation");

        set_last_activity_ancient(&mut conn, idle_port).await;

        let touched = port::touch_live_rooms(&orch, &mut conn, DEV)
            .await
            .expect("touch");
        assert_eq!(touched, 1, "only the running room's reservation is touched");

        // The idle room stays oldest, so it remains the next victim -- which is the pre-API
        // degradation: LRU means "least recently running" rather than "least recently allocated".
        let newcomer = insert_room(&mut conn, generation, "idle").await;
        shrink_range_to_bound_only(&mut conn).await;
        let taken = port::allocate_pair(&orch, &mut conn, DEV, newcomer)
            .await
            .expect("allocation");
        assert_eq!(taken, idle_port, "the idle room should be reclaimed first");
    })
    .await;
}

// ---- helpers ----

async fn set_last_activity_ancient(conn: &mut diesel_async::AsyncPgConnection, base_port: u16) {
    use diesel_async::RunQueryDsl;
    diesel::sql_query(
        "UPDATE port_reservations SET last_activity = now() - interval '30 days'
          WHERE environment = 'dev' AND base_port = $1",
    )
    .bind::<diesel::sql_types::Integer, _>(base_port as i32)
    .execute(conn)
    .await
    .expect("age the reservation");
}

/// Drop every unbound pair, so the next allocation must reclaim.
async fn shrink_range_to_bound_only(conn: &mut diesel_async::AsyncPgConnection) {
    use diesel_async::RunQueryDsl;
    diesel::sql_query("DELETE FROM port_reservations WHERE room_id IS NULL")
        .execute(conn)
        .await
        .expect("drop unbound pairs");
}

async fn room_still_exists(
    conn: &mut diesel_async::AsyncPgConnection,
    room: puna_core::ids::RoomId,
) -> bool {
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    let rows: Vec<Row> = diesel::sql_query("SELECT count(*) AS n FROM rooms WHERE id = $1")
        .bind::<diesel::sql_types::Uuid, _>(room)
        .load(conn)
        .await
        .expect("count");
    rows.into_iter().next().map(|r| r.n).unwrap_or(0) == 1
}
