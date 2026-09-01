//! That `LISTEN` actually listens.
//!
//! **This is a regression test for a bug that was live for weeks and invisible.**
//! `raw_connection_with_notifications` used to hand back the raw message *stream*, which is also
//! what drives the connection — so `client.batch_execute("LISTEN ...")` awaited a response that
//! could only arrive if somebody polled the stream, which the caller could not do while awaiting
//! the query. Every listener in Puna deadlocked on its first statement: no error, no log line, no
//! symptom beyond "NOTIFY never arrives".
//!
//! It survived because every caller has a polling fallback and the design says *NOTIFY is latency,
//! the tick is the contract* — so the only cost was that a room start waited up to 30 seconds for
//! the next tick instead of being immediate. Nothing was broken; everything was just slow, in a way
//! nobody had a number for.
//!
//! The lesson is the shape of the test rather than the fix: assert that a notification **arrives**,
//! end to end, because every weaker assertion passed the whole time.

use std::time::Duration;

use puna_core::notify;

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

#[tokio::test]
async fn a_notification_reaches_a_listener() {
    let Some(url) = database_url() else {
        assert!(
            std::env::var("PUNA_REQUIRE_DB_TESTS").is_err(),
            "PUNA_REQUIRE_DB_TESTS is set but DATABASE_URL is not"
        );
        eprintln!("DATABASE_URL unset; skipping");
        return;
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = tokio::spawn({
        let url = url.clone();
        async move {
            notify::listen(&url, "puna_test_channel", move |payload| {
                let _ = tx.send(payload.to_string());
            })
            .await;
        }
    });

    // The subscription is asynchronous, so give it a moment before firing. A notification sent
    // before `LISTEN` completes is genuinely lost: that is Postgres's behavior, not a bug here.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (client, _rx) = puna_core::db::raw_connection_with_notifications(&url)
        .await
        .expect("a connection to notify from");
    client
        .batch_execute("NOTIFY puna_test_channel, 'hello'")
        .await
        .expect("notify");

    // **The assertion the old code could never have passed.** Bounded rather than unbounded so a
    // regression fails in two seconds instead of hanging the suite forever, which is exactly how
    // the original bug behaved.
    let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a notification within five seconds")
        .expect("the listener is still running");

    assert_eq!(received, "hello");
    listener.abort();
}

/// The same statement the deadlock hit. A query on a listening client must complete — if the
/// connection is not being driven, this never returns.
#[tokio::test]
async fn a_query_on_a_listening_connection_completes() {
    let Some(url) = database_url() else {
        eprintln!("DATABASE_URL unset; skipping");
        return;
    };

    let (client, _notifications) = puna_core::db::raw_connection_with_notifications(&url)
        .await
        .expect("a connection");

    // Two statements, because the original failure was on the FIRST one and a fix that only made
    // one work would be a fix that happened to.
    for statement in ["LISTEN puna_test_channel", "SELECT 1"] {
        tokio::time::timeout(Duration::from_secs(5), client.batch_execute(statement))
            .await
            .unwrap_or_else(|_| panic!("{statement} never completed; the connection is not driven"))
            .unwrap_or_else(|e| panic!("{statement} failed: {e}"));
    }
}
