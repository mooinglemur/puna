//! Postgres-backed tests for the tracker's shared document cache.
//!
//! The SQL here is new and is the load-bearing half of M36's fix: the documents cross this boundary
//! as **text**, and the merge of the two into one column happens in the database rather than by
//! reading the column back into the process. Both of those are properties of a statement, so a
//! database is the only place they can be asserted.
//!
//! Gated on `DATABASE_URL` / `PUNA_REQUIRE_DB_TESTS` only — nothing here needs a real seed.

mod common;

use common::{insert_generation, insert_room, with_db};
use puna_core::model::tracker::{self, Kind};

/// A document big enough to be recognizable and small enough to be boring.
fn document(marker: &str) -> String {
    format!(r#"{{"marker":"{marker}","hints":[],"total_checks_done":{{"1":0}}}}"#)
}

const NO_CAP: usize = 1 << 20;

#[tokio::test]
async fn each_document_has_its_own_key_and_neither_write_evicts_the_other() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let room = insert_room(&mut conn, generation, "running").await;

        assert!(
            tracker::cached(&mut conn, room)
                .await
                .expect("read")
                .is_none(),
            "a room that has never been tracked has nothing cached"
        );

        assert!(
            tracker::store(&mut conn, room, Kind::Live, &document("live"), NO_CAP)
                .await
                .expect("store live")
        );
        assert!(
            tracker::store(&mut conn, room, Kind::Static, &document("static"), NO_CAP)
                .await
                .expect("store static")
        );

        // **The property the read-modify-write used to provide.** Writing one key merges into the
        // column rather than replacing it; a room whose live document was cached and whose static
        // one was not would render a slot table with no game names.
        let cached = tracker::cached(&mut conn, room)
            .await
            .expect("read")
            .expect("both documents");
        assert!(cached.live.as_deref().expect("live").contains("\"live\""));
        assert!(
            cached
                .statics
                .as_deref()
                .expect("static")
                .contains("\"static\"")
        );

        // And a re-store of one replaces only that one.
        assert!(
            tracker::store(&mut conn, room, Kind::Live, &document("newer"), NO_CAP)
                .await
                .expect("re-store live")
        );
        let cached = tracker::cached(&mut conn, room)
            .await
            .expect("read")
            .expect("both documents");
        assert!(cached.live.as_deref().expect("live").contains("\"newer\""));
        assert!(
            cached
                .statics
                .as_deref()
                .expect("static")
                .contains("\"static\"")
        );
    })
    .await;
}

/// What comes back is the document, as JSON text a proxy can serve without touching it.
#[tokio::test]
async fn a_stored_document_reads_back_as_the_document() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let room = insert_room(&mut conn, generation, "running").await;

        let body = document("live");
        tracker::store(&mut conn, room, Kind::Live, &body, NO_CAP)
            .await
            .expect("store");

        let cached = tracker::cached(&mut conn, room)
            .await
            .expect("read")
            .expect("a document");
        let read = cached.live.expect("live");

        // Equal as documents rather than byte for byte: `jsonb` normalizes whitespace and key
        // order, which is why the `ETag` a cache hit produces already differs from the one a fresh
        // fetch produces. What must hold is that a viewer is served the same *document*.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&read).expect("valid JSON came back"),
            serde_json::from_str::<serde_json::Value>(&body).expect("valid JSON went in"),
        );
    })
    .await;
}

/// **Over the cap the column is left alone rather than truncated**, so a room that is too big to
/// cache degrades to "fetched every time" instead of to a permanently unparseable column.
#[tokio::test]
async fn an_oversized_document_is_refused_and_leaves_what_is_there() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let room = insert_room(&mut conn, generation, "running").await;

        let keep = document("live");
        tracker::store(&mut conn, room, Kind::Live, &keep, NO_CAP)
            .await
            .expect("store");

        let huge = document("enormous");
        assert!(
            !tracker::store(&mut conn, room, Kind::Live, &huge, huge.len() - 1)
                .await
                .expect("store"),
            "a document over the cap must report that it was not stored"
        );

        let cached = tracker::cached(&mut conn, room)
            .await
            .expect("read")
            .expect("the earlier document");
        assert!(
            cached.live.as_deref().expect("live").contains("\"live\""),
            "the refused write overwrote the document that was already cached"
        );
    })
    .await;
}

/// The cast to `jsonb` is where the document is parsed at all, and it is Postgres that does it.
///
/// Worth pinning because it is the one thing the caller gave up by not parsing: a room answering
/// with something that is not JSON now fails here rather than at the fetch. The caller warns and
/// serves the body anyway, which is right — the room is what said it — so the only symptom is a
/// tracker that never caches, and this is what says why.
#[tokio::test]
async fn a_body_that_is_not_json_fails_rather_than_being_cached() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let generation = insert_generation(&mut conn).await;
        let room = insert_room(&mut conn, generation, "running").await;

        assert!(
            tracker::store(&mut conn, room, Kind::Live, "not json", NO_CAP)
                .await
                .is_err(),
            "the database accepted something that is not a document"
        );
    })
    .await;
}
