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
    /// What the room's spec would hash to **if it were rendered right now**, where that is known.
    ///
    /// Computed by the caller only for rooms whose decision it can change — today that is `failed`
    /// rooms still inside their backoff, and nothing else. Rendering it costs a room's secrets, its
    /// slot list and its reservation, so paying it for every room on every tick would buy a
    /// comparison exactly one state acts on.
    ///
    /// **`None` means "not computed", never "unchanged".** That is why it can only ever *cause* a
    /// retry in the company of a recorded hash to disagree with, and never on its own.
    pub desired_spec_hash: Option<String>,
    pub state_changed_at: DateTime<Utc>,
    pub retry_after: Option<DateTime<Utc>>,
    pub not_ready_sweeps: i32,
    /// When somebody asked for this room to be restarted onto its current spec.
    ///
    /// **The only thing that makes a running room bounce for a spec change.** Drift on its own
    /// never does and must never start to: an image bump lands on the whole environment at once,
    /// and a room mid-session is not something a `git push` gets to interrupt. Set by an operator
    /// through the console, or by a `slot_auth` change, which has to reach pahoa now rather than
    /// whenever the room happens to restart.
    ///
    /// A request, so it is **consumed** rather than observed: every step that leaves the room
    /// running its freshly-rendered spec clears it.
    pub redeploy_requested_at: Option<DateTime<Utc>>,
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

impl Step {
    /// Whether taking this step leaves the room somewhere it still has to move on from.
    ///
    /// This is what tells the loop to look again in seconds rather than at the next full pass, and
    /// it is asked of the steps just *applied* — because the room views were read before they ran.
    /// A pass that recreates a room read it as `running`; by the time the pass ends it is `idle`
    /// with a Deployment draining, and nothing in the views says so.
    ///
    /// Answering `false` where it should be `true` costs the latency this whole mechanism exists to
    /// remove. Answering `true` where it should be `false` costs cheap passes that plan nothing —
    /// so the doubtful cases go to `true`.
    pub fn leaves_work_pending(&self) -> bool {
        match self {
            // Each of these hands the room to a later pass: provisioned rooms start, started rooms
            // wait on a pod, recreated rooms wait on a drain, stopping rooms wait on an exit, and a
            // delete walks a sequence.
            Step::Provision
            | Step::Start
            | Step::Recreate
            | Step::Retry
            | Step::Stop
            | Step::Delete => true,
            // `MarkIdle` settles the room only if nobody wants it running — and if somebody does,
            // the next pass plans a Start. Cheaper to look again than to encode that here.
            Step::MarkIdle(_) => true,
            // Terminal for this transition. `NotReady` and `MarkDegraded` describe a room waiting on
            // something outside Puna — an image pull, a scheduler — on a timescale where a
            // three-second pass is noise; `FailStart` is a backoff, which is a wall clock.
            Step::MarkRunning | Step::MarkDegraded | Step::NotReady | Step::FailStart => false,
        }
    }
}

/// Rooms the loop is waiting on, as the views describe them.
///
/// Deliberately a property of the world rather than of the last pass, so a restarted orchestrator
/// picks the short cadence back up without having to remember anything. The one thing it cannot see
/// is a transition this pass just caused — [`Step::leaves_work_pending`] covers that.
pub fn converging(rooms: &[RoomView], cluster: &ClusterSnapshot) -> usize {
    rooms
        .iter()
        .filter(|room| {
            // Somebody asked for this room to go, and a delete crosses several passes.
            if room.desired == DesiredState::Deleted {
                return true;
            }
            match room.state {
                // Mid-flight by definition.
                RoomState::Provisioning | RoomState::Starting | RoomState::Stopping => true,
                // **The case `puna_rooms{state}` cannot show.** A room that is restarting reads as
                // `idle` for the whole time its previous pod is draining, which is indistinguishable
                // from resting — and is exactly the window worth looking at often. Also covers a
                // plain start request, where there is no Deployment and the work is immediate.
                RoomState::Idle => room.desired == DesiredState::Running,
                // Settled, or waiting on a clock. A `running` room wanting a redeploy is settled:
                // the recreate is paced, so looking sooner would not start it sooner.
                _ => false,
            }
        })
        .count()
        .max(
            // A Deployment still draining for a room that is otherwise settled would be missed
            // above; count it here rather than trusting the row alone.
            cluster
                .deployments
                .iter()
                .filter(|d| d.deleting && d.room_id.is_some())
                .count(),
        )
}

/// One action, addressed to one room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub room: RoomId,
    pub lock_key: i32,
    pub step: Step,
}

/// Which kind of pass this is.
///
/// The loop runs a short **convergence** pass while a room is mid-transition, so a restart does not
/// spend most of its downtime waiting for the next full pass. Everything a convergence pass does is
/// a subset of a full one — same planner, same applier, same idempotence — but two steps are barred
/// from it, and both bars are statements about the state machine rather than about scheduling,
/// which is why they live here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickKind {
    /// The whole pass, on `PUNA_RECONCILE_INTERVAL`.
    Reconcile,
    /// A short look at rooms in flight, on `PUNA_CONVERGE_INTERVAL`.
    Converge,
}

impl TickKind {
    /// The metric label. Must match `puna_core::metrics::TICK_KINDS`, which is what `init` seeds to
    /// zero — a label written here and absent there renders as missing data rather than as a zero,
    /// and "the loop has stopped converging" would look exactly like "nothing has scraped yet".
    pub fn as_str(self) -> &'static str {
        match self {
            TickKind::Reconcile => "reconcile",
            TickKind::Converge => "converge",
        }
    }
}

/// Decide what to do, for every room, from the world as it is.
///
/// **At most one action per room per tick.** Not an optimization: it is what makes "apply up to 8
/// rooms concurrently" safe to say, since two actions against one room could only be serialized by
/// the caller remembering to.
pub fn plan(
    rooms: &[RoomView],
    cluster: &ClusterSnapshot,
    now: DateTime<Utc>,
    max_recreates: usize,
    kind: TickKind,
) -> Vec<Action> {
    let planned: Vec<(&RoomView, Step)> = rooms
        .iter()
        .filter_map(|room| {
            step_for(room, cluster.deployment(room.id), now).map(|step| (room, step))
        })
        // **Two steps a convergence pass must not take.** Both would otherwise change behavior
        // simply because the loop looked more often, which is the trap in a variable cadence.
        //
        //   * `Recreate` is PACED, and its pace is expressed as a per-pass cap. Allowing it here
        //     would turn "one room per reconcile interval" into "one room every few seconds" and
        //     tear through a fleet-wide restart queue -- the exact stampede the cap exists to
        //     prevent, reintroduced by the mechanism meant to make one restart quicker.
        //   * `NotReady` COUNTS PASSES. `not_ready_sweeps` reaching DEGRADED_SWEEPS is what calls a
        //     room degraded, so counting convergence passes would declare a room degraded roughly
        //     ten times sooner -- a threshold silently redefined by a scheduling change.
        //
        // Nothing is lost by deferring either: the next full pass sees the same world. The rule for
        // anything added later is the one these two fail -- **a step whose meaning depends on how
        // often it is taken belongs to the full pass.**
        .filter(|(_, step)| {
            kind == TickKind::Reconcile || !matches!(step, Step::Recreate | Step::NotReady)
        })
        .collect();

    // **The cap exists because nothing else bounds this.** Applying is a sequential loop with no
    // throttle, and a foreground delete returns as soon as the API server accepts it rather than
    // when the pod is gone -- so an uncapped pass would stop every room it planned for inside one
    // tick and bring them all back together: one simultaneous final save and restore per room, on
    // one shared CephFS volume. Deferring costs nothing, because the loop is level-triggered and
    // a room not recreated this tick is recreated on the next.
    //
    // Chosen oldest-request-first so a rollout drains in the order people asked for it and a room
    // cannot be starved by later requests arriving. Everything else keeps the caller's ordering --
    // the tick reads rooms by `created_at`, and reordering the whole pass to cap one step would
    // make the sweep's behavior depend on uuids.
    let mut by_age: Vec<usize> = planned
        .iter()
        .enumerate()
        .filter(|(_, (_, step))| *step == Step::Recreate)
        .map(|(index, _)| index)
        .collect();
    by_age.sort_by_key(|&index| {
        let room = planned[index].0;
        (room.redeploy_requested_at, room.id)
    });
    let deferred: Vec<usize> = by_age.into_iter().skip(max_recreates).collect();

    planned
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !deferred.contains(index))
        .map(|(_, (room, step))| Action {
            room: room.id,
            lock_key: room.lock_key,
            step,
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
            // **Not while the last Deployment is still going away.** A recreate drops the room to
            // `idle` the moment the API server accepts the delete, and under foreground propagation
            // the object outlives that call for as long as the pod takes to drain -- up to the full
            // grace period, since pahoa flushes a final save on SIGTERM.
            //
            // Starting inside that window is worse than waiting: the name is still taken, so a
            // create conflicts, and the applier's `get` finds a readable Deployment whose spec hash
            // matches the one about to be rendered -- so it ADOPTS a dying object and waits for a
            // ready replica that can never arrive, until START_DEADLINE five minutes later.
            //
            // Nothing else needs to happen here: the object's disappearance is what makes the room
            // startable, and the loop is level-triggered, so the next pass takes the ordinary path.
            DesiredState::Running if deployment.is_some_and(|d| d.deleting) => None,
            DesiredState::Running => Some(Step::Start),
            // Resting, holding its port. Nothing to do, and specifically nothing to clean up:
            // idle teardown never touches the directory or the reservation.
            // At rest, holding its port. Nothing to do, and specifically nothing to clean up:
            // idle teardown never touches the directory or the reservation. `closed` rests here
            // too -- it differs from `stopped` only in who may ask for it to run again, which is
            // the web tier's question and not this one's.
            DesiredState::Stopped | DesiredState::Closed => None,
            DesiredState::Deleted => unreachable!("handled above"),
        },

        RoomState::Failed => match room.desired {
            DesiredState::Running => {
                // **A changed spec interrupts the backoff, and outranks the timer.** The backoff
                // exists to stop a room broken by its own configuration being retried forever — but
                // a spec that now renders differently is evidence that the recorded failure no
                // longer describes this room, because an operator has already changed something:
                // the image, or the row. Waiting out a timer that measures a problem somebody has
                // just fixed is the case this arm got wrong, and at `failure_count = 7` it is the
                // full ten-minute cap.
                //
                // Both hashes have to be present, and for different reasons. A `None` desired hash
                // is "not computed"; a `None` recorded hash is a room that never got far enough to
                // have one. Neither is a disagreement, so neither retries.
                if let (Some(want), Some(have)) = (&room.desired_spec_hash, &room.spec_hash)
                    && want != have
                {
                    return Some(Step::Retry);
                }

                match room.retry_after {
                    // No backoff recorded is not a licence to retry immediately: something failed
                    // and did not say when to try again, so wait for an operator rather than spin.
                    // A spec change above is exactly that operator, which is why it is checked
                    // first rather than inside this match.
                    None => None,
                    Some(after) if after <= now => Some(Step::Retry),
                    Some(_) => None,
                }
            }
            // Nobody wants it up, so the backoff has nothing to count down to. A closed room that
            // last failed stays failed and stays closed; reopening it is what asks again.
            DesiredState::Stopped | DesiredState::Closed => None,
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
            //
            // **`is_at_rest`, not `== Stopped`.** `closed` is the same instruction to the
            // reconciler and this is the arm that carries it out — an equality check here is the
            // one the compiler cannot catch, and getting it wrong leaves a closed room running
            // forever while its page says it is closed.
            if room.desired.is_at_rest() {
                return Some(Step::Stop);
            }

            match deployment {
                None => {
                    // Believed only after the grace period, because the snapshot comes from the
                    // watch cache and a fresh create can be missing from a stale read.
                    (now - room.state_changed_at > VANISH_GRACE)
                        .then_some(Step::MarkIdle(IdleReason::DeploymentGone))
                }

                // **Somebody asked.** First among the arms that see a Deployment, because a
                // redeploy is the one instruction here that a human issued about this room
                // specifically: it outranks re-affirming `running` and outranks waiting out a
                // start. It does NOT outrank `Stop` or `Delete`, both handled above -- a room
                // being torn down has no use for a restart.
                Some(_) if room.redeploy_requested_at.is_some() => Some(Step::Recreate),

                // The running spec is not the one the row describes -- a new image, a changed
                // port, or a `slot_auth` change, which reaches pahoa through the Secret and moves
                // nothing else in the pod. A hash we cannot match at all (`None` on the row) is the
                // crash window in §7 step 3, and is treated the same way: the row is authoritative
                // and adoption would mean trusting a label to prove provenance.
                //
                // Note what is NOT here: a comparison against `desired_spec_hash`. Drift from the
                // rendered spec is reported, never acted on -- see `redeploy_requested_at`.
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
            // Not computed, which is the state every room outside `failed` is in.
            desired_spec_hash: None,
            state_changed_at: now() - Duration::seconds(10),
            retry_after: None,
            not_ready_sweeps: 0,
            // Nobody has asked for a restart, which is the state every room is in until a person
            // acts. Drift alone must never populate this.
            redeploy_requested_at: None,
        }
    }

    fn deployment(room: &RoomView, hash: &str, ready: i32) -> RoomDeployment {
        RoomDeployment {
            name: crate::cluster::object_name(room.id),
            uid: "uid-1".into(),
            room_id: Some(room.id),
            spec_hash: Some(hash.to_string()),
            image: Some("pahoa:test".into()),
            replicas: 1,
            ready_replicas: ready,
            created_at: now(),
            deleting: false,
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
        let actions = plan(
            std::slice::from_ref(room),
            cluster,
            now(),
            1,
            TickKind::Reconcile,
        );
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

    /// **An idle room does not start on top of a Deployment that is still draining.**
    ///
    /// The row above — idle, wanting to run, with a leftover Deployment — plans `Start` on purpose,
    /// because `Start` is idempotent and reconciles the spec itself. That reasoning holds for an
    /// object that is *staying*; it is exactly wrong for one that has been accepted for deletion.
    /// The name is still taken, so nothing can be created under it, and the applier's read finds a
    /// live-looking object whose spec hash matches the one about to be rendered.
    ///
    /// Waiting costs a pass. Not waiting cost five minutes: the room would adopt the dying object
    /// and sit in `starting` until `START_DEADLINE`.
    #[test]
    fn a_draining_deployment_is_waited_out_rather_than_started_over() {
        let room = view(RoomState::Idle, DesiredState::Running);

        let mut draining = deployment(&room, "hash-1", 1);
        draining.deleting = true;
        assert_eq!(
            decide(&room, &snapshot(vec![draining])),
            None,
            "a room whose last Deployment is going away has nothing to do but wait"
        );

        // And the moment it is gone, the ordinary path resumes. This half is what stops the guard
        // from being a room that never starts again.
        assert_eq!(
            decide(&room, &snapshot(Vec::new())),
            Some(Step::Start),
            "the object's disappearance is the signal"
        );
    }

    /// **A convergence pass may not recreate, and may not count a sweep.**
    ///
    /// Both would change behavior purely because the loop looked more often, which is the trap in
    /// running two cadences. A recreate is paced *per pass*, so allowing it at the short cadence
    /// turns "one room per reconcile interval" into one every few seconds and tears through a
    /// fleet-wide restart queue. `NotReady` increments `not_ready_sweeps`, and `DEGRADED_SWEEPS` is
    /// a count of passes — so a room would be called degraded about ten times sooner.
    #[test]
    fn a_convergence_pass_takes_neither_of_the_two_steps_that_count_passes() {
        let mut drifting = view(RoomState::Running, DesiredState::Running);
        drifting.redeploy_requested_at = Some(now());
        let unready = view(RoomState::Running, DesiredState::Running);

        let rooms = vec![drifting.clone(), unready.clone()];
        let cluster = snapshot(vec![
            deployment(&drifting, "hash-1", 1),
            deployment(&unready, "hash-1", 0),
        ]);

        // The full pass does both.
        let full: Vec<Step> = plan(&rooms, &cluster, now(), 1, TickKind::Reconcile)
            .into_iter()
            .map(|a| a.step)
            .collect();
        assert!(full.contains(&Step::Recreate), "{full:?}");
        assert!(full.contains(&Step::NotReady), "{full:?}");

        // The convergence pass does neither, and does not substitute something else for them.
        assert!(
            plan(&rooms, &cluster, now(), 1, TickKind::Converge).is_empty(),
            "a convergence pass must leave a settled fleet alone entirely"
        );
    }

    /// The steps a convergence pass *is* for still happen at the short cadence, or the mechanism
    /// buys nothing: a room coming up must reach `running` without waiting for the full pass.
    #[test]
    fn a_convergence_pass_still_converges() {
        for (state, dep, expected) in [
            (RoomState::Provisioning, None, Step::Provision),
            (RoomState::Idle, None, Step::Start),
            (RoomState::Starting, Some(("hash-1", 1)), Step::MarkRunning),
            (
                RoomState::Stopping,
                None,
                Step::MarkIdle(IdleReason::StopComplete),
            ),
        ] {
            let room = view(state, DesiredState::Running);
            let cluster = snapshot(
                dep.map(|(hash, ready)| deployment(&room, hash, ready))
                    .into_iter()
                    .collect(),
            );
            let actions = plan(
                std::slice::from_ref(&room),
                &cluster,
                now(),
                1,
                TickKind::Converge,
            );
            assert_eq!(
                actions.into_iter().map(|a| a.step).collect::<Vec<_>>(),
                vec![expected.clone()],
                "state={state:?} must still be converged on the short cadence"
            );
        }
    }

    /// What keeps the loop on the short cadence, and the case that made it necessary.
    #[test]
    fn a_restarting_room_reads_as_converging_though_its_state_says_idle() {
        let restarting = view(RoomState::Idle, DesiredState::Running);
        let mut draining = deployment(&restarting, "hash-1", 0);
        draining.deleting = true;

        assert_eq!(
            converging(std::slice::from_ref(&restarting), &snapshot(vec![draining])),
            1,
            "a room mid-restart looks like a resting room in every column"
        );

        // A settled fleet is not converging, or the short cadence would never stop.
        let running = view(RoomState::Running, DesiredState::Running);
        assert_eq!(
            converging(
                std::slice::from_ref(&running),
                &snapshot(vec![deployment(&running, "hash-1", 1)])
            ),
            0
        );

        // Including one that has drifted: the recreate is paced, so looking sooner would not start
        // it sooner, and converging on it would hold the whole loop at the short cadence for as
        // long as a rollout takes.
        let mut drifted = view(RoomState::Running, DesiredState::Running);
        drifted.redeploy_requested_at = Some(now());
        assert_eq!(
            converging(
                std::slice::from_ref(&drifted),
                &snapshot(vec![deployment(&drifted, "hash-1", 1)])
            ),
            0,
            "a queued redeploy is not a reason to spin"
        );
    }

    /// The label vocabulary is one vocabulary, not two that happen to agree today.
    ///
    /// `metrics::init` seeds `TICK_KINDS` to zero so a cold orchestrator renders both series. A
    /// kind written here and missing from that list would render as *no data* instead of as a zero
    /// — and "the loop has stopped converging" would be indistinguishable from "nothing has been
    /// scraped yet", which is precisely the ambiguity the seeding exists to remove.
    #[test]
    fn every_tick_kind_is_a_label_the_registry_seeds() {
        for kind in [TickKind::Reconcile, TickKind::Converge] {
            assert!(
                puna_core::metrics::TICK_KINDS.contains(&kind.as_str()),
                "{kind:?} publishes {:?}, which metrics::init does not seed",
                kind.as_str()
            );
        }
        assert_eq!(
            puna_core::metrics::TICK_KINDS.len(),
            2,
            "a seeded label with no kind to produce it is a series that stays zero forever"
        );
    }

    /// **To the reconciler, `closed` IS `stopped`** — and the arm that carries that out is an
    /// equality check the compiler cannot make exhaustive.
    ///
    /// Adding a variant to `DesiredState` produced two errors in this file and left the third
    /// site — `if room.desired == DesiredState::Stopped` in the live-states arm — compiling
    /// perfectly and silently wrong. That is the one that matters: a running room asked to close
    /// would have kept running indefinitely while its page said closed, with nothing logged and
    /// nothing to look at.
    #[test]
    fn closing_a_live_room_stops_it_and_a_closed_room_stays_down() {
        for state in [RoomState::Starting, RoomState::Running, RoomState::Degraded] {
            let room = view(state, DesiredState::Closed);
            let cluster = snapshot(vec![deployment(&room, "hash-1", 1)]);
            assert_eq!(
                decide(&room, &cluster),
                Some(Step::Stop),
                "a {state:?} room asked to close must come down"
            );
        }

        // And once down it rests exactly as a stopped room does — holding its reservation, holding
        // its directory. The gate on starting it again is the web tier's, not the planner's.
        let closed = view(RoomState::Idle, DesiredState::Closed);
        assert_eq!(
            decide(&closed, &snapshot(Vec::new())),
            None,
            "a closed room must never be started by the reconciler"
        );

        // A failed room that was closed stops retrying: there is nothing to retry toward.
        let mut failed = view(RoomState::Failed, DesiredState::Closed);
        failed.retry_after = Some(now() - Duration::seconds(1));
        assert_eq!(decide(&failed, &snapshot(Vec::new())), None);
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

    /// **The M10c case.** A room pinned at the ten-minute cap keeps waiting after an operator has
    /// already fixed what broke it — so a spec that renders differently now is what cuts the wait
    /// short, because that difference *is* the operator having acted.
    #[test]
    fn a_changed_spec_interrupts_the_backoff() {
        let mut room = view(RoomState::Failed, DesiredState::Running);
        room.retry_after = Some(now() + Duration::minutes(10));
        room.spec_hash = Some("what-we-tried".into());

        // The timer alone: still waiting.
        assert_eq!(decide(&room, &snapshot(vec![])), None);

        // The spec would render differently now. Retry, without waiting out a timer that measures
        // a problem somebody has fixed.
        room.desired_spec_hash = Some("what-we-would-try-now".into());
        assert_eq!(decide(&room, &snapshot(vec![])), Some(Step::Retry));

        // Agreement is not a change, and this is the assertion that stops the interrupt from
        // becoming an unconditional retry -- which would defeat the backoff entirely.
        room.desired_spec_hash = Some("what-we-tried".into());
        assert_eq!(decide(&room, &snapshot(vec![])), None);
    }

    /// A missing hash on either side is not a disagreement, and the two absences mean different
    /// things: the desired one is "not computed", the recorded one is "never got that far".
    #[test]
    fn an_absent_hash_never_interrupts_a_backoff() {
        let mut room = view(RoomState::Failed, DesiredState::Running);
        room.retry_after = Some(now() + Duration::minutes(10));

        // Not computed -- which is every room the caller did not render, so this must never be read
        // as "the spec changed" or every failed room would retry at once.
        room.spec_hash = Some("what-we-tried".into());
        room.desired_spec_hash = None;
        assert_eq!(decide(&room, &snapshot(vec![])), None);

        // Never had a Deployment recorded: there is nothing to have changed *from*.
        room.spec_hash = None;
        room.desired_spec_hash = Some("what-we-would-try-now".into());
        assert_eq!(decide(&room, &snapshot(vec![])), None);
    }

    /// The interrupt is scoped to `failed`. A room that wants to stay stopped stays stopped, however
    /// much its spec has moved on -- otherwise a changed image would start rooms nobody asked for.
    #[test]
    fn a_changed_spec_does_not_start_a_room_that_wants_to_be_stopped() {
        let mut room = view(RoomState::Failed, DesiredState::Stopped);
        room.retry_after = Some(now() - Duration::days(1));
        room.spec_hash = Some("what-we-tried".into());
        room.desired_spec_hash = Some("what-we-would-try-now".into());

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

    /// **The invariant M17 exists to protect: drift is reported, never acted on.**
    ///
    /// An image bump moves `PUNA_PAHOA_IMAGE` for the whole environment at once. If a rendered-spec
    /// disagreement were enough to plan a recreate, that one `git push` would restart every room in
    /// the environment — including rooms with people in them, at whatever hour it merged. The only
    /// thing that may bounce a running room is somebody asking.
    #[test]
    fn drift_alone_plans_nothing() {
        let mut room = view(RoomState::Running, DesiredState::Running);
        room.desired_spec_hash = Some("what-it-would-render-to-now".into());
        let cluster = snapshot(vec![deployment(&room, "hash-1", 1)]);

        assert_eq!(
            decide(&room, &cluster),
            None,
            "a room whose spec would render differently is left alone until asked"
        );
    }

    /// The same room, once somebody asks. This is the whole of the mechanism: one nullable column,
    /// and it outranks re-affirming a healthy room.
    #[test]
    fn a_redeploy_request_recreates_from_every_live_state() {
        for state in [RoomState::Starting, RoomState::Running, RoomState::Degraded] {
            let mut room = view(state, DesiredState::Running);
            room.redeploy_requested_at = Some(now());
            let cluster = snapshot(vec![deployment(&room, "hash-1", 1)]);
            assert_eq!(decide(&room, &cluster), Some(Step::Recreate), "{state:?}");
        }
    }

    /// A room being torn down has no use for a restart, and honoring one would recreate the pod a
    /// stop had just removed.
    #[test]
    fn stopping_and_deleting_outrank_a_redeploy_request() {
        for desired in [DesiredState::Stopped, DesiredState::Deleted] {
            let mut room = view(RoomState::Running, desired);
            room.redeploy_requested_at = Some(now());
            let cluster = snapshot(vec![deployment(&room, "hash-1", 1)]);
            assert!(
                matches!(
                    decide(&room, &cluster),
                    Some(Step::Stop) | Some(Step::Delete)
                ),
                "{desired:?} wins over a pending redeploy"
            );
        }
    }

    /// **A fleet-wide redeploy is a rolling restart, not a simultaneous one.**
    ///
    /// Nothing else bounds this: applying is a sequential loop with no throttle, and a foreground
    /// delete returns when the API server accepts it rather than when the pod is gone. Uncapped,
    /// every room asked for would stop inside one tick and come back together.
    #[test]
    fn the_recreate_cap_defers_the_rest_and_takes_the_oldest_request_first() {
        let mut rooms: Vec<RoomView> = (0..4)
            .map(|_| view(RoomState::Running, DesiredState::Running))
            .collect();
        // Requested in a deliberately different order from the list order.
        for (index, minutes) in [40_i64, 10, 30, 20].into_iter().enumerate() {
            rooms[index].redeploy_requested_at = Some(now() - Duration::minutes(minutes));
        }
        let cluster = snapshot(
            rooms
                .iter()
                .map(|room| deployment(room, "hash-1", 1))
                .collect(),
        );

        let actions = plan(&rooms, &cluster, now(), 2, TickKind::Reconcile);
        assert_eq!(actions.len(), 2, "the cap holds");
        let acted: Vec<RoomId> = actions.iter().map(|a| a.room).collect();
        assert!(
            acted.contains(&rooms[0].id) && acted.contains(&rooms[2].id),
            "the two oldest requests go first, so nobody is starved by later arrivals"
        );

        // The deferred rooms are not lost -- the loop is level-triggered, so the next tick sees
        // them again and they are now the oldest outstanding requests.
        let remaining: Vec<RoomView> = vec![rooms[1].clone(), rooms[3].clone()];
        let actions = plan(&remaining, &cluster, now(), 2, TickKind::Reconcile);
        assert_eq!(actions.len(), 2, "deferred, never dropped");
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

        let actions = plan(&rooms, &cluster, now(), 1, TickKind::Reconcile);
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
        assert_eq!(
            plan(&rooms, &cluster, now(), 1, TickKind::Reconcile),
            actions
        );
    }
}
