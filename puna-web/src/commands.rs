//! Waiting for a command to finish, without a connection per request.
//!
//! A console request wants to return the room's answer, not a job id — but the work happens in
//! another process, so the handler has to wait. The naive way is to poll the row, and the cost is
//! one query per waiter per interval; the naive fix is a `LISTEN` per request, and the cost is a
//! Postgres session per waiter.
//!
//! So: **one `LISTEN` connection per replica**, demultiplexing to in-process waiters through a map
//! keyed by command id. A hundred people watching a hundred commands is one connection and a
//! hundred `oneshot`s.
//!
//! ## Polling is still the contract
//!
//! The notification is latency, exactly as it is for the reconcile tick. If it does not arrive
//! within [`FIRST_POLL`] the handler starts reading the row anyway, so a dropped listener degrades
//! the console to a slightly slower console rather than a hung one — and the request gives up at
//! [`BUDGET`] with "still running" rather than holding a worker open indefinitely. The row stays
//! readable at `/room/<id>/command/<cid>` either way, which is what makes giving up safe.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use puna_core::db::Pool;
use puna_core::ids::CommandId;
use puna_core::model::command::{self, CommandRow, DONE_CHANNEL};

/// How long to wait for a notification before reading the row anyway.
const FIRST_POLL: Duration = Duration::from_millis(250);

/// How often to re-read once polling has started.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The whole request's budget. Past this the UI says "still running" and links to the row.
const BUDGET: Duration = Duration::from_secs(10);

/// In-process waiters, keyed by the command they are waiting for.
#[derive(Default)]
pub struct Waiters {
    entries: Mutex<HashMap<CommandId, Vec<tokio::sync::oneshot::Sender<()>>>>,
}

impl Waiters {
    /// A `Vec` per id rather than one sender: two people can watch the same command, and the second
    /// registering must not evict the first into an indefinite wait.
    fn register(&self, id: CommandId) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut entries) = self.entries.lock() {
            entries.entry(id).or_default().push(tx);
        }
        rx
    }

    pub fn wake(&self, id: CommandId) {
        if let Ok(mut entries) = self.entries.lock()
            && let Some(senders) = entries.remove(&id)
        {
            for sender in senders {
                // A closed receiver is a request that gave up or disconnected, which is ordinary.
                let _ = sender.send(());
            }
        }
    }

    fn forget(&self, id: CommandId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(&id);
        }
    }
}

/// Wait for a command to reach a terminal state, or run out of budget.
///
/// `None` means "still running" rather than "failed" — the caller shows that and links to the row.
pub async fn wait_for(pool: &Pool, waiters: &Arc<Waiters>, id: CommandId) -> Option<CommandRow> {
    // Registered BEFORE the first read, so a command that finishes between the two is not missed.
    // The other order is the classic lost-wakeup: read (not finished), dispatcher finishes and
    // notifies, then register and wait for a notification that has already been and gone.
    let notified = waiters.register(id);

    let finished = tokio::time::timeout(BUDGET, async {
        // The notification, if it is quick.
        if tokio::time::timeout(FIRST_POLL, notified).await.is_ok()
            && let Some(row) = read(pool, id).await
            && row.is_finished()
        {
            return Some(row);
        }

        // Otherwise poll. Reached when the listener is down, when the notification raced the
        // registration, and when the command is simply slow.
        loop {
            if let Some(row) = read(pool, id).await
                && row.is_finished()
            {
                return Some(row);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;

    waiters.forget(id);
    finished.ok().flatten()
}

async fn read(pool: &Pool, id: CommandId) -> Option<CommandRow> {
    let mut conn = pool.get().await.ok()?;
    command::get(&mut conn, id).await.ok().flatten()
}

/// Hold a `LISTEN` on the done channel and wake whoever is waiting.
///
/// Its own raw connection, because `LISTEN` is session-scoped and a pooled one is recycled between
/// callers. Losing it costs 250 ms per command, not correctness.
pub async fn listen(database_url: String, waiters: Arc<Waiters>) {
    puna_core::notify::listen(&database_url, DONE_CHANNEL, |payload| {
        // An id this build cannot parse is a notification from a newer schema, not an error worth
        // a log line per command: the waiter falls back to polling and finds the row anyway.
        if let Ok(id) = payload.parse::<CommandId>() {
            waiters.wake(id);
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two people can watch one command — a second organizer opening the console, or the same
    /// person in two tabs. The second registration must not evict the first.
    #[tokio::test]
    async fn two_waiters_on_one_command_are_both_woken() {
        let waiters = Waiters::default();
        let id = CommandId::new();

        let first = waiters.register(id);
        let second = waiters.register(id);

        waiters.wake(id);

        assert!(first.await.is_ok(), "the first waiter was evicted");
        assert!(second.await.is_ok(), "the second waiter was never woken");
    }

    /// Waking an id nobody is watching is ordinary: every replica sees every notification, and only
    /// one of them has the waiter.
    #[tokio::test]
    async fn waking_an_unwatched_command_is_harmless() {
        let waiters = Waiters::default();
        waiters.wake(CommandId::new());
    }

    /// A waiter that gave up leaves nothing behind, or the map grows for the life of the process.
    #[tokio::test]
    async fn forgetting_a_waiter_drops_it() {
        let waiters = Waiters::default();
        let id = CommandId::new();

        let receiver = waiters.register(id);
        waiters.forget(id);
        waiters.wake(id);

        assert!(
            receiver.await.is_err(),
            "a forgotten waiter still held a sender"
        );
        assert!(waiters.entries.lock().unwrap().is_empty());
    }
}
