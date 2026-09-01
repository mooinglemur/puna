//! Postgres-backed tests for who holds a reference to a generation.
//!
//! Deduplication is content-addressed and global — two people uploading one zip get one row and one
//! copy of the bytes — while a *reference* is per person. Every property here sits on that seam,
//! and two of them are not merely correctness:
//!
//!   * **A second uploader must not learn the seed was already here.** The signal the page renders
//!     comes from [`generation::record_upload`], which answers about the caller alone, never from
//!     `Insertion::created`, which answers about everybody. The two diverge in exactly one case,
//!     and that case is the whole reason the table exists.
//!   * **A listing shows the reader's OWN upload time.** The generation's `created_at` belongs to
//!     whoever got there first, and rendering it under a column headed "Uploaded" would both
//!     misdate a second uploader's entry and disclose that the seed predates their upload.

mod common;

use common::with_db;
use puna_core::ids::GenerationId;
use puna_core::model::{generation, user};

use diesel::sql_types::{BigInt, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

const ALICE: i64 = 101;
const BOB: i64 = 202;

/// A generation with no uploader recorded, so each test states its own references.
///
/// Deliberately not `common::insert_generation` plus a reference: the point of most of these is
/// which references exist, so they are never created as a side effect of the fixture.
async fn a_generation(conn: &mut AsyncPgConnection) -> GenerationId {
    let id = GenerationId::new();
    diesel::sql_query(
        "INSERT INTO generations (id, sha256, size_bytes, seed_name, slots, locations)
         VALUES ($1, decode(md5(random()::text) || md5(random()::text), 'hex'), 1, 'seed', 1, 1)",
    )
    .bind::<SqlUuid, _>(id)
    .execute(conn)
    .await
    .expect("insert generation");
    id
}

async fn reference_count(conn: &mut AsyncPgConnection, generation: GenerationId) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }
    let rows: Vec<Row> =
        diesel::sql_query("SELECT count(*) AS n FROM generation_uploads WHERE generation_id = $1")
            .bind::<SqlUuid, _>(generation)
            .load(conn)
            .await
            .expect("count");
    rows[0].n
}

/// One account, one zip, twice: told about it, and holding exactly one reference.
///
/// The count is the half that would fail silently. A page saying the right thing over a table that
/// had accumulated a row per upload would look entirely correct until somebody deleted one.
#[tokio::test]
async fn uploading_your_own_zip_twice_is_one_reference_and_you_are_told() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        user::ensure_exists(&mut conn, ALICE).await.expect("user");
        let generation = a_generation(&mut conn).await;

        assert!(
            generation::record_upload(&mut conn, generation, ALICE)
                .await
                .expect("first"),
            "the first upload is news"
        );
        assert!(
            !generation::record_upload(&mut conn, generation, ALICE)
                .await
                .expect("second"),
            "the second is a duplicate TO ALICE, which is what the page reports"
        );

        assert_eq!(
            reference_count(&mut conn, generation).await,
            1,
            "a repeat upload converges rather than accumulating"
        );
        assert_eq!(
            generation::list_for_user(&mut conn, ALICE, 50)
                .await
                .expect("list")
                .len(),
            1,
            "and it appears once in the listing"
        );
    })
    .await;
}

/// **The disclosure property.** Bob uploads a zip Alice already uploaded: he gets a reference, and
/// the answer he is shown is indistinguishable from a first upload of a seed nobody had.
///
/// If this ever reads `false` for Bob, the page tells him another account holds the same seed —
/// which is exactly what the reference table was introduced to stop.
#[tokio::test]
async fn a_second_uploader_gains_a_reference_and_learns_nothing() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        for id in [ALICE, BOB] {
            user::ensure_exists(&mut conn, id).await.expect("user");
        }

        let shared = a_generation(&mut conn).await;
        generation::record_upload(&mut conn, shared, ALICE)
            .await
            .expect("alice");

        let bob_was_told_it_is_new = generation::record_upload(&mut conn, shared, BOB)
            .await
            .expect("bob");

        // A control: a seed genuinely nobody has. Bob's two uploads must be indistinguishable.
        let fresh = a_generation(&mut conn).await;
        let bob_uploads_something_nobody_has = generation::record_upload(&mut conn, fresh, BOB)
            .await
            .expect("bob again");

        assert_eq!(
            bob_was_told_it_is_new, bob_uploads_something_nobody_has,
            "uploading a seed somebody else already has must look exactly like uploading a new one"
        );
        assert!(bob_was_told_it_is_new, "and both are new TO BOB");

        assert_eq!(
            reference_count(&mut conn, shared).await,
            2,
            "both accounts hold a reference to the one row"
        );

        // Both listings carry it, and neither is missing anything.
        for user_id in [ALICE, BOB] {
            let listed = generation::list_for_user(&mut conn, user_id, 50)
                .await
                .expect("list");
            assert!(
                listed.iter().any(|u| u.generation.id == shared),
                "{user_id} uploaded it and must see it"
            );
        }
    })
    .await;
}

/// The listing carries the READER's upload time, not the generation's.
///
/// Asserted by making them provably different: the generation and Alice's reference are backdated,
/// Bob's is not. Rendering `generation.created_at` here would date Bob's entry to Alice's day —
/// wrong on its face, and a disclosure that the seed predates him.
#[tokio::test]
async fn each_uploader_sees_their_own_upload_time() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        for id in [ALICE, BOB] {
            user::ensure_exists(&mut conn, id).await.expect("user");
        }

        let generation = a_generation(&mut conn).await;
        generation::record_upload(&mut conn, generation, ALICE)
            .await
            .expect("alice");

        // Alice's upload, and the generation itself, are three weeks old.
        for statement in [
            "UPDATE generations SET created_at = now() - interval '21 days' WHERE id = $1",
            "UPDATE generation_uploads SET uploaded_at = now() - interval '21 days'
              WHERE generation_id = $1",
        ] {
            diesel::sql_query(statement)
                .bind::<SqlUuid, _>(generation)
                .execute(&mut conn)
                .await
                .expect("backdate");
        }

        generation::record_upload(&mut conn, generation, BOB)
            .await
            .expect("bob");

        let alice = generation::list_for_user(&mut conn, ALICE, 50)
            .await
            .expect("list");
        let bob = generation::list_for_user(&mut conn, BOB, 50)
            .await
            .expect("list");

        let age = |at: chrono::DateTime<chrono::Utc>| (chrono::Utc::now() - at).num_days();
        assert_eq!(
            age(alice[0].uploaded_at),
            21,
            "alice uploaded it three weeks ago"
        );
        assert_eq!(age(bob[0].uploaded_at), 0, "bob uploaded it just now");

        // And the generation's own date is the old one for both, which is why it cannot be the
        // value the listing renders.
        assert_eq!(age(bob[0].generation.created_at), 21);
    })
    .await;
}

/// A repeat upload keeps the original `uploaded_at`.
///
/// Re-uploading is the same act, not a newer one. Touching the timestamp would jump the entry to
/// the top of the reader's listing for no reason they could see — and would make the ordering
/// depend on how many times somebody happened to re-upload.
#[tokio::test]
async fn re_uploading_does_not_move_your_entry() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        user::ensure_exists(&mut conn, ALICE).await.expect("user");

        let old = a_generation(&mut conn).await;
        generation::record_upload(&mut conn, old, ALICE)
            .await
            .expect("first");
        diesel::sql_query(
            "UPDATE generation_uploads SET uploaded_at = now() - interval '30 days'
              WHERE generation_id = $1",
        )
        .bind::<SqlUuid, _>(old)
        .execute(&mut conn)
        .await
        .expect("backdate");

        let recent = a_generation(&mut conn).await;
        generation::record_upload(&mut conn, recent, ALICE)
            .await
            .expect("second generation");

        // The newer UPLOAD is of the older GENERATION, so the two possible sort keys disagree.
        // Without this the test passes against an `ORDER BY generations.created_at`, which is the
        // mutation that would sink a second uploader's brand new entry to the bottom of their list,
        // and make its position report how old the seed is.
        diesel::sql_query(
            "UPDATE generations SET created_at = now() - interval '60 days' WHERE id = $1",
        )
        .bind::<SqlUuid, _>(recent)
        .execute(&mut conn)
        .await
        .expect("backdate the generation, not the upload");

        // Re-upload the old one. Newest-first ordering must not put it back on top.
        generation::record_upload(&mut conn, old, ALICE)
            .await
            .expect("re-upload");

        let listed = generation::list_for_user(&mut conn, ALICE, 50)
            .await
            .expect("list");
        assert_eq!(
            listed.iter().map(|u| u.generation.id).collect::<Vec<_>>(),
            vec![recent, old],
            "the re-uploaded entry keeps its place"
        );
    })
    .await;
}

/// One user's listing is only their own, which is the property that survived from the old
/// `first_ingested_by` scoping and must not have been widened by the join.
#[tokio::test]
async fn a_listing_never_shows_somebody_elses_upload() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        for id in [ALICE, BOB] {
            user::ensure_exists(&mut conn, id).await.expect("user");
        }

        let hers = a_generation(&mut conn).await;
        generation::record_upload(&mut conn, hers, ALICE)
            .await
            .expect("alice");

        assert!(
            generation::list_for_user(&mut conn, BOB, 50)
                .await
                .expect("list")
                .is_empty(),
            "bob uploaded nothing and must see nothing"
        );
    })
    .await;
}
