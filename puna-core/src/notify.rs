//! Holding a `LISTEN`, and getting it back when it drops.
//!
//! Three places want this — the orchestrator's wake channel, its command channel, and the web
//! tier's result channel — and they all want the same loop: open a raw connection, subscribe,
//! forward payloads, and reconnect when it dies. Written once because the interesting part is the
//! *reconnect*, and three copies is three chances to get the retry wrong.
//!
//! ## Losing it is latency, never correctness
//!
//! Every caller has a fallback that does not depend on this: the reconcile tick runs on its
//! interval, the dispatcher polls the queue, and a console request polls the row. That is what
//! makes a permanently-failing connection an annoyance rather than an outage — and it is why this
//! logs a warning and sleeps rather than propagating an error nobody could act on.
//!
//! `LISTEN` is session-scoped, so this needs its own connection: a pooled one is recycled between
//! callers and the subscription would go with it.

use std::time::Duration;

/// How long before reopening a connection that dropped.
///
/// Short enough that a database restart costs seconds of latency, long enough that a database
/// refusing connections is not hammered by every tier at once.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Subscribe to `channel` and hand each payload to `on_payload`, forever.
///
/// Never returns. The caller spawns it and, if it needs to stop, aborts the task.
///
/// `on_payload` runs on the connection's own task, so it must not block or await: its job is to
/// poke something — a `Notify`, a waiter map — and return. Anything slower would stall the stream
/// and delay every later notification on the same channel.
pub async fn listen<F>(database_url: &str, channel: &str, mut on_payload: F)
where
    F: FnMut(&str) + Send,
{
    loop {
        match crate::db::raw_connection_with_notifications(database_url).await {
            Ok((client, mut notifications)) => {
                // The channel name is a compile-time constant at every call site, never user input:
                // `LISTEN` takes an identifier, which cannot be parameterized.
                if let Err(e) = client.batch_execute(&format!("LISTEN {channel}")).await {
                    tracing::warn!(channel, error = %e, "LISTEN failed; falling back to polling");
                } else {
                    tracing::info!(channel, "listening");
                    while let Some(note) = notifications.recv().await {
                        on_payload(note.payload());
                    }
                }
            }
            Err(e) => tracing::warn!(channel, error = %e, "could not open a LISTEN connection"),
        }

        tracing::warn!(channel, "LISTEN connection lost; polling until it returns");
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}
