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
async fn path_2_never_allocated_ports_are_distinct_and_not_sequential() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        let mut seen = Vec::new();
        for _ in 0..8 {
            let room = insert_room(&mut conn, generation, "idle").await;
            seen.push(
                port::allocate_pair(&orch, &mut conn, DEV, room)
                    .await
                    .expect("allocation"),
            );
        }

        let (lo, hi) = common::port_range(&mut conn, "dev").await;

        // Every pair is distinct, even, and inside the range.
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "ports must not repeat: {seen:?}");
        for port in &seen {
            assert!((lo..=hi).contains(port), "{port} outside {lo}..={hi}");
            assert_eq!(port % 2, 0, "{port} must be an even base port");
        }

        // **The property this ordering exists for: a live port must not reveal how many rooms have
        // ever existed.** Filling from the bottom made the highest allocated port a room counter,
        // which is at its most telling early in an environment's life. So the allocations must not
        // be the lowest N pairs in order.
        //
        // This is probabilistic, and the probability is the reason it is safe to assert: with 2500
        // pairs, drawing exactly the lowest 8 is one chance in about 10^22. A failure here is a
        // change in the ordering, not luck.
        let lowest: Vec<u16> = (0..seen.len() as u16).map(|i| lo + i * 2).collect();
        assert_ne!(
            seen, lowest,
            "allocation is filling sequentially from the bottom of the range"
        );
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

        shrink_range(&mut conn, "dev", 2).await;

        // Bind ONE of the two pairs to an idle room, then age it so LRU ordering alone would pick
        // it. Which pair is arbitrary; the property is that the other one is taken anyway.
        let holder = insert_room(&mut conn, generation, "idle").await;
        let held = port::allocate_pair(&orch, &mut conn, DEV, holder)
            .await
            .expect("allocation");
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
        // port, which Cilium REFUSES rather than resolving: a Service requesting a specific
        // address gets none at all on conflict, so the room simply never starts.
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

        let room = insert_room(&mut conn, generation, "idle").await;
        // Which of the two pairs it lands on is arbitrary -- the tie among never-allocated pairs
        // is broken randomly -- and this test is about the quarantine, not the choice.
        let first = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("allocation");

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

/// **A narrowed range never blocks startup, and a room caught outside it moves.**
///
/// A range is configuration, so refusing to run because it changed would let one edit wedge the
/// orchestrator for a whole environment. Instead the reservation is released and — for a room that
/// is actually serving on that port — a restart is queued, which stops it and brings it back on a
/// valid port through the ordinary path.
#[tokio::test]
async fn narrowing_the_range_moves_a_live_room_rather_than_refusing() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        // Four pairs only, so both rooms land somewhere in a known low band -- WHICH of the four
        // each takes is random, so the window below is chosen to sit above all of them rather than
        // relative to whatever they happened to get.
        shrink_range(&mut conn, "dev", 4).await;
        let (lo, _) = common::port_range(&mut conn, "dev").await;

        let running = insert_room(&mut conn, generation, "running").await;
        let idle = insert_room(&mut conn, generation, "idle").await;
        let held = port::allocate_pair(&orch, &mut conn, DEV, running)
            .await
            .expect("a port");
        let idle_held = port::allocate_pair(&orch, &mut conn, DEV, idle)
            .await
            .expect("a port");

        // Strictly above every pair either room could be holding.
        let (window_lo, window_hi) = (lo + 8, lo + 20);
        port::ensure_range(&orch, &mut conn, DEV, (window_lo, window_hi))
            .await
            .expect("a narrowed range must not fail");

        // Neither keeps a reservation it could return to.
        assert_eq!(
            port::reserved_pair(&mut conn, running).await.expect("read"),
            None
        );
        assert_eq!(
            port::reserved_pair(&mut conn, idle).await.expect("read"),
            None
        );
        assert_ne!(held, idle_held, "they held different ports to begin with");

        // The serving one is queued for a restart; the idle one is left alone, because it is not
        // on that port and will take a valid one when it next starts.
        assert!(
            common::redeploy_requested(&mut conn, running).await,
            "a room serving on a now-invalid port must be moved"
        );
        assert!(
            !common::redeploy_requested(&mut conn, idle).await,
            "an idle room needs no restart -- there is nothing to move"
        );

        // And the room that has to move gets a port inside the new range.
        let fresh = port::allocate_pair(&orch, &mut conn, DEV, running)
            .await
            .expect("a fresh port");
        assert!(
            (window_lo..=window_hi).contains(&fresh),
            "reallocated inside the configured range, got {fresh}"
        );
    })
    .await;
}

/// The allocator's own guard, independent of startup reconciliation.
///
/// Step one of allocation is "the room's own previous pair", which is what makes a torn-down room
/// come back on the address its players already hold. That must not resurrect a reservation the
/// range no longer covers — the room would return to a port this deployment does not own, and the
/// collision is silent.
#[tokio::test]
async fn a_reservation_outside_the_range_is_never_handed_back() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let room = insert_room(&mut conn, generation, "idle").await;

        let held = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("a port");

        // **The window is derived from the SEEDED range, not from `held`, and that is what makes
        // this test deterministic.**
        //
        // It used to be `held + 100 ..= held + 300`, which reads as though `held` were the bottom
        // of the range. It is not: phase 2 of the allocator picks with `ORDER BY last_activity ASC,
        // random()`, and on a fresh database every unbound pair ties at `-infinity` -- so `held` is
        // a uniformly random pair anywhere in dev's 5000. Whenever it landed in the top 300 the
        // window ran off the end of the seeded rows, no reservation existed inside it, and the
        // allocator correctly answered `Exhausted`: the right answer to a question the test did
        // not mean to ask. Measured at 6 failures in 80 runs of the original.
        let (low, high) = common::port_range(&mut conn, "dev").await;
        let (window_lo, window_hi) = if held < low + (high - low) / 2 {
            (high - 200, high)
        } else {
            (low, low + 200)
        };
        assert!(
            !(window_lo..=window_hi).contains(&held),
            "the window has to exclude the held port, or this asserts nothing"
        );

        // Move the recorded range out from under the reservation WITHOUT reconciling the rows, so
        // the stale binding survives -- the state a mid-flight range change would leave behind.
        common::set_recorded_range(&mut conn, "dev", window_lo, window_hi).await;

        let fresh = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("a fresh port");
        assert_ne!(fresh, held, "the stale reservation must not be returned");
        assert!(
            (window_lo..=window_hi).contains(&fresh),
            "and the replacement is inside the range, got {fresh}"
        );
    })
    .await;
}

/// Widening adds capacity without disturbing what is already there, and narrowing removes only
/// what nobody holds.
///
/// The `last_activity` check is the subtle half: that column IS the LRU ordering, so a reconcile
/// that deleted and re-seeded rows would silently reset every port's age and make the allocator
/// hand out a recently-released port ahead of one idle for weeks.
#[tokio::test]
async fn reconciling_the_range_preserves_existing_reservations() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let room = insert_room(&mut conn, generation, "idle").await;

        let held = port::allocate_pair(&orch, &mut conn, DEV, room)
            .await
            .expect("a port");
        let (low, high) = common::port_range(&mut conn, "dev").await;

        // Narrow to a window that still contains the held port, then restore.
        port::ensure_range(&orch, &mut conn, DEV, (low, held + 10))
            .await
            .expect("narrowing around a held port is fine");
        assert_eq!(
            common::port_range(&mut conn, "dev").await,
            (low, held + 10),
            "the recorded range follows configuration"
        );
        assert_eq!(
            port::reserved_pair(&mut conn, room).await.expect("read"),
            Some(held),
            "the held reservation survived"
        );

        port::ensure_range(&orch, &mut conn, DEV, (low, high))
            .await
            .expect("widening back");
        assert_eq!(common::port_range(&mut conn, "dev").await, (low, high));

        // Idempotent: running it again writes nothing and changes nothing.
        port::ensure_range(&orch, &mut conn, DEV, (low, high))
            .await
            .expect("second pass");
        assert_eq!(
            port::reserved_pair(&mut conn, room).await.expect("read"),
            Some(held)
        );
    })
    .await;
}

/// **The other environment's rows are removed, and the range row only once they are gone.**
///
/// Every database is seeded with reservations for both environments and carries a backfilled range
/// row for each, but a database serves exactly one. The foreign rows are inert — `allocate` filters
/// on environment — and they are removed because `port_ranges` is what somebody reads to answer
/// "which ports does this environment own", where a stale row answers with a number that is no
/// longer true.
#[tokio::test]
async fn the_other_environments_rows_are_forgotten() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");

        // A fresh database has both, from the initial seed and the range backfill.
        assert!(common::reservation_count(&mut conn, "prod").await > 0);
        assert!(common::has_range_row(&mut conn, "prod").await);

        port::forget_foreign_environment(&orch, &mut conn, DEV)
            .await
            .expect("cleanup");

        assert_eq!(
            common::reservation_count(&mut conn, "prod").await,
            0,
            "the other environment's reservations are gone"
        );
        assert!(
            !common::has_range_row(&mut conn, "prod").await,
            "and so is its range row"
        );

        // This environment is untouched.
        assert!(common::reservation_count(&mut conn, "dev").await > 0);
        assert!(common::has_range_row(&mut conn, "dev").await);
    })
    .await;
}

/// The range row is kept while ANY foreign reservation survives.
///
/// The condition is a second opinion rather than the mechanism — the delete above should already
/// have emptied them. But a bound foreign reservation is a wrong-database misconfiguration, and in
/// that case the row documenting the other environment is exactly what a person needs to see rather
/// than something to tidy away.
#[tokio::test]
async fn a_surviving_foreign_reservation_keeps_its_range_row() {
    with_db(|pool| async move {
        let orch = Orchestrator::assume_leader();
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let squatter = insert_room(&mut conn, generation, "running").await;

        // A prod reservation bound to a room: the shape `assert_environment_matches` refuses to
        // start on, and which this cleanup must not erase.
        common::bind_foreign_reservation(&mut conn, "prod", squatter).await;

        port::forget_foreign_environment(&orch, &mut conn, DEV)
            .await
            .expect("cleanup");

        assert_eq!(
            common::reservation_count(&mut conn, "prod").await,
            1,
            "a bound foreign reservation is left alone"
        );
        assert!(
            common::has_range_row(&mut conn, "prod").await,
            "and its range row stays, because a reservation still references that environment"
        );
    })
    .await;
}
