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

/// Which of the three credential changes asked for a sync, **for the wording alone**.
///
/// The mechanism does not branch on this — [`Dispatcher::sync_slot_credential`] pushes whatever the
/// row says either way — and that is the point of keeping it to a `&'static str`: an enum the
/// operation itself read would be a second source of truth beside the row it exists to obey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotChange {
    Rotated,
    Locked,
    Unlocked,
}

impl SlotChange {
    /// What happened, for the console line.
    fn done(self) -> &'static str {
        match self {
            Self::Rotated => "password rotated",
            Self::Locked => "locked; this slot can no longer connect",
            Self::Unlocked => "unlocked; this slot can connect again",
        }
    }

    /// What an old image could not do, for the degraded answer.
    fn verb(self) -> &'static str {
        match self {
            Self::Rotated => "change a password",
            Self::Locked | Self::Unlocked => "lock or unlock a slot",
        }
    }
}

pub struct Dispatcher {
    pool: Pool,
    prober: Arc<Prober>,
    /// Held for one command: [`RoomCommand::RotatePassword`] must write the room's Secret before it
    /// touches the running room, because the Secret is what survives a restart. Every other command
    /// is a passthrough to pahoa and needs nothing from the cluster.
    cluster: Arc<dyn crate::cluster::ClusterApi>,
}

impl Dispatcher {
    pub fn new(
        pool: Pool,
        prober: Arc<Prober>,
        cluster: Arc<dyn crate::cluster::ClusterApi>,
    ) -> Self {
        Self {
            pool,
            prober,
            cluster,
        }
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

    /// Make the running room agree with the row about one slot's credential: **Secret first, then
    /// the running room.**
    ///
    /// That order is the whole content of this function, and §4 is emphatic about why. The room's
    /// password endpoint changes the live process and **persists nothing** -- deliberately, because
    /// that is what stops a stale on-disk value shadowing the configured one. So a change pushed
    /// only to the room reverts the next time it starts: a rotation would hand a player a password
    /// that worked until the room bounced, and a lock would quietly lapse at a restart nobody
    /// decided on, like a reap or an image bump.
    ///
    /// **Rotation, locking and unlocking are one operation here**, and that is not a shortcut. The
    /// web tier has already written what it wanted to `room_slots` -- a new password, or
    /// `locked_at` -- and marked the Secret stale; all three then mean the same thing to this side,
    /// which is *make the pod match the row*. The value that goes out is whatever the row now says:
    /// `null` for a locked slot, and its password otherwise. One code path cannot disagree with
    /// itself about which of the three it is doing.
    async fn sync_slot_credential(
        &self,
        conn: &mut AsyncPgConnection,
        room_id: RoomId,
        slot_number: i32,
        change: SlotChange,
        endpoint: &puna_core::room::RoomEndpoint,
        admin_token: &str,
    ) -> (&'static str, Option<CommandOutput>, Option<String>) {
        let failed = |why: String| ("failed", None, Some(why));

        let (Ok(Some(room)), Ok(Some(secrets)), Ok(slots)) = (
            room::get(conn, room_id).await,
            room::secrets(conn, room_id).await,
            puna_core::model::slot::list(conn, room_id).await,
        ) else {
            return failed("could not read the room to render its Secret".into());
        };

        // A live read rather than the tick's snapshot: this runs on its own cadence and the
        // ownerReference has to name the Deployment that exists right now, or garbage collection
        // would not take the Secret away with the room.
        let name = crate::cluster::object_name(room_id);
        let owner = match self.cluster.get_deployment(&name).await {
            Ok(Some(deployment)) => crate::cluster::OwnerRef {
                name: deployment.name,
                uid: deployment.uid,
            },
            Ok(None) => return failed("the room's Deployment went away mid-rotation".into()),
            Err(e) => return failed(format!("could not read the room's Deployment: {e}")),
        };

        if let Err(e) = crate::sweep::apply_room_secret(
            conn,
            self.cluster.as_ref(),
            room_id,
            &room,
            &secrets,
            &slots,
            owner,
        )
        .await
        {
            // **Stop here.** Reaching the room now would make a change the next restart discards,
            // which is worse than not making it: the player is told a password that works until it
            // silently does not, or a locked slot lets somebody back in with nothing to say so.
            return failed(format!("the change was not written to the Secret: {e}"));
        }

        let Some(slot) = slots.iter().find(|s| s.slot_number == slot_number) else {
            return failed("that slot is no longer part of this room".into());
        };

        // **The row decides, not the command.** A locked slot gets `null`, which pahoa reads as a
        // refusal; anything else gets its stored password. Reading it back here rather than
        // carrying a value through the queue is also what keeps a credential out of the audit
        // trail.
        let live = if slot.is_locked() {
            None
        } else {
            match slot.password.as_deref() {
                Some(password) => Some(password),
                None => return failed("that slot has no password to push".into()),
            }
        };

        match self
            .prober
            .probe()
            .set_slot_password(endpoint, admin_token, slot_number, live)
            .await
        {
            Ok(()) => (
                "ok",
                Some(CommandOutput {
                    ok: true,
                    output: vec![format!("slot {slot_number}: {}", change.done())],
                    affected_slots: vec![slot_number],
                }),
                None,
            ),
            // The Secret is written, so the change is **durable** and takes effect at the room's
            // next start. Reported as an answer rather than a failure for that reason: nothing is
            // lost, and what did not happen is only the live push.
            Err(ProbeError::Unsupported { .. }) => (
                "ok",
                Some(CommandOutput {
                    ok: false,
                    output: vec![format!(
                        "the change is stored and takes effect when the room next starts; \
                         this room's image cannot {} on a running server",
                        change.verb()
                    )],
                    affected_slots: vec![slot_number],
                }),
                None,
            ),
            Err(e) => failed(format!("the room refused the change: {e}")),
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

        // **Handled before the passthrough, because neither is a pahoa command.** Serialized into
        // an `/admin/v1/command` body either would be a `400` -- pahoa's set is the other fourteen,
        // and both of these are its slot-password endpoint wearing a queue row.
        let credential_change = match claimed.command {
            puna_core::model::command::RoomCommand::RotatePassword { slot } => {
                Some((slot, SlotChange::Rotated))
            }
            puna_core::model::command::RoomCommand::LockSlot { slot, locked } => Some((
                slot,
                if locked {
                    SlotChange::Locked
                } else {
                    SlotChange::Unlocked
                },
            )),
            _ => None,
        };
        if let Some((slot, change)) = credential_change {
            return self
                .sync_slot_credential(
                    conn,
                    claimed.room_id,
                    slot,
                    change,
                    &endpoint,
                    &reachable.admin_token,
                )
                .await;
        }

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
    // The payload is the command id, and it is deliberately ignored: each pass claims every
    // pending command, so this only has to say "something arrived".
    puna_core::notify::listen(&database_url, REQUEST_CHANNEL, |_payload| wake.notify_one()).await;
}

#[cfg(test)]
mod tests {
    /// **Both Puna-side branches must come before the passthrough**, and this is a source lint
    /// because the failure is not a panic: it is a `400` from pahoa, logged as "the room could not
    /// understand a command Puna generated", which is true and points at the wrong thing entirely.
    ///
    /// Neither `RotatePassword` nor `LockSlot` is one of pahoa's fourteen commands — both are its
    /// slot-password endpoint traveling on this queue. Serialized into an `/admin/v1/command` body
    /// either is a shape the room has no parser for, so the intercept above is what makes them work
    /// at all, and its position is the whole of that.
    #[test]
    fn the_puna_side_commands_are_intercepted_before_the_command_passthrough() {
        let source = include_str!("dispatch.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("the file has a non-test half");

        // The match ARMS, not any mention of the variants. Anchoring on the bare type name matched
        // the doc comment on the `cluster` field forty lines above the intercept, so the lint
        // passed with the intercept deleted -- which is the exact failure a lint is for, found by
        // mutating it.
        let passthrough = source
            .find(".execute(&endpoint")
            .expect("the passthrough call was renamed; re-point this lint rather than deleting it");

        for (arm, what) in [
            (
                "puna_core::model::command::RoomCommand::RotatePassword { slot } =>",
                "rotation",
            ),
            (
                "puna_core::model::command::RoomCommand::LockSlot { slot, locked } =>",
                "locking",
            ),
        ] {
            let intercept = source
                .find(arm)
                .unwrap_or_else(|| panic!("the dispatcher no longer intercepts {what}"));
            assert!(
                intercept < passthrough,
                "the {what} command reaches pahoa's command endpoint, which has no such command"
            );
        }

        // And the branch is actually taken before the passthrough runs, not merely written above
        // it: the `if let` that consumes the match is what returns early.
        let taken = source
            .find("if let Some((slot, change)) = credential_change")
            .expect("the intercept no longer returns before the passthrough");
        assert!(taken < passthrough);
    }
}
