//! The state machine, as a pure function.
//!
//! `(rooms, cluster, now) -> actions`. No database, no filesystem, no cluster, no clock — every
//! input is an argument, so every transition in §3's table is a line in a test rather than a
//! sequence somebody has to reproduce against a live room.
//!
//! ## Deciding and doing are separated on purpose
//!
//! This module decides; [`Step`] is what it decides; the applier does. The value of the split is
//! that the interesting failures here are all *decisions* — starting a room that should be idle,
//! marking a room running while its pod is gone, reclaiming a port from under connected players —
//! and none of them need I/O to be wrong in. What it costs is that a `Step` is a promise the
//! applier has to keep, so each variant documents what "done" means for it.
//!
//! ## What it deliberately does not decide
//!
//! **The integrity check.** `provisioned_at` set with the directory missing is a filesystem
//! property, and folding it in would mean handing the planner a third view of the world whose
//! *absence* — a `readdir` that failed — would read as every room being faulted at once. It stays
//! in [`crate::reconcile`], where a failed read is a failed read.
//!
//! **Orphans and stale Secrets.** Both need memory across ticks (an orphan is only an orphan on the
//! second consecutive sighting) or a comparison with a column no room transition reads. They are
//! M9's sweep, and they are cluster-scoped rather than room-scoped, so they do not fit [`Action`].
// The applier is M7; the tests below are the whole of this module's use until then. `expect` rather
// than `allow` so this warns the moment `ensure_room_running` starts consuming these.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the applier that executes these Steps lands at M7"
    )
)]

use chrono::{DateTime, Duration, Utc};
use puna_core::ids::RoomId;
use puna_core::model::room::{DesiredState, RoomState};

use crate::cluster::{ClusterSnapshot, RoomDeployment};

/// How long a room may sit in `starting` before the attempt is called a failure.
///
/// Matches the Deployment's `progressDeadlineSeconds`, and it has to be generous: a cold start is
/// an image pull plus restoring a save from CephFS, which is why the pod's own `startupProbe`
/// budgets five minutes too.
const START_DEADLINE: Duration = Duration::seconds(300);

/// How long a room may sit in `stopping` before the graceful path is abandoned.
///
/// Twice `terminationGracePeriodSeconds`. A pod still present after that did not go down when
/// asked, so the Deployment is deleted rather than waited on indefinitely — pahoa holds an
/// exclusive `flock` on the save directory, so a room that will not exit is a room that cannot
/// restart.
const STOP_DEADLINE: Duration = Duration::seconds(90);

/// How long after entering a live state a missing Deployment is believed.
///
/// The snapshot is read with `resourceVersion=0`, which is the watch cache and can lag. Without
/// this a room whose Deployment was created moments ago would be declared vanished and dropped to
/// `idle`, then started again — a loop that costs a room its pod every tick and looks like a
/// scheduling problem.
const VANISH_GRACE: Duration = Duration::seconds(60);

/// Consecutive sweeps with no ready replica before a room is called degraded.
const DEGRADED_SWEEPS: i32 = 3;

/// Everything the planner needs to know about one room.
///
/// A projection, not [`puna_core::model::room::Room`]: the planner reads observed columns the room
/// page has no business rendering, and deliberately **not** the columns it would be wrong to decide
/// on. `base_port` and `deployment_uid` are absent for that reason — they are things the applier
/// needs, and a planner that could see them could branch on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomView {
    pub id: RoomId,
    /// The room's `pg_try_advisory_lock` key, carried through to the action so the applier holds
    /// the right lock without a second query.
    pub lock_key: i32,
    pub state: RoomState,
    pub desired: DesiredState,
    /// What the row says the running spec should hash to. `None` means no Deployment was ever
    /// recorded for this room.
    pub spec_hash: Option<String>,
    pub state_changed_at: DateTime<Utc>,
    pub retry_after: Option<DateTime<Utc>>,
    pub not_ready_sweeps: i32,
}

/// Why a room is being put back to `idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleReason {
    /// The Deployment is gone and Puna did not remove it — a hand-deleted Deployment, a drained
    /// node whose pod never came back, a namespace someone tidied.
    DeploymentGone,
    /// The stop Puna asked for finished.
    StopComplete,
    // A spec change is deliberately not a reason here: it is [`Step::Recreate`], which lands the
    // room in `idle` itself. One step, so the applier cannot delete the Deployment and then fail to
    // move the row -- which would leave a room advertising a pod that is gone.
}

/// One thing to do to one room.
///
/// Each variant is a promise about what the applier leaves behind, because a level-triggered loop
/// re-derives its work from state: a step that half-finishes without moving the row is a step that
/// runs again forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Materialize the room's state directory, then set `provisioned_at` and `state = 'idle'`.
    /// In that order — the reverse is what produces an `integrity_fault`.
    Provision,

    /// Allocate a port pair and create the objects: Secret, Deployment, own the Secret, Service,
    /// verify the ingress address. Leaves the room `starting` with `spec_hash` and
    /// `deployment_uid` persisted, or `failed` with a reason.
    Start,

    /// Delete the Deployment and drop the room to `idle`, **keeping the port reservation**.
    ///
    /// Not a rolling update, and it cannot be one: the port is in the args *and* the Service, and
    /// pahoa holds an exclusive `flock` on the save directory, so a surge pod would crashloop until
    /// the progress deadline expired. Going through `idle` rather than straight to a recreate is
    /// what lets the next tick take the ordinary [`Step::Start`] path — one code path for creating
    /// a room's objects, whatever the reason.
    Recreate,

    /// The Deployment has a ready replica. Set `advertised_*` and `started_at`, clear
    /// `not_ready_sweeps`, and move to `running`.
    MarkRunning,

    /// No ready replica, but not for long enough to call it degraded. Increment
    /// `not_ready_sweeps` and nothing else.
    NotReady,

    /// [`DEGRADED_SWEEPS`] consecutive sweeps with no ready replica.
    ///
    /// **Still live for the allocator**: players may be mid-reconnect, so the reservation is not
    /// reclaimable. Degraded is a report, not a teardown.
    MarkDegraded,

    /// Put the room back to `idle`, clearing `advertised_*` and **never touching the reservation**
    /// — a room comes back on the same port, which is the requirement the whole reservation table
    /// exists for.
    MarkIdle(IdleReason),

    /// Ask the room to quiesce and save, then delete the Deployment. Leaves it `stopping`.
    Stop,

    /// The backoff has expired: back to `idle` so the next tick starts it again.
    Retry,

    /// The §7 deletion sequence: Deployment gone, reservation released, directory moved to the
    /// trash, row deleted. Ordered so a crash leaves a recoverable directory rather than an
    /// orphaned one.
    Delete,

    /// The room sat in `starting` past [`START_DEADLINE`]. Record `last_error`, bump
    /// `failure_count`, set `retry_after`, and move to `failed`.
    FailStart,
}

/// One action, addressed to one room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub room: RoomId,
    pub lock_key: i32,
    pub step: Step,
}

/// Decide what to do, for every room, from the world as it is.
///
/// **At most one action per room per tick.** Not an optimization: it is what makes "apply up to 8
/// rooms concurrently" safe to say, since two actions against one room could only be serialized by
/// the caller remembering to.
pub fn plan(rooms: &[RoomView], cluster: &ClusterSnapshot, now: DateTime<Utc>) -> Vec<Action> {
    rooms
        .iter()
        .filter_map(|room| {
            step_for(room, cluster.deployment(room.id), now).map(|step| Action {
                room: room.id,
                lock_key: room.lock_key,
                step,
            })
        })
        .collect()
}

/// The transition table from §3, as one expression.
fn step_for(
    room: &RoomView,
    deployment: Option<&RoomDeployment>,
    now: DateTime<Utc>,
) -> Option<Step> {
    // Deletion wins from every state, including `integrity_fault` and `failed`. It is the one
    // request that must not be blocked by the room being in a bad way -- a room nobody can fix is
    // precisely a room somebody wants to delete.
    if room.desired == DesiredState::Deleted {
        return Some(Step::Delete);
    }

    match room.state {
        // The row exists and the directory may not, which is one of the two states D3's invariant
        // does not cover. Provisioning is unconditional: it is cheap, idempotent, and a room
        // cannot do anything else until its directory is there.
        RoomState::Provisioning => Some(Step::Provision),

        // Never auto-repaired, in any direction. Recreating the directory would replace saved
        // progress with an empty room and look like a successful start, so the only way out is an
        // operator -- or the deletion handled above.
        RoomState::IntegrityFault => None,

        // Orchestrator-owned and transient: whatever is mid-flight owns it, and the row moves when
        // that finishes. Reached only through a crash, which the next tick's `deleting` sweep or an
        // operator resolves.
        RoomState::Deleting => None,

        RoomState::Idle => match room.desired {
            DesiredState::Running => Some(Step::Start),
            // Resting, holding its port. Nothing to do, and specifically nothing to clean up:
            // idle teardown never touches the directory or the reservation.
            DesiredState::Stopped => None,
            DesiredState::Deleted => unreachable!("handled above"),
        },

        RoomState::Failed => match room.desired {
            DesiredState::Running => match room.retry_after {
                // No backoff recorded is not a licence to retry immediately: something failed and
                // did not say when to try again, so wait for an operator rather than spin.
                None => None,
                Some(after) if after <= now => Some(Step::Retry),
                Some(_) => None,
            },
            DesiredState::Stopped => None,
            DesiredState::Deleted => unreachable!("handled above"),
        },

        RoomState::Stopping => match deployment {
            None => Some(Step::MarkIdle(IdleReason::StopComplete)),
            // Past the grace period twice over: ask again, which this time deletes the Deployment
            // outright. A pod that will not exit holds the save directory's flock, so leaving it
            // alone means the room can never start again.
            Some(_) if now - room.state_changed_at > STOP_DEADLINE => Some(Step::Stop),
            Some(_) => None,
        },

        RoomState::Starting | RoomState::Running | RoomState::Degraded => {
            // A stop request outranks everything below: there is no point converging a spec on a
            // room that is being taken down.
            if room.desired == DesiredState::Stopped {
                return Some(Step::Stop);
            }

            match deployment {
                None => {
                    // Believed only after the grace period, because the snapshot comes from the
                    // watch cache and a fresh create can be missing from a stale read.
                    (now - room.state_changed_at > VANISH_GRACE)
                        .then_some(Step::MarkIdle(IdleReason::DeploymentGone))
                }

                // The running spec is not the one the row describes -- a new image, a changed
                // port, or a `slot_auth` change, which reaches pahoa through the Secret and moves
                // nothing else in the pod. A hash we cannot match at all (`None` on the row) is the
                // crash window in §7 step 3, and is treated the same way: the row is authoritative
                // and adoption would mean trusting a label to prove provenance.
                Some(cluster) if cluster.spec_hash != room.spec_hash => Some(Step::Recreate),

                Some(cluster) if cluster.ready_replicas >= 1 => {
                    // Re-affirm only when something actually changed, so a healthy room costs one
                    // read per tick and no writes.
                    let already_running =
                        room.state == RoomState::Running && room.not_ready_sweeps == 0;
                    (!already_running).then_some(Step::MarkRunning)
                }

                Some(_) if room.state == RoomState::Starting => {
                    // Still coming up, or never will.
                    (now - room.state_changed_at > START_DEADLINE).then_some(Step::FailStart)
                }

                // Was up, is not now. Degraded rather than failed: the Deployment is there, the
                // pod may be restarting, and the room's port stays reserved either way.
                Some(_) => {
                    if room.state == RoomState::Running {
                        if room.not_ready_sweeps + 1 >= DEGRADED_SWEEPS {
                            Some(Step::MarkDegraded)
                        } else {
                            Some(Step::NotReady)
                        }
                    } else {
                        // Already degraded. Reported once; nothing further to say until it comes
                        // back, its Deployment goes away, or somebody stops it.
                        None
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_770_000_000, 0).expect("a valid fixed instant")
    }

    /// A room in `state`, wanting `desired`, that has been there for ten seconds.
    fn view(state: RoomState, desired: DesiredState) -> RoomView {
        RoomView {
            id: RoomId::new(),
            lock_key: 7,
            state,
            desired,
            spec_hash: Some("hash-1".into()),
            state_changed_at: now() - Duration::seconds(10),
            retry_after: None,
            not_ready_sweeps: 0,
        }
    }

    fn deployment(room: &RoomView, hash: &str, ready: i32) -> RoomDeployment {
        RoomDeployment {
            name: crate::cluster::object_name(room.id),
            uid: "uid-1".into(),
            room_id: Some(room.id),
            spec_hash: Some(hash.to_string()),
            replicas: 1,
            ready_replicas: ready,
            created_at: now(),
        }
    }

    fn snapshot(deployments: Vec<RoomDeployment>) -> ClusterSnapshot {
        ClusterSnapshot {
            deployments,
            ..Default::default()
        }
    }

    /// What the planner decides for one room, given a cluster.
    fn decide(room: &RoomView, cluster: &ClusterSnapshot) -> Option<Step> {
        let actions = plan(std::slice::from_ref(room), cluster, now());
        assert!(actions.len() <= 1, "one action per room per tick");
        actions.into_iter().next().map(|a| a.step)
    }

    /// One row: the room's state and wish, the Deployment the cluster reports for it as
    /// `(spec_hash, ready_replicas)`, and the step that must fall out.
    type Case = (
        RoomState,
        DesiredState,
        Option<(&'static str, i32)>,
        Option<Step>,
    );

    /// §3's table, walked. Each row is `(what the world looks like, what to do about it)`.
    #[test]
    fn the_transition_table() {
        use DesiredState::{Running, Stopped};
        use RoomState as S;

        let cases: Vec<Case> = vec![
            // Provisioning is unconditional and happens before anything can be wanted.
            (S::Provisioning, Running, None, Some(Step::Provision)),
            (S::Provisioning, Stopped, None, Some(Step::Provision)),
            // Idle rests, or starts.
            (S::Idle, Running, None, Some(Step::Start)),
            (S::Idle, Stopped, None, None),
            // A leftover Deployment does not change the decision: `Start` is idempotent and
            // reconciles the spec itself.
            (S::Idle, Running, Some(("hash-1", 1)), Some(Step::Start)),
            // Coming up.
            (S::Starting, Running, Some(("hash-1", 0)), None),
            (
                S::Starting,
                Running,
                Some(("hash-1", 1)),
                Some(Step::MarkRunning),
            ),
            (
                S::Starting,
                Running,
                Some(("hash-0", 0)),
                Some(Step::Recreate),
            ),
            // A stop request outranks converging the spec.
            (S::Starting, Stopped, Some(("hash-1", 0)), Some(Step::Stop)),
            // Up and healthy: no writes at all, which is the common case every tick.
            (S::Running, Running, Some(("hash-1", 1)), None),
            (
                S::Running,
                Running,
                Some(("hash-1", 0)),
                Some(Step::NotReady),
            ),
            (
                S::Running,
                Running,
                Some(("hash-2", 1)),
                Some(Step::Recreate),
            ),
            (S::Running, Stopped, Some(("hash-1", 1)), Some(Step::Stop)),
            // Degraded reports once and then waits.
            (S::Degraded, Running, Some(("hash-1", 0)), None),
            (
                S::Degraded,
                Running,
                Some(("hash-1", 1)),
                Some(Step::MarkRunning),
            ),
            (S::Degraded, Stopped, Some(("hash-1", 0)), Some(Step::Stop)),
            // Stopping finishes when the Deployment is gone.
            (
                S::Stopping,
                Stopped,
                None,
                Some(Step::MarkIdle(IdleReason::StopComplete)),
            ),
            (S::Stopping, Stopped, Some(("hash-1", 1)), None),
            // Failed waits for its backoff; with none recorded it waits for a person.
            (S::Failed, Running, None, None),
            (S::Failed, Stopped, None, None),
            // Never touched, in any direction.
            (S::IntegrityFault, Running, None, None),
            (S::IntegrityFault, Stopped, Some(("hash-1", 1)), None),
            (S::Deleting, Stopped, None, None),
        ];

        for (state, desired, dep, expected) in cases {
            let room = view(state, desired);
            let cluster = snapshot(
                dep.map(|(hash, ready)| deployment(&room, hash, ready))
                    .into_iter()
                    .collect(),
            );
            assert_eq!(
                decide(&room, &cluster),
                expected,
                "state={state:?} desired={desired:?} deployment={dep:?}"
            );
        }
    }

    /// Deletion is reachable from every state, because a room nobody can fix is exactly the room
    /// somebody wants gone.
    #[test]
    fn a_delete_request_is_honored_from_every_state() {
        for state in RoomState::ALL {
            let room = view(state, DesiredState::Deleted);
            let cluster = snapshot(vec![deployment(&room, "hash-1", 1)]);
            assert_eq!(
                decide(&room, &cluster),
                Some(Step::Delete),
                "a room in {state:?} must still be deletable"
            );
        }
    }

    /// The one state that is never acted on otherwise. Worth its own sweep of the cross product,
    /// because "never auto-repaired" is a promise about *all* the other inputs.
    #[test]
    fn an_integrity_fault_is_only_ever_deleted() {
        for desired in DesiredState::ALL {
            for dep in [None, Some(("hash-1", 1)), Some(("hash-9", 0))] {
                let mut room = view(RoomState::IntegrityFault, desired);
                room.state_changed_at = now() - Duration::days(30);
                room.retry_after = Some(now() - Duration::days(1));
                let cluster = snapshot(
                    dep.map(|(hash, ready)| deployment(&room, hash, ready))
                        .into_iter()
                        .collect(),
                );

                let expected = (desired == DesiredState::Deleted).then_some(Step::Delete);
                assert_eq!(decide(&room, &cluster), expected, "{desired:?} {dep:?}");
            }
        }
    }

    /// A missing Deployment inside the grace window is a stale read, not a vanished room.
    ///
    /// The snapshot comes from the watch cache (`resourceVersion=0`), so a Deployment created
    /// seconds ago can be absent from it. Believing that immediately would drop the room to `idle`
    /// and start it again every tick — a pod destroyed per sweep, looking for all the world like a
    /// scheduling problem.
    #[test]
    fn a_vanished_deployment_is_believed_only_after_the_grace_period() {
        for state in [RoomState::Starting, RoomState::Running, RoomState::Degraded] {
            let mut room = view(state, DesiredState::Running);

            room.state_changed_at = now() - VANISH_GRACE;
            assert_eq!(decide(&room, &snapshot(vec![])), None, "{state:?} at grace");

            room.state_changed_at = now() - VANISH_GRACE - Duration::seconds(1);
            assert_eq!(
                decide(&room, &snapshot(vec![])),
                Some(Step::MarkIdle(IdleReason::DeploymentGone)),
                "{state:?} past grace"
            );
        }
    }

    /// Five minutes matching `progressDeadlineSeconds`: a cold start is an image pull plus a save
    /// restored from CephFS, so a short deadline would fail rooms that were merely slow.
    #[test]
    fn starting_becomes_failed_only_after_the_progress_deadline() {
        let mut room = view(RoomState::Starting, DesiredState::Running);
        let cluster = snapshot(vec![deployment(&room, "hash-1", 0)]);

        room.state_changed_at = now() - START_DEADLINE;
        assert_eq!(decide(&room, &cluster), None, "still within the deadline");

        room.state_changed_at = now() - START_DEADLINE - Duration::seconds(1);
        assert_eq!(decide(&room, &cluster), Some(Step::FailStart));

        // ...and a ready replica beats the deadline, however long it took to arrive.
        let ready = snapshot(vec![deployment(&room, "hash-1", 1)]);
        assert_eq!(decide(&room, &ready), Some(Step::MarkRunning));
    }

    #[test]
    fn a_backoff_is_waited_out_and_then_retried() {
        let mut room = view(RoomState::Failed, DesiredState::Running);

        room.retry_after = Some(now() + Duration::seconds(1));
        assert_eq!(decide(&room, &snapshot(vec![])), None);

        room.retry_after = Some(now());
        assert_eq!(decide(&room, &snapshot(vec![])), Some(Step::Retry));

        // A room that wants to stay stopped does not retry, however long ago it failed.
        room.desired = DesiredState::Stopped;
        room.retry_after = Some(now() - Duration::days(1));
        assert_eq!(decide(&room, &snapshot(vec![])), None);
    }

    /// Three consecutive sweeps, counted on the row rather than in memory, so a leader handover
    /// does not reset it.
    #[test]
    fn degraded_takes_three_consecutive_sweeps() {
        let mut room = view(RoomState::Running, DesiredState::Running);
        let not_ready = snapshot(vec![deployment(&room, "hash-1", 0)]);

        for sweeps in 0..DEGRADED_SWEEPS - 1 {
            room.not_ready_sweeps = sweeps;
            assert_eq!(decide(&room, &not_ready), Some(Step::NotReady), "{sweeps}");
        }

        room.not_ready_sweeps = DEGRADED_SWEEPS - 1;
        assert_eq!(decide(&room, &not_ready), Some(Step::MarkDegraded));

        // One ready replica clears the count, which is why `MarkRunning` fires for a room already
        // in `running`: the counter is state and has to be reset by something.
        let ready = snapshot(vec![deployment(&room, "hash-1", 1)]);
        assert_eq!(decide(&room, &ready), Some(Step::MarkRunning));
        room.not_ready_sweeps = 0;
        assert_eq!(
            decide(&room, &ready),
            None,
            "a healthy room costs one read and no writes"
        );
    }

    /// A stop that does not complete cannot be waited on forever: pahoa's `flock` means a pod that
    /// will not exit is a room that can never start.
    #[test]
    fn a_stop_that_never_finishes_is_escalated() {
        let mut room = view(RoomState::Stopping, DesiredState::Stopped);
        let cluster = snapshot(vec![deployment(&room, "hash-1", 1)]);

        room.state_changed_at = now() - STOP_DEADLINE;
        assert_eq!(decide(&room, &cluster), None);

        room.state_changed_at = now() - STOP_DEADLINE - Duration::seconds(1);
        assert_eq!(decide(&room, &cluster), Some(Step::Stop));
    }

    /// A `slot_auth` change moves nothing in the pod spec — it rides in the Secret through
    /// `envFrom` — so the mode is folded into the hash precisely to make this fire.
    #[test]
    fn a_changed_spec_hash_recreates_from_every_live_state() {
        for state in [RoomState::Starting, RoomState::Running, RoomState::Degraded] {
            let room = view(state, DesiredState::Running);
            let stale = snapshot(vec![deployment(&room, "an-older-hash", 1)]);
            assert_eq!(decide(&room, &stale), Some(Step::Recreate), "{state:?}");
        }

        // The §7 step-3 crash window: the Deployment exists and the row never recorded its hash.
        let mut room = view(RoomState::Starting, DesiredState::Running);
        room.spec_hash = None;
        let cluster = snapshot(vec![deployment(&room, "hash-1", 1)]);
        assert_eq!(
            decide(&room, &cluster),
            Some(Step::Recreate),
            "an unmatchable Deployment is replaced rather than adopted"
        );
    }

    /// Another room's Deployment is not this room's, and a Deployment with no room label belongs
    /// to nobody. Both would otherwise read as "the room is up".
    #[test]
    fn a_room_is_only_matched_to_its_own_deployment() {
        let room = view(RoomState::Starting, DesiredState::Running);
        let other = view(RoomState::Running, DesiredState::Running);

        let mut unlabeled = deployment(&room, "hash-1", 1);
        unlabeled.room_id = None;

        let cluster = snapshot(vec![deployment(&other, "hash-1", 1), unlabeled]);
        let mut waiting = room.clone();
        waiting.state_changed_at = now() - VANISH_GRACE - Duration::seconds(1);
        assert_eq!(
            decide(&waiting, &cluster),
            Some(Step::MarkIdle(IdleReason::DeploymentGone))
        );
    }

    /// The planner is a function of its arguments, and the lock key rides along so the applier
    /// takes the right per-room lock without going back to the database for it.
    #[test]
    fn every_action_carries_the_room_it_is_for() {
        let rooms = vec![
            view(RoomState::Provisioning, DesiredState::Running),
            view(RoomState::Idle, DesiredState::Running),
            // Healthy: contributes no action, so the output is not positional.
            view(RoomState::Running, DesiredState::Running),
            view(RoomState::Idle, DesiredState::Deleted),
        ];
        let cluster = snapshot(vec![deployment(&rooms[2], "hash-1", 1)]);

        let actions = plan(&rooms, &cluster, now());
        assert_eq!(actions.len(), 3);
        assert_eq!(
            actions.iter().map(|a| a.step.clone()).collect::<Vec<_>>(),
            [Step::Provision, Step::Start, Step::Delete]
        );
        for action in &actions {
            let room = rooms
                .iter()
                .find(|r| r.id == action.room)
                .expect("its room");
            assert_eq!(action.lock_key, room.lock_key);
        }

        // Same inputs, same answer: nothing here reads a clock or a cache.
        assert_eq!(plan(&rooms, &cluster, now()), actions);
    }
}
