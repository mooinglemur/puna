//! Running console commands against rooms.
//!
//! **A dedicated task, deliberately not folded into the 30-second reconcile tick.** Somebody
//! pressing a button expects an answer in under a second, and the tick's cadence is chosen for
//! converging a fleet — the two cannot share a rhythm. They share nothing but the connection pool.
//!
//! ## Level-triggered, like everything else here
//!
//! `NOTIFY` is a wake-up, not the work list: each pass claims *every* pending command rather than
//! the one the notification named. That makes a lost notification cost latency rather than a
//! command, and it means a command queued while this process was down runs when it comes back
//! without any catch-up path of its own.
//!
//! ## What must be terminal, and why
//!
//! Every outcome writes a terminal state. A command left `pending` after a refusal would be
//! re-claimed on the next pass and re-run forever — and under pahoa's ten-failures-per-minute
//! limit, a loop locks Puna out of the room for the rest of the window, with the lockout applying
//! to the correct token too. [`Disposition`] is the type that keeps that decision in one place.
//!
//! ## A room that is not running is a REJECTION, not an auto-start
//!
//! Deliberately: a hint command silently provisioning a pod is surprising, and a cold start is a
//! multi-second visible state that deserves its own affordance. The console offers a Start button
//! instead.

use std::sync::Arc;
use std::time::Duration;

use diesel::sql_types::{Text, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use puna_core::db::Pool;
use puna_core::ids::RoomId;
use puna_core::model::command::{self, CommandOutput, CommandRow, Disposition, REQUEST_CHANNEL};
use puna_core::model::{port, room};
use puna_core::probe::ProbeError;

use crate::probing::Prober;

/// The backstop poll, for a notification that never arrived.
///
/// Short because it is a latency floor for a button press when `LISTEN` is down, not a sweep — the
/// query it runs finds nothing almost every time and costs an index scan.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a `running` row may sit before it is presumed abandoned.
///
/// Must exceed anything this process could legitimately still be doing, which the probe's own
/// timeout bounds at a few seconds. Anything older belongs to a dispatcher that went away.
const STALE_AFTER: Duration = Duration::from_secs(120);

/// How long to wait before reconnecting a dropped `LISTEN`.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

pub struct Dispatcher {
    pool: Pool,
    prober: Arc<Prober>,
}

impl Dispatcher {
    pub fn new(pool: Pool, prober: Arc<Prober>) -> Self {
        Self { pool, prober }
    }

    /// Listen, and drain the queue on every wake.
    pub async fn run(&self, database_url: String) {
        // **Before anything else**: a `running` row from a previous process will never finish on
        // its own, and a waiter is already timing out against it with no reason recorded.
        self.recover().await;

        let wake = Arc::new(tokio::sync::Notify::new());
        tokio::spawn(listen(database_url, Arc::clone(&wake)));

        loop {
            self.drain().await;

            // Either signal is enough; neither is required. A dropped listener degrades this to a
            // five-second console rather than a broken one.
            tokio::select! {
                () = wake.notified() => {}
                () = tokio::time::sleep(POLL_INTERVAL) => self.recover().await,
            }
        }
    }

    async fn recover(&self) {
        let Ok(mut conn) = self.pool.get().await else {
            return;
        };
        match command::fail_stale(&mut conn, STALE_AFTER).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(
                commands = n,
                "failed commands left running by a previous dispatcher"
            ),
            Err(e) => tracing::warn!(error = ?e, "could not sweep stale commands"),
        }
    }

    /// Claim and run until the queue is empty.
    async fn drain(&self) {
        loop {
            let Ok(mut conn) = self.pool.get().await else {
                return;
            };

            let claimed = match command::claim(&mut conn).await {
                Ok(Some(row)) => row,
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!(error = ?e, "could not claim a command");
                    return;
                }
            };

            let (state, result, error) = self.execute(&mut conn, &claimed).await;
            if let Err(e) = command::finish(
                &mut conn,
                claimed.id,
                state,
                result.as_ref(),
                error.as_deref(),
            )
            .await
            {
                // The row stays `running` and the stale sweep will fail it. Worse than answering,
                // better than losing the record -- and a waiter times out rather than hanging.
                tracing::error!(command = %claimed.id, error = ?e, "could not record a command result");
                return;
            }

            puna_core::metrics::COMMANDS
                .with_label_values(&[claimed.command.name(), state])
                .inc();

            tracing::info!(
                command = %claimed.id,
                room = %claimed.room_id,
                kind = claimed.command.name(),
                requested_by = claimed.requested_by,
                state,
                "console command finished"
            );
        }
    }

    /// Run one command, and classify what came back.
    async fn execute(
        &self,
        conn: &mut AsyncPgConnection,
        claimed: &CommandRow,
    ) -> (&'static str, Option<CommandOutput>, Option<String>) {
        let _timer = puna_core::metrics::COMMAND_SECONDS.start_timer();

        if !self.prober.probe().capabilities().commands {
            return (
                "rejected",
                None,
                Some("this room's image is too old to accept console commands".into()),
            );
        }

        let Some(reachable) = self.reachable(conn, claimed.room_id).await else {
            // **Rejected, not failed.** Nothing is wrong; the room is simply not up, and the
            // distinction is what lets the console offer a Start button rather than an error.
            return (
                "rejected",
                None,
                Some("this room is not running; start it and try again".into()),
            );
        };

        let endpoint = self.prober.endpoint(claimed.room_id, reachable.base_port);
        match self
            .prober
            .probe()
            .execute(&endpoint, &reachable.admin_token, &claimed.command)
            .await
        {
            // Includes a refusal: `ok: false` is the room's ANSWER, and lands in a terminal `ok`
            // with `output` saying why. Retrying it would loop forever.
            Ok(output) => (Disposition::Answered.state(), Some(output), None),

            Err(e) => {
                let disposition = match &e {
                    ProbeError::Room(puna_core::room::RoomError::Status { status }) => {
                        Disposition::from_status(*status)
                    }
                    ProbeError::Room(puna_core::room::RoomError::RateLimited { .. }) => {
                        Disposition::RateLimited
                    }
                    ProbeError::Unsupported { .. } => Disposition::Malformed,
                    _ => Disposition::Failed,
                };

                if disposition == Disposition::Malformed {
                    // A `400` means the typed set and pahoa's parser have drifted -- Puna generated
                    // a body the room could not read. That is a bug on this side, not a caller's,
                    // so it is loud rather than a line in a console pane.
                    tracing::error!(
                        command = %claimed.id,
                        kind = claimed.command.name(),
                        error = %e,
                        "the room could not understand a command Puna generated; the command set \
                         and the room's image have drifted"
                    );
                }

                (disposition.state(), None, Some(e.to_string()))
            }
        }
    }

    /// The room's port and token, if it is up enough to be asked.
    async fn reachable(&self, conn: &mut AsyncPgConnection, id: RoomId) -> Option<Reachable> {
        #[derive(diesel::QueryableByName)]
        struct Row {
            #[diesel(sql_type = Text)]
            state: String,
        }

        let rows: Vec<Row> =
            diesel::sql_query("SELECT state::text AS state FROM rooms WHERE id = $1")
                .bind::<SqlUuid, _>(id)
                .load(conn)
                .await
                .ok()?;

        // `running` only. A `degraded` room has no ready replica, so a command would land on a pod
        // that is restarting -- rejected with "not running" is the honest answer, and the console
        // shows the room's real state beside it.
        if rows.into_iter().next()?.state != "running" {
            return None;
        }

        Some(Reachable {
            base_port: port::reserved_pair(conn, id).await.ok().flatten()?,
            admin_token: room::secrets(conn, id).await.ok().flatten()?.admin_token,
        })
    }
}

struct Reachable {
    base_port: u16,
    admin_token: String,
}

/// Hold a `LISTEN` on the request channel and poke `wake`.
///
/// Its own raw connection, because `LISTEN` is session-scoped and a pooled one is recycled between
/// callers. Losing it costs latency: the backstop poll still drains the queue.
async fn listen(database_url: String, wake: Arc<tokio::sync::Notify>) {
    use futures_util::StreamExt;

    loop {
        match puna_core::db::raw_connection_with_notifications(&database_url).await {
            Ok((client, mut notifications)) => {
                if let Err(e) = client
                    .batch_execute(&format!("LISTEN {REQUEST_CHANNEL}"))
                    .await
                {
                    tracing::warn!(error = %e, "LISTEN failed; the console falls back to polling");
                } else {
                    tracing::info!(channel = REQUEST_CHANNEL, "listening for console commands");
                    while let Some(message) = notifications.next().await {
                        if matches!(message, tokio_postgres::AsyncMessage::Notification(_)) {
                            wake.notify_one();
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not open a command LISTEN connection"),
        }

        tracing::warn!("command LISTEN connection lost; polling until it returns");
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}
