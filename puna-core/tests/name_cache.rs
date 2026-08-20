//! Postgres-backed tests for the tracker's name cache.
//!
//! Gated on `DATABASE_URL` / `PUNA_REQUIRE_DB_TESTS` and **not** on `PUNA_TEST_GENERATION_ZIP`:
//! everything here works on synthetic tables, so it runs in CI, where there is a database and no
//! zip. The extraction from a real seed is `tests/names.rs`, which is gated the other way. Keeping
//! the two guards independent is deliberate — coupling them is what once made a suite pass locally
//! and skip silently in CI.

mod common;

use std::collections::BTreeMap;

use common::{insert_generation, with_db};
use puna_core::artifact::names::{GameNames, NameTables};
use puna_core::model::names;

fn game(items: &[(i64, &str)], locations: &[(i64, &str)]) -> GameNames {
    GameNames {
        items: items.iter().map(|(i, n)| (*i, n.to_string())).collect(),
        locations: locations.iter().map(|(i, n)| (*i, n.to_string())).collect(),
    }
}

fn tables() -> NameTables {
    let mut games = BTreeMap::new();
    games.insert(
        "A Link to the Past".to_string(),
        game(
            &[(1, "Progressive Sword"), (2, "Bow")],
            &[(100, "Link's House"), (101, "Eastern Palace - Big Chest")],
        ),
    );
    games.insert(
        "Timespinner".to_string(),
        // Overlapping ids with the game above, which is the whole reason names are keyed by game:
        // id 1 is a different item depending on whose world you are looking at.
        game(&[(1, "Talaria Attachment")], &[(100, "Lake Desolation")]),
    );

    let mut slot_locations = BTreeMap::new();
    slot_locations.insert(1, vec![100, 101]);
    slot_locations.insert(2, vec![100]);

    NameTables {
        games,
        slot_locations,
    }
}

#[tokio::test]
async fn names_round_trip_and_stay_scoped_to_their_game() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        names::store(&mut conn, generation, &tables())
            .await
            .expect("store");

        let alttp = names::game(&mut conn, generation, "A Link to the Past")
            .await
            .expect("query")
            .expect("the game is cached");
        assert_eq!(
            alttp.items.get(&1).map(String::as_str),
            Some("Progressive Sword")
        );
        assert_eq!(
            alttp.locations.get(&100).map(String::as_str),
            Some("Link's House")
        );

        // **The same id, a different game, a different name.** Item and location ids are namespaced
        // per game, so a cache that collapsed them would render confident nonsense -- the failure
        // mode with no symptom, because every name it produces is a real name of something.
        let timespinner = names::game(&mut conn, generation, "Timespinner")
            .await
            .expect("query")
            .expect("the game is cached");
        assert_eq!(
            timespinner.items.get(&1).map(String::as_str),
            Some("Talaria Attachment")
        );

        let all = names::all_games(&mut conn, generation)
            .await
            .expect("query");
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("Timespinner"), Some(&timespinner));
    })
    .await;
}

/// A missing row is normal, not an error: generations ingested before this table existed have none,
/// and the caller's job is to render the raw id rather than fail the page.
#[tokio::test]
async fn an_uncached_generation_reads_as_absent_rather_than_failing() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        assert_eq!(
            names::game(&mut conn, generation, "A Link to the Past")
                .await
                .expect("query"),
            None
        );
        assert!(
            names::all_games(&mut conn, generation)
                .await
                .expect("query")
                .is_empty()
        );
        assert_eq!(
            names::slot_locations(&mut conn, generation, 1)
                .await
                .expect("query"),
            None
        );
        assert!(
            !names::is_cached(&mut conn, generation)
                .await
                .expect("query")
        );
    })
    .await;
}

/// Storing twice replaces rather than conflicts, which is what makes a rebuild the same code path
/// as a first write — a repair needing its own statement would be a repair nobody had tested.
#[tokio::test]
async fn storing_again_replaces_what_was_there() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;

        names::store(&mut conn, generation, &tables())
            .await
            .expect("first store");

        let mut corrected = tables();
        corrected.games.insert(
            "A Link to the Past".to_string(),
            game(
                &[(1, "Progressive Sword (corrected)")],
                &[(100, "Elsewhere")],
            ),
        );
        corrected.slot_locations.insert(1, vec![100, 101, 102]);

        names::store(&mut conn, generation, &corrected)
            .await
            .expect("second store");

        let alttp = names::game(&mut conn, generation, "A Link to the Past")
            .await
            .expect("query")
            .expect("cached");
        assert_eq!(
            alttp.items.get(&1).map(String::as_str),
            Some("Progressive Sword (corrected)")
        );
        assert_eq!(
            names::slot_locations(&mut conn, generation, 1)
                .await
                .expect("query"),
            Some(vec![100, 101, 102])
        );
    })
    .await;
}

/// The backfill's work list: generations with nothing cached, and only those.
#[tokio::test]
async fn the_rebuild_list_holds_exactly_the_uncached_generations() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let cached = insert_generation(&mut conn).await;
        let uncached = insert_generation(&mut conn).await;

        names::store(&mut conn, cached, &tables())
            .await
            .expect("store");

        let pending = names::uncached(&mut conn).await.expect("query");
        let ids: Vec<_> = pending.iter().map(|g| g.id).collect();

        assert!(
            ids.contains(&uncached),
            "the uncached generation is missing"
        );
        assert!(
            !ids.contains(&cached),
            "a generation with names is in the rebuild list, so a backfill would redo it every run"
        );
        assert!(names::is_cached(&mut conn, cached).await.expect("query"));
    })
    .await;
}

/// Deleting a generation takes its cache with it. It is derived data, so leaving rows behind would
/// be an orphan nothing could ever explain or reclaim.
#[tokio::test]
async fn deleting_a_generation_deletes_its_names() {
    with_db(|pool| async move {
        use diesel_async::RunQueryDsl;

        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        names::store(&mut conn, generation, &tables())
            .await
            .expect("store");
        assert!(
            names::is_cached(&mut conn, generation)
                .await
                .expect("query")
        );

        diesel::sql_query("DELETE FROM generations WHERE id = $1")
            .bind::<diesel::sql_types::Uuid, _>(generation)
            .execute(&mut conn)
            .await
            .expect("delete");

        assert!(
            !names::is_cached(&mut conn, generation)
                .await
                .expect("query")
        );
        assert_eq!(
            names::slot_locations(&mut conn, generation, 1)
                .await
                .expect("query"),
            None
        );
    })
    .await;
}
