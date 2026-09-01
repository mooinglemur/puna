//! Postgres-backed tests for the console's command queue.
//!
//! The queue is what makes a console command auditable and survivable, and the two properties that
//! matter are both about *exactly once*: two dispatchers must not run one command, and a command
//! must never be left in a state that invites a re-run.

mod common;

use std::time::Duration;

use common::{insert_generation, insert_room, with_db};
use puna_core::model::command::{self, CommandOutput, RoomCommand};
use puna_core::model::member::RoomRole;
use puna_core::model::user;

const TROY: i64 = 100;

async fn a_room(conn: &mut diesel_async::AsyncPgConnection) -> puna_core::ids::RoomId {
    user::upsert(conn, TROY, "troy").await.expect("a user");
    let generation = insert_generation(conn).await;
    insert_room(conn, generation, "running").await
}

#[tokio::test]
async fn a_command_round_trips_through_the_queue() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        let command = RoomCommand::Hint {
            slot: 3,
            item: "Progressive Sword".into(),
            force: true,
        };
        let id = command::enqueue(&mut conn, room, TROY, RoomRole::Helper, &command)
            .await
            .expect("enqueue");

        let queued = command::get(&mut conn, id)
            .await
            .expect("get")
            .expect("the row");
        assert_eq!(queued.command, command, "the typed command survived JSONB");
        assert_eq!(queued.state, "pending");
        assert_eq!(queued.requested_role, RoomRole::Helper);
        assert!(!queued.is_finished());

        let claimed = command::claim(&mut conn)
            .await
            .expect("claim")
            .expect("a row");
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.state, "running");

        let output = CommandOutput {
            ok: true,
            output: vec!["hinted Progressive Sword for Troy".into()],
            affected_slots: vec![3],
        };
        command::finish(&mut conn, id, "ok", Some(&output), None)
            .await
            .expect("finish");

        let done = command::get(&mut conn, id)
            .await
            .expect("get")
            .expect("the row");
        assert_eq!(done.state, "ok");
        assert!(done.is_finished());
        assert_eq!(done.result, Some(output));
        assert!(done.finished_at.is_some());
    })
    .await;
}

/// **The property the conditional claim exists for.** Two dispatchers racing one row must produce
/// exactly one execution — otherwise a `release` runs twice, which for a player is items appearing
/// out of nowhere and cannot be undone.
#[tokio::test]
async fn two_dispatchers_racing_one_command_execute_it_once() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        command::enqueue(
            &mut conn,
            room,
            TROY,
            RoomRole::Organizer,
            &RoomCommand::Release { slot: 1 },
        )
        .await
        .expect("enqueue");

        // Two connections, as two dispatchers would be.
        let mut first = pool.get().await.expect("connection");
        let mut second = pool.get().await.expect("connection");

        let a = command::claim(&mut first).await.expect("claim");
        let b = command::claim(&mut second).await.expect("claim");

        assert!(a.is_some() ^ b.is_some(), "exactly one dispatcher gets it");

        // And the queue is empty afterwards: a claimed row is not pending.
        assert!(
            command::claim(&mut conn).await.expect("claim").is_none(),
            "a running command was claimed a second time"
        );
    })
    .await;
}

/// A refusal is a **terminal** state carrying the room's own words. Leaving it non-terminal would
/// re-run it every pass, and under pahoa's ten-per-minute limit that locks Puna out of the room.
#[tokio::test]
async fn a_refusal_is_terminal_and_keeps_the_rooms_own_words() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        let id = command::enqueue(
            &mut conn,
            room,
            TROY,
            RoomRole::Organizer,
            &RoomCommand::Kick {
                slot: 9,
                reason: None,
            },
        )
        .await
        .expect("enqueue");
        command::claim(&mut conn).await.expect("claim");

        // `ok: false`: the room understood and said no.
        let refusal = CommandOutput {
            ok: false,
            output: vec!["no such slot: 9".into()],
            affected_slots: Vec::new(),
        };
        command::finish(&mut conn, id, "ok", Some(&refusal), None)
            .await
            .expect("finish");

        let done = command::get(&mut conn, id)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(done.state, "ok", "a refusal is an answer, not a failure");
        assert!(done.is_finished());
        assert_eq!(
            done.result.as_ref().map(|r| r.ok),
            Some(false),
            "and the answer says no"
        );
        assert_eq!(done.result.unwrap().output, vec!["no such slot: 9"]);

        // Terminal means terminal: it is not claimable again.
        assert!(command::claim(&mut conn).await.expect("claim").is_none());
    })
    .await;
}

/// A dispatcher that goes away mid-command leaves a `running` row nobody will finish. Without this
/// sweep the waiter times out and the row stays `running` forever, with no record of why.
#[tokio::test]
async fn a_command_abandoned_by_a_dead_dispatcher_is_failed_with_a_reason() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        let id = command::enqueue(
            &mut conn,
            room,
            TROY,
            RoomRole::Helper,
            &RoomCommand::Status,
        )
        .await
        .expect("enqueue");
        command::claim(&mut conn).await.expect("claim");

        // Not yet stale: a command in flight must survive the sweep, or every long command dies.
        let swept = command::fail_stale(&mut conn, Duration::from_secs(120))
            .await
            .expect("sweep");
        assert_eq!(swept, 0, "an in-flight command was swept");
        assert_eq!(
            command::get(&mut conn, id)
                .await
                .expect("get")
                .unwrap()
                .state,
            "running"
        );

        // Older than the window: presumed abandoned.
        let swept = command::fail_stale(&mut conn, Duration::from_secs(0))
            .await
            .expect("sweep");
        assert_eq!(swept, 1);

        let done = command::get(&mut conn, id).await.expect("get").unwrap();
        assert_eq!(done.state, "failed");
        assert!(
            done.error.unwrap().contains("orchestrator restarted"),
            "the reason has to survive, or the console shows a command that just stopped"
        );
    })
    .await;
}

/// Oldest first, so a queue of commands runs in the order somebody pressed the buttons.
#[tokio::test]
async fn commands_are_claimed_in_the_order_they_were_requested() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        let mut ids = Vec::new();
        for seconds in [10, 5, 1] {
            let id = command::enqueue(
                &mut conn,
                room,
                TROY,
                RoomRole::Helper,
                &RoomCommand::Say {
                    text: format!("{seconds}"),
                },
            )
            .await
            .expect("enqueue");

            diesel_async::RunQueryDsl::execute(
                diesel::sql_query(
                    "UPDATE room_commands SET requested_at = now() - make_interval(secs => $2)
                      WHERE id = $1",
                )
                .bind::<diesel::sql_types::Uuid, _>(id)
                .bind::<diesel::sql_types::Double, _>(f64::from(seconds)),
                &mut conn,
            )
            .await
            .expect("age the row");

            ids.push((seconds, id));
        }

        // Claimed oldest-first: 10 seconds ago, then 5, then 1.
        for expected in [10, 5, 1] {
            let claimed = command::claim(&mut conn)
                .await
                .expect("claim")
                .expect("a row");
            let (_, id) = ids.iter().find(|(s, _)| *s == expected).expect("the row");
            assert_eq!(claimed.id, *id, "claimed out of order at {expected}s");
        }
    })
    .await;
}

/// The console's history pane, newest first.
#[tokio::test]
async fn a_rooms_recent_commands_read_newest_first() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        for text in ["first", "second", "third"] {
            command::enqueue(
                &mut conn,
                room,
                TROY,
                RoomRole::Helper,
                &RoomCommand::Say { text: text.into() },
            )
            .await
            .expect("enqueue");
        }

        let recent = command::recent(&mut conn, room, 10).await.expect("recent");
        assert_eq!(recent.len(), 3);
        assert!(
            recent[0].requested_at >= recent[2].requested_at,
            "the history pane is newest first"
        );

        assert_eq!(
            command::recent(&mut conn, room, 2)
                .await
                .expect("recent")
                .len(),
            2,
            "the limit is honored"
        );
    })
    .await;
}

/// A batch is enqueued whole, reads back in order, and belongs to its room.
#[tokio::test]
async fn a_batch_is_all_or_nothing_and_scoped_to_its_room() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;
        let generation = insert_generation(&mut conn).await;
        let other = insert_room(&mut conn, generation, "running").await;

        let commands: Vec<RoomCommand> =
            (1..=3).map(|slot| RoomCommand::Release { slot }).collect();
        let batch = command::enqueue_batch(&mut conn, room, TROY, RoomRole::Helper, &commands)
            .await
            .expect("enqueue")
            .expect("a batch id for a non-empty batch");

        let rows = command::batch(&mut conn, room, batch).await.expect("batch");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().map(|r| r.command.clone()).collect::<Vec<_>>(),
            commands,
            "staged order is the order they read back in"
        );

        // **A batch id is not a capability.** The same id asked for against another room answers
        // nothing, or holding one would be a way to read commands the guard never authorized.
        assert!(
            command::batch(&mut conn, other, batch)
                .await
                .expect("batch")
                .is_empty(),
            "a batch must not be readable through a room it does not belong to"
        );

        // An empty stage mints no id: a page that exists and lists nothing is worse than the route
        // saying there was nothing to do.
        assert_eq!(
            command::enqueue_batch(&mut conn, room, TROY, RoomRole::Helper, &[])
                .await
                .expect("empty"),
            None
        );
    })
    .await;
}

/// **Refused is not failed**, and the three buckets have to stay three.
///
/// The case is not hypothetical: a bulk `set_status` over a sync where some slots have already
/// goaled produces refusals that are the correct answer, and rendering them as errors is how the
/// bucket that matters gets ignored.
#[tokio::test]
async fn a_batchs_outcome_separates_a_refusal_from_a_failure() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        let commands: Vec<RoomCommand> =
            (1..=4).map(|slot| RoomCommand::Release { slot }).collect();
        let batch = command::enqueue_batch(&mut conn, room, TROY, RoomRole::Helper, &commands)
            .await
            .expect("enqueue")
            .expect("a batch");

        let staged = command::batch(&mut conn, room, batch).await.expect("batch");
        let outcome = command::BatchOutcome::of(&staged);
        assert_eq!(outcome.outstanding, 4, "nothing has run yet");
        assert!(!outcome.is_finished());

        let yes = CommandOutput {
            ok: true,
            output: vec!["done".into()],
            ..Default::default()
        };
        let no = CommandOutput {
            ok: false,
            output: vec!["that slot has already goaled".into()],
            ..Default::default()
        };

        command::finish(&mut conn, staged[0].id, "ok", Some(&yes), None)
            .await
            .expect("succeeded");
        command::finish(&mut conn, staged[1].id, "ok", Some(&no), None)
            .await
            .expect("refused");
        command::finish(&mut conn, staged[2].id, "failed", None, Some("no route"))
            .await
            .expect("failed");

        let outcome =
            command::BatchOutcome::of(&command::batch(&mut conn, room, batch).await.unwrap());
        assert_eq!(outcome.succeeded, 1);
        assert_eq!(
            outcome.refused, 1,
            "a 200 with ok:false is the room answering no, not a fault"
        );
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.outstanding, 1);
        assert_eq!(outcome.total(), 4);
        assert!(!outcome.is_finished(), "one is still in flight");

        command::finish(
            &mut conn,
            staged[3].id,
            "rejected",
            None,
            Some("not running"),
        )
        .await
        .expect("rejected");
        let outcome =
            command::BatchOutcome::of(&command::batch(&mut conn, room, batch).await.unwrap());
        assert!(outcome.is_finished());
        assert_eq!(outcome.failed, 2, "a rejection is a failure, not a refusal");
    })
    .await;
}
