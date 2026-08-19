//! Executing one [`Step`], against the database and the cluster.
//!
//! The planner decided; [`crate::apply`] knows the cluster ordering; this is where the two meet the
//! room's row. Every function here writes **observed** columns — `state`, `advertised_*`,
//! `deployment_uid`, `spec_hash` — which is why they all take the [`Orchestrator`] token.
//!
//! ## The advisory lock covers the row, not the cluster call
//!
//! Each step takes `pg_try_advisory_xact_lock` on the room's `lock_key` for its database work and
//! releases it at commit, so a cluster call is made with no lock held. That is deliberate and it is
//! what §2 already argues: **the lock is a simplicity property, not a correctness one.** Every
//! mutation survives a brief double-run on its own — `create` treats `AlreadyExists` as success, the
//! allocator hands a room back its own pair, a Secret apply is idempotent, and pahoa's `flock` is the
//! last backstop against two pods serving one room. Holding a transaction open across a
//! multi-second cluster call to buy a property the design does not need would cost a pooled
//! connection and a vacuum horizon for nothing.

use chrono::Utc;
use diesel::sql_types::{Bool, Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use puna_core::Environment;
use puna_core::ids::RoomId;
use puna_core::model::{Orchestrator, port, room, slot};

use crate::apply::{self, DeploymentRecorder, StartRequest, Started};
use crate::cluster::{ClusterApi, object_name};
use crate::leader;
use crate::plan::{Action, IdleReason, Step};
use crate::spec::{self, Site};
use crate::storage::{self, Layout};

/// Everything a step needs beyond the action itself.
pub struct Context<'a> {
    pub pool: &'a puna_core::db::Pool,
    pub cluster: &'a dyn ClusterApi,
    pub layout: &'a Layout,
    pub site: &'a Site,
    pub environment: Environment,
    /// The DNS name rooms are advertised on — also the name on the room certificate, which is what
    /// makes it load-bearing rather than cosmetic.
    pub advertise_host: &'a str,
    pub orchestrator: Orchestrator,
    pub pahoa_image: &'a str,
}

/// What one step did, for the tick's report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Another actor holds the room's lock. A skip rather than a failure: the tick is
    /// level-triggered, so the next pass picks it up.
    SkippedLocked,
}

/// How long a mis-allocated port pair is left alone.
///
/// The collision is necessarily with something Puna did not create — Puna's own uniqueness is
/// database-enforced — so an hour is a guess at how long somebody else's object lives, and the
/// reservation is not scarce.
const QUARANTINE: chrono::TimeDelta = chrono::TimeDelta::hours(1);

pub async fn execute(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    match &action.step {
        Step::Provision => provision(ctx, action).await,
        Step::Start => start(ctx, action).await,
        Step::Recreate => recreate(ctx, action).await,
        Step::MarkRunning => mark_running(ctx, action).await,
        Step::NotReady => bump_not_ready(ctx, action, false).await,
        Step::MarkDegraded => bump_not_ready(ctx, action, true).await,
        Step::MarkIdle(reason) => mark_idle(ctx, action, *reason).await,
        Step::Stop => stop(ctx, action).await,
        Step::Retry => retry(ctx, action).await,
        Step::Delete => delete(ctx, action).await,
        Step::FailStart => {
            fail(
                ctx,
                action,
                "the pod did not become ready before the progress deadline",
            )
            .await
        }
    }
}

// -- provisioning ------------------------------------------------------------------------------

/// Materialize the room's state directory, then claim it in the row. In that order, always.
async fn provision(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    use diesel_async::{AsyncConnection, scoped_futures::ScopedFutureExt};

    let mut conn = ctx.pool.get().await?;
    let id = action.room;
    let lock_key = action.lock_key;
    let layout = ctx.layout.clone();

    let sha = match generation_sha(&mut conn, id).await? {
        Some(sha) => sha,
        // The room is gone, or its generation is. Either way the next tick's plan will not include
        // it, and inventing a directory for it would be worse than doing nothing.
        None => return Ok(Outcome::Done),
    };

    let done = conn
        .transaction::<bool, anyhow::Error, _>(|conn| {
            async move {
                if !leader::try_lock_room(conn, lock_key).await? {
                    return Ok(false);
                }

                let nonce = uuid::Uuid::new_v4().simple().to_string();
                let outcome = storage::provision(&layout, id, &sha, &nonce)?;

                // Only after the directory is on disk and fsynced. The reverse order is what
                // produces a row that claims a directory which is not there.
                diesel::sql_query(
                    "UPDATE rooms
                        SET provisioned_at = COALESCE(provisioned_at, now()),
                            state = 'idle',
                            state_changed_at = now()
                      WHERE id = $1 AND state = 'provisioning'",
                )
                .bind::<SqlUuid, _>(id)
                .execute(conn)
                .await?;

                event(
                    conn,
                    id,
                    "provisioned",
                    serde_json::json!({ "outcome": format!("{outcome:?}") }),
                )
                .await?;

                tracing::info!(room = %id, ?outcome, "provisioned");
                Ok(true)
            }
            .scope_boxed()
        })
        .await?;

    Ok(if done {
        Outcome::Done
    } else {
        Outcome::SkippedLocked
    })
}

async fn generation_sha(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Option<String>, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Bytea)]
        sha256: Vec<u8>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT g.sha256 FROM rooms r JOIN generations g ON g.id = r.generation_id WHERE r.id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .next()
        .map(|row| puna_core::hash::hex(&row.sha256)))
}

// -- starting ----------------------------------------------------------------------------------

/// The room's own columns that only a start reads.
struct StartInputs {
    save_interval_secs: i32,
    use_embedded_options: bool,
    /// `generations.slots` — **every** slot, groups included, because pahoa sizes its outbound
    /// budget from `slot_info.len()` and the connectable count would under-request memory.
    slot_count: i32,
}

async fn start(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    use diesel_async::{AsyncConnection, scoped_futures::ScopedFutureExt};

    let mut conn = ctx.pool.get().await?;
    let id = action.room;
    let lock_key = action.lock_key;
    let environment = ctx.environment;
    let orchestrator = ctx.orchestrator;

    // The port first, and under the lock: it is the one input a concurrent actor could take away.
    let allocated = conn
        .transaction::<Option<u16>, anyhow::Error, _>(|conn| {
            async move {
                if !leader::try_lock_room(conn, lock_key).await? {
                    return Ok(None);
                }
                // `allocate` rather than `allocate_pair`: a reclaim has a victim, and the victim is
                // owed a record. See `note_reclaim`.
                match port::allocate(&orchestrator, conn, environment, id).await {
                    Ok(allocation) => {
                        if let Some(victim) = allocation.reclaimed_from {
                            note_reclaim(conn, victim, id, allocation.base_port).await?;
                        }
                        Ok(Some(allocation.base_port))
                    }
                    Err(port::AllocError::Exhausted { .. }) => {
                        // A first-class outcome, not an error to retry: an operator has to free
                        // capacity. Retrying in a loop would spin against a full range.
                        puna_core::metrics::ROOM_STARTS
                            .with_label_values(&["port_exhausted"])
                            .inc();
                        anyhow::bail!(
                            "no ports available in the {} range: every pair is in use by a live \
                             room or quarantined",
                            environment.as_str()
                        )
                    }
                    Err(e) => Err(e.into()),
                }
            }
            .scope_boxed()
        })
        .await;

    let base_port = match allocated {
        Ok(Some(base)) => base,
        Ok(None) => return Ok(Outcome::SkippedLocked),
        Err(e) => {
            // Exhaustion and a database failure both land here; both are worth a `failed` room with
            // the reason on it, because both are things a person has to read.
            fail(ctx, action, &e.to_string()).await?;
            return Ok(Outcome::Done);
        }
    };

    let Some(room) = room::get(&mut conn, id).await? else {
        return Ok(Outcome::Done);
    };
    let Some(secrets) = room::secrets(&mut conn, id).await? else {
        return Ok(Outcome::Done);
    };
    let slots = slot::list(&mut conn, id).await?;
    let inputs = start_inputs(&mut conn, id).await?;

    // Fail closed, loudly. An incomplete `PAHOA_SLOT_PASSWORDS` is a room nobody can join, so the
    // builder refuses and the room lands in `failed` with the slots named -- which is recoverable,
    // where a room serving with the wrong door open is not.
    let environment_vars = match spec::secret::build(&room, &secrets, &slots) {
        Ok(data) => data,
        Err(e) => {
            fail(ctx, action, &e.to_string()).await?;
            return Ok(Outcome::Done);
        }
    };

    let room_spec = spec::room::Draft {
        room_id: id,
        image: ctx.pahoa_image.to_string(),
        base_port,
        wants_filtered: room.wants_filtered,
        slot_count: inputs.slot_count,
        save_interval_secs: inputs.save_interval_secs,
        use_embedded_options: inputs.use_embedded_options,
    }
    .build(room.slot_auth, &environment_vars);

    let outcome = {
        let mut recorder = RowRecorder { conn: &mut conn };
        apply::ensure_room_running(
            ctx.cluster,
            &StartRequest {
                spec: &room_spec,
                secret: &environment_vars,
                site: ctx.site,
            },
            &mut recorder,
        )
        .await
    };

    match outcome {
        Ok(Started::Converged { .. }) => {
            let filtered = room.wants_filtered.then(|| i32::from(base_port) + 1);
            diesel::sql_query(
                "UPDATE rooms
                    SET state = 'starting', state_changed_at = now(),
                        advertised_host = $2, advertised_port = $3, advertised_filtered_port = $4,
                        last_error = NULL
                  WHERE id = $1",
            )
            .bind::<SqlUuid, _>(id)
            .bind::<Text, _>(ctx.advertise_host)
            .bind::<Integer, _>(i32::from(base_port))
            .bind::<Nullable<Integer>, _>(filtered)
            .execute(&mut conn)
            .await?;

            event(
                &mut conn,
                id,
                "starting",
                serde_json::json!({ "port": base_port, "filtered_port": filtered }),
            )
            .await?;
            tracing::info!(room = %id, port = base_port, "objects created; waiting for the pod");
        }

        Ok(Started::AwaitingAddress) => {
            // Left in `idle` on purpose. A room may not be advertised before its address is known,
            // and `Start` is idempotent -- so the next tick runs it again and converges, rather than
            // parking in `starting` where nothing would ever re-read the address.
            tracing::warn!(
                room = %id,
                "the load balancer has not assigned an address yet; the next tick will look again"
            );
        }

        Ok(Started::Recreating) => {
            clear_deployment(&mut conn, id, "the running spec no longer matches the row").await?;
        }

        Ok(Started::AddressMismatch { observed }) => {
            puna_core::metrics::PORT_IP_MISMATCH.inc();
            puna_core::metrics::ROOM_STARTS
                .with_label_values(&["ip_mismatch"])
                .inc();
            tracing::error!(
                room = %id,
                port = base_port,
                observed = %observed,
                expected = %ctx.site.lb_ip,
                "the room's Service was given an address Puna did not ask for. Sharing degraded: \
                 the room would have been healthy and unreachable by name. Quarantining the pair."
            );

            port::quarantine(
                &ctx.orchestrator,
                &mut conn,
                ctx.environment,
                base_port,
                Utc::now() + QUARANTINE,
            )
            .await?;
            clear_deployment(
                &mut conn,
                id,
                "the load balancer assigned the wrong address",
            )
            .await?;
            event(
                &mut conn,
                id,
                "ip_mismatch",
                serde_json::json!({ "port": base_port, "observed": observed }),
            )
            .await?;
        }

        Err(e) => {
            // A fatal cluster error stops this room rather than failing identically every 30
            // seconds; a transient one is left to the next tick, which is the retry.
            let fatal = matches!(
                &e,
                apply::ApplyError::Cluster(c) if c.is_fatal()
            );
            if fatal {
                fail(ctx, action, &e.to_string()).await?;
            } else {
                tracing::warn!(room = %id, error = ?e, "the start attempt did not finish");
            }
        }
    }

    Ok(Outcome::Done)
}

/// Record that a pair was taken from an idle room and given to another.
///
/// **The victim gets the row, not the taker**, because the victim is the one who will be surprised:
/// a reclaimed port invalidates the address embedded in every patch its players have already
/// downloaded, so "why does my client connect to somebody else's room" has an answer in the room's
/// own history. The reservation is a weak claim by design — honoured while nothing else needs the
/// port — and this is what makes that claim's expiry visible rather than silent.
async fn note_reclaim(
    conn: &mut AsyncPgConnection,
    victim: RoomId,
    taken_by: RoomId,
    base_port: u16,
) -> Result<(), diesel::result::Error> {
    puna_core::metrics::PORT_RECLAIMS.inc();
    tracing::warn!(
        room = %victim,
        taken_by = %taken_by,
        port = base_port,
        "this room's port was reclaimed: the range had no free pair. Patches already downloaded \
         from it carry an address that is now another room's."
    );

    event(
        conn,
        victim,
        "port_reclaimed",
        serde_json::json!({ "port": base_port, "taken_by": taken_by.to_string() }),
    )
    .await
}

async fn start_inputs(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<StartInputs, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Integer)]
        save_interval_secs: i32,
        #[diesel(sql_type = Bool)]
        use_embedded_options: bool,
        #[diesel(sql_type = Integer)]
        slot_count: i32,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT r.save_interval_secs, r.use_embedded_options, g.slots AS slot_count
           FROM rooms r JOIN generations g ON g.id = r.generation_id
          WHERE r.id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;

    rows.into_iter()
        .next()
        .map(|row| StartInputs {
            save_interval_secs: row.save_interval_secs,
            use_embedded_options: row.use_embedded_options,
            slot_count: row.slot_count,
        })
        .ok_or(diesel::result::Error::NotFound)
}

/// Writes the uid and hash down, in the middle of the start sequence.
///
/// **Before anything is owned by that Deployment**, which is the property that makes a crash here
/// recoverable: the next tick reads the uid off the live object and carries on.
struct RowRecorder<'a> {
    conn: &'a mut AsyncPgConnection,
}

#[async_trait::async_trait]
impl DeploymentRecorder for RowRecorder<'_> {
    async fn record(&mut self, room: RoomId, uid: &str, spec_hash: &str) -> anyhow::Result<()> {
        diesel::sql_query("UPDATE rooms SET deployment_uid = $2, spec_hash = $3 WHERE id = $1")
            .bind::<SqlUuid, _>(room)
            .bind::<Text, _>(uid)
            .bind::<Text, _>(spec_hash)
            .execute(&mut *self.conn)
            .await?;
        Ok(())
    }
}

// -- the observed-state transitions ------------------------------------------------------------

async fn recreate(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;
    // Not a rolling update: the port is in the args and the Service, and pahoa holds an exclusive
    // flock on the save directory, so two pods cannot overlap. Dropping to `idle` lets the next tick
    // take the ordinary Start path -- one code path for creating a room's objects.
    ctx.cluster
        .delete_deployment(&object_name(action.room))
        .await?;
    clear_deployment(&mut conn, action.room, "the room's spec changed").await?;
    Ok(Outcome::Done)
}

async fn mark_running(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Double)]
        waited_seconds: f64,
    }

    // `failure_count` is cleared here and nowhere else: a room that has been up is not still
    // carrying the backoff from a failure three weeks ago.
    //
    // The elapsed time comes back from the same statement, computed by Postgres. It spans a request
    // the web tier wrote and a transition this process makes, so measuring it against a local clock
    // would fold two machines' skew into the one number a player actually experiences.
    let rows: Vec<Row> = diesel::sql_query(
        "UPDATE rooms
            SET state = 'running', state_changed_at = now(),
                started_at = COALESCE(started_at, now()),
                not_ready_sweeps = 0, failure_count = 0, retry_after = NULL, last_error = NULL
          WHERE id = $1
        RETURNING EXTRACT(EPOCH FROM (now() - desired_at))::float8 AS waited_seconds",
    )
    .bind::<SqlUuid, _>(action.room)
    .load(&mut conn)
    .await?;

    // Desired-to-running: it starts when somebody asked, not when the orchestrator got to it, which
    // is the difference between measuring the cold start and measuring the last hop of it.
    if let Some(waited) = rows.into_iter().next().map(|row| row.waited_seconds)
        && waited >= 0.0
    {
        puna_core::metrics::ROOM_START_SECONDS.observe(waited);
    }

    puna_core::metrics::ROOM_STARTS
        .with_label_values(&["ok"])
        .inc();
    event(&mut conn, action.room, "running", serde_json::json!({})).await?;
    tracing::info!(room = %action.room, "running");
    Ok(Outcome::Done)
}

/// A sweep with no ready replica, and — on the third — the degraded state itself.
///
/// Counted on the row rather than in memory so a leader handover does not reset it.
async fn bump_not_ready(
    ctx: &Context<'_>,
    action: &Action,
    degraded: bool,
) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;

    if degraded {
        diesel::sql_query(
            "UPDATE rooms
                SET state = 'degraded', state_changed_at = now(),
                    not_ready_sweeps = not_ready_sweeps + 1
              WHERE id = $1",
        )
        .bind::<SqlUuid, _>(action.room)
        .execute(&mut conn)
        .await?;

        // Still live for the allocator: players may be mid-reconnect, so the pair is not
        // reclaimable. Degraded is a report, not a teardown.
        tracing::warn!(
            room = %action.room,
            "no ready replica for three sweeps; the room is degraded and keeps its port"
        );
        event(&mut conn, action.room, "degraded", serde_json::json!({})).await?;
    } else {
        diesel::sql_query("UPDATE rooms SET not_ready_sweeps = not_ready_sweeps + 1 WHERE id = $1")
            .bind::<SqlUuid, _>(action.room)
            .execute(&mut conn)
            .await?;
    }

    Ok(Outcome::Done)
}

async fn mark_idle(
    ctx: &Context<'_>,
    action: &Action,
    reason: IdleReason,
) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;
    let (kind, note) = match reason {
        IdleReason::DeploymentGone => (
            "deployment_gone",
            "the room's Deployment is gone and Puna did not remove it",
        ),
        IdleReason::StopComplete => ("stopped", "the room stopped"),
    };

    clear_deployment(&mut conn, action.room, note).await?;
    event(&mut conn, action.room, kind, serde_json::json!({})).await?;

    match reason {
        // Worth a warning: somebody or something removed a room's Deployment out from under Puna.
        IdleReason::DeploymentGone => tracing::warn!(room = %action.room, "{note}"),
        IdleReason::StopComplete => tracing::info!(room = %action.room, "{note}"),
    }
    Ok(Outcome::Done)
}

async fn stop(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;

    // Deleting the Deployment is the graceful path today: the pod gets SIGTERM and 45 seconds, which
    // is what pahoa's final save needs. `POST /admin/v1/shutdown` is a nicety on top of it and
    // arrives with the probe at M11 -- it saves a few seconds, not a save.
    ctx.cluster
        .delete_deployment(&object_name(action.room))
        .await?;

    diesel::sql_query(
        "UPDATE rooms SET state = 'stopping', state_changed_at = now() WHERE id = $1",
    )
    .bind::<SqlUuid, _>(action.room)
    .execute(&mut conn)
    .await?;

    event(&mut conn, action.room, "stopping", serde_json::json!({})).await?;
    tracing::info!(room = %action.room, "stopping");
    Ok(Outcome::Done)
}

async fn retry(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;

    // `failure_count` survives: the backoff keeps growing until a start actually succeeds, which is
    // what stops a permanently broken room from being retried every fifteen seconds forever.
    diesel::sql_query(
        "UPDATE rooms
            SET state = 'idle', state_changed_at = now(), retry_after = NULL
          WHERE id = $1 AND state = 'failed'",
    )
    .bind::<SqlUuid, _>(action.room)
    .execute(&mut conn)
    .await?;

    event(&mut conn, action.room, "retrying", serde_json::json!({})).await?;
    Ok(Outcome::Done)
}

/// The §7 deletion sequence.
///
/// Ordered so that a crash leaves a *recoverable* directory rather than an orphaned one: the
/// directory moves to the trash before the row goes, so the worst case is a room whose row points at
/// a directory now in the trash — which the integrity check finds and an operator can undo. The other
/// order would delete the row and leave a directory nothing references.
async fn delete(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    use diesel_async::{AsyncConnection, scoped_futures::ScopedFutureExt};

    let mut conn = ctx.pool.get().await?;
    let id = action.room;
    let lock_key = action.lock_key;
    let layout = ctx.layout.clone();

    apply::teardown_room(ctx.cluster, id).await?;

    // Deletion is FOREGROUND, so the Deployment outlives its pod: while it is still there, the room
    // is still running and its save directory is still being written to. Moving that directory now
    // would take it out from under a live process.
    if ctx
        .cluster
        .get_deployment(&object_name(id))
        .await?
        .is_some()
    {
        diesel::sql_query(
            "UPDATE rooms
                SET state = 'deleting', state_changed_at = now()
              WHERE id = $1 AND state <> 'deleting'",
        )
        .bind::<SqlUuid, _>(id)
        .execute(&mut conn)
        .await?;
        tracing::info!(room = %id, "waiting for the room's pod to finish shutting down");
        return Ok(Outcome::Done);
    }

    let done = conn
        .transaction::<bool, anyhow::Error, _>(|conn| {
            async move {
                if !leader::try_lock_room(conn, lock_key).await? {
                    return Ok(false);
                }

                // A timestamp in the name, so deleting and recreating a room twice in one day does
                // not collide in the trash -- and so an operator can tell which copy is which.
                let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                let moved = storage::trash(&layout, id, &stamp)?;

                // Members, slots, commands and events cascade. The port reservation is released by
                // the FK's ON DELETE SET NULL, which deliberately leaves `last_activity` alone so
                // the pair keeps its place in the LRU order.
                diesel::sql_query("DELETE FROM rooms WHERE id = $1")
                    .bind::<SqlUuid, _>(id)
                    .execute(conn)
                    .await?;

                tracing::info!(
                    room = %id,
                    trashed = ?moved,
                    "room deleted; its state directory is recoverable from the trash until the \
                     retention window expires"
                );
                Ok(true)
            }
            .scope_boxed()
        })
        .await?;

    Ok(if done {
        Outcome::Done
    } else {
        Outcome::SkippedLocked
    })
}

/// Record a failure and when to try again.
async fn fail(ctx: &Context<'_>, action: &Action, error: &str) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Integer)]
        failure_count: i32,
    }

    // One statement, so the count the backoff is computed from is the count that was stored.
    let rows: Vec<Row> = diesel::sql_query(
        "UPDATE rooms
            SET state = 'failed', state_changed_at = now(),
                failure_count = failure_count + 1,
                last_error = $2,
                advertised_host = NULL, advertised_port = NULL, advertised_filtered_port = NULL
          WHERE id = $1
        RETURNING failure_count",
    )
    .bind::<SqlUuid, _>(action.room)
    .bind::<Text, _>(error)
    // `&mut *conn` rather than `&mut conn`: diesel infers the connection type, so a pooled handle
    // has to be dereferenced explicitly instead of coerced.
    .load(&mut *conn)
    .await?;

    // `into_iter().next()`, not `first()`: diesel's `RunQueryDsl` is in scope and brings its own
    // `first` along, which resolves ahead of the slice method and produces an unreadable error.
    let failure_count = rows.into_iter().next().map_or(1, |row| row.failure_count);
    let retry_after = Utc::now() + retry_delay(failure_count, action.room);

    diesel::sql_query("UPDATE rooms SET retry_after = $2 WHERE id = $1")
        .bind::<SqlUuid, _>(action.room)
        .bind::<Timestamptz, _>(retry_after)
        .execute(&mut conn)
        .await?;

    puna_core::metrics::ROOM_STARTS
        .with_label_values(&["failed"])
        .inc();
    event(
        &mut conn,
        action.room,
        "failed",
        serde_json::json!({ "error": error, "retry_after": retry_after }),
    )
    .await?;
    tracing::error!(room = %action.room, %error, %retry_after, "room failed");

    let _ = ctx;
    Ok(Outcome::Done)
}

/// `15s * 2^failures`, capped at ten minutes, ±20%.
///
/// **The jitter comes from the room's own id, not a random number.** It is stable for a room and
/// spread across rooms, which is all a herd needs — and it keeps the backoff reproducible, so a
/// support question about when a room will retry has an answer that does not depend on what the
/// process happened to roll.
fn retry_delay(failure_count: i32, room: RoomId) -> chrono::TimeDelta {
    const BASE_SECS: i64 = 15;
    const CAP_SECS: i64 = 600;

    let exponent = failure_count.clamp(1, 8) - 1;
    let base = (BASE_SECS << exponent).min(CAP_SECS);

    // The last byte of the uuid, mapped to [-20%, +20%].
    let byte = i64::from(uuid::Uuid::from(room).as_bytes()[15]);
    let spread = base * 2 / 5; // 40% of base, so ±20% around it
    let jitter = spread * byte / 255 - spread / 2;

    chrono::TimeDelta::seconds(base + jitter)
}

// -- shared statements -------------------------------------------------------------------------

/// Put a room back to `idle`, forgetting the Deployment it used to have.
///
/// **The port reservation is deliberately untouched.** A room comes back on the same port, which is
/// the requirement the whole reservation table exists for -- and the reason idle teardown never
/// releases anything.
async fn clear_deployment(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    note: &str,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "UPDATE rooms
            SET state = 'idle', state_changed_at = now(),
                deployment_uid = NULL, spec_hash = NULL,
                advertised_host = NULL, advertised_port = NULL, advertised_filtered_port = NULL,
                not_ready_sweeps = 0, started_at = NULL, stopped_at = now(),
                last_error = $2
          WHERE id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Nullable<Text>, _>(Some(note))
    .execute(conn)
    .await?;
    Ok(())
}

async fn event(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    kind: &str,
    detail: serde_json::Value,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "INSERT INTO room_events (room_id, actor, kind, detail) VALUES ($1, 'orchestrator', $2, $3)",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Text, _>(kind)
    .bind::<diesel::sql_types::Jsonb, _>(detail)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backoff's shape: fifteen seconds doubling to a ten-minute ceiling.
    #[test]
    fn the_backoff_doubles_and_then_stops() {
        let room = RoomId::new();
        let secs = |failures| retry_delay(failures, room).num_seconds();

        // ±20%, so each step is checked as a band rather than a number.
        let within = |actual: i64, nominal: i64| {
            let slack = nominal * 2 / 5;
            (actual - nominal).abs() <= slack / 2 + 1
        };

        assert!(within(secs(1), 15), "{}", secs(1));
        assert!(within(secs(2), 30), "{}", secs(2));
        assert!(within(secs(3), 60), "{}", secs(3));
        assert!(within(secs(6), 480), "{}", secs(6));
        // Capped, and it stays capped however long a room has been broken.
        for failures in [7, 8, 50, i32::MAX] {
            assert!(
                within(secs(failures), 600),
                "{failures}: {}",
                secs(failures)
            );
        }
        // Never zero, and never negative: a retry that fires immediately is not a backoff.
        for failures in [0, 1, 2, 100] {
            assert!(secs(failures) > 0, "{failures}");
        }
    }

    /// Jitter exists to spread a herd, so it has to differ between rooms and hold for one.
    #[test]
    fn the_jitter_is_per_room_and_stable() {
        let room = RoomId::new();
        assert_eq!(retry_delay(4, room), retry_delay(4, room));

        let spread: std::collections::BTreeSet<i64> = (0..64)
            .map(|_| retry_delay(4, RoomId::new()).num_seconds())
            .collect();
        assert!(
            spread.len() > 8,
            "64 rooms produced only {} distinct delays; a herd would still be a herd",
            spread.len()
        );
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::cluster::fake::FakeCluster;
    use crate::plan::{Action, IdleReason, Step};
    use crate::testdb::{self, NewRoom};
    use puna_core::db::Pool;

    fn site() -> Site {
        Site {
            namespace: "puna-dev".into(),
            lb_ip: "38.246.56.121".into(),
            lb_sharing_key: "ap-lobby-public".into(),
            tls_secret: "puna-room-tls".into(),
            data_pvc: "puna-data".into(),
        }
    }

    fn context<'a>(
        pool: &'a Pool,
        cluster: &'a FakeCluster,
        layout: &'a Layout,
        site: &'a Site,
    ) -> Context<'a> {
        Context {
            pool,
            cluster,
            layout,
            site,
            environment: Environment::Dev,
            advertise_host: "mw.example",
            orchestrator: Orchestrator::assume_leader(),
            pahoa_image: "pahoa:test",
        }
    }

    async fn action(pool: &Pool, room: RoomId, step: Step) -> Action {
        let mut conn = pool.get().await.expect("connection");
        Action {
            room,
            lock_key: testdb::lock_key(&mut conn, room).await,
            step,
        }
    }

    /// The directory lands before the row claims it, and the row's claim is what `provisioned_at`
    /// means. A crash between the two is recoverable; the reverse order is an `integrity_fault`.
    #[tokio::test]
    async fn provisioning_creates_the_directory_and_then_claims_it() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let cluster = FakeCluster::new();
            let site = site();
            let ctx = context(&pool, &cluster, &layout, &site);

            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 4).await;
            let room = testdb::insert_room(&mut conn, generation, NewRoom::default()).await;

            let outcome = execute(&ctx, &action(&pool, room, Step::Provision).await)
                .await
                .expect("provision");
            assert_eq!(outcome, Outcome::Done);

            let observed = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(observed.state, "idle");
            assert!(observed.provisioned_at.is_some());
            assert!(
                layout.room(room).join("seed.archipelago").exists(),
                "the seed is copied in, so a room is self-contained"
            );
            assert!(
                testdb::event_kinds(&mut conn, room)
                    .await
                    .contains(&"provisioned".to_string())
            );

            // Level-triggered: running it again is a no-op rather than an error.
            execute(&ctx, &action(&pool, room, Step::Provision).await)
                .await
                .expect("second provision");
            assert_eq!(
                testdb::observed(&mut conn, room).await.unwrap().state,
                "idle"
            );
        })
        .await;
    }

    /// The whole start path, against the fake: a port, the objects, the uid written down, and a
    /// room that says where it will be.
    #[tokio::test]
    async fn starting_allocates_a_port_and_records_what_it_created() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let cluster = FakeCluster::new();
            let site = site();
            let ctx = context(&pool, &cluster, &layout, &site);

            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 4).await;
            let room = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "idle",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;
            testdb::insert_slot(&mut conn, room, 1, None).await;

            execute(&ctx, &action(&pool, room, Step::Start).await)
                .await
                .expect("start");

            let observed = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(observed.state, "starting");
            assert_eq!(observed.advertised_host.as_deref(), Some("mw.example"));

            let port = testdb::reservation(&mut conn, room)
                .await
                .expect("a reservation");
            assert_eq!(observed.advertised_port, Some(port));
            // The adjacent half of the pair, never allocated separately.
            assert_eq!(observed.advertised_filtered_port, Some(port + 1));

            // The uid and hash are persisted, which is what makes a crash here recoverable.
            assert!(observed.deployment_uid.is_some());
            assert!(observed.spec_hash.is_some());

            // And the cluster has exactly the three objects, all owned by that uid.
            let snapshot = cluster.snapshot().await.expect("snapshot");
            assert_eq!(
                snapshot.deployment(room).map(|d| d.uid.clone()),
                observed.deployment_uid
            );
            assert_eq!(
                snapshot.service(room).and_then(|s| s.owner_uid.clone()),
                observed.deployment_uid
            );
            assert_eq!(
                snapshot.secret(room).and_then(|s| s.owner_uid.clone()),
                observed.deployment_uid
            );
        })
        .await;
    }

    /// **The fail-closed rule, end to end.** A per-slot room with a slot that has no password would
    /// render a map that locks that player out, so the Secret builder refuses and the room lands in
    /// `failed` with the slots named -- rather than starting and quietly turning a player away.
    #[tokio::test]
    async fn an_incomplete_slot_password_map_fails_the_room_rather_than_starting_it() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let cluster = FakeCluster::new();
            let site = site();
            let ctx = context(&pool, &cluster, &layout, &site);

            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 2).await;
            let room = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "idle",
                    desired: "running",
                    slot_auth: "per_slot",
                },
            )
            .await;
            testdb::insert_slot(&mut conn, room, 1, Some("has-one")).await;
            testdb::insert_slot(&mut conn, room, 2, None).await;

            execute(&ctx, &action(&pool, room, Step::Start).await)
                .await
                .expect("the step itself succeeds; the room fails");

            let observed = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(observed.state, "failed");
            let error = observed.last_error.expect("a reason");
            assert!(error.contains("[2]"), "the failing slot is named: {error}");
            assert_eq!(observed.failure_count, 1);
            assert!(observed.retry_after.is_some(), "and it will be retried");

            // Nothing was created: a room that cannot be configured correctly must not run.
            assert!(
                cluster
                    .snapshot()
                    .await
                    .expect("snapshot")
                    .deployment(room)
                    .is_none(),
                "no Deployment for a room whose Secret was refused"
            );
        })
        .await;
    }

    /// The reservation is the point of the reservation table: a room that goes idle keeps its port,
    /// because coming back on the same address is the requirement the whole design rests on.
    #[tokio::test]
    async fn going_idle_forgets_the_deployment_and_keeps_the_port() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let cluster = FakeCluster::new();
            let site = site();
            let ctx = context(&pool, &cluster, &layout, &site);

            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 4).await;
            let room = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "idle",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;
            testdb::insert_slot(&mut conn, room, 1, None).await;

            execute(&ctx, &action(&pool, room, Step::Start).await)
                .await
                .expect("start");
            let port = testdb::reservation(&mut conn, room).await.expect("a port");

            execute(
                &ctx,
                &action(&pool, room, Step::MarkIdle(IdleReason::DeploymentGone)).await,
            )
            .await
            .expect("mark idle");

            let observed = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(observed.state, "idle");
            assert_eq!(
                observed.advertised_port, None,
                "not reachable, so not advertised"
            );
            assert_eq!(observed.deployment_uid, None);
            assert_eq!(
                observed.spec_hash, None,
                "the row stops claiming a Deployment"
            );

            assert_eq!(
                testdb::reservation(&mut conn, room).await,
                Some(port),
                "the room must come back on the same port"
            );
        })
        .await;
    }

    /// Deletion: the objects go, the directory moves to the trash, the row goes, and the pair is
    /// released -- in that order, so a crash leaves something recoverable.
    #[tokio::test]
    async fn deleting_a_room_trashes_its_directory_and_releases_its_pair() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let cluster = FakeCluster::new();
            let site = site();
            let ctx = context(&pool, &cluster, &layout, &site);

            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 4).await;
            let room = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "idle",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;
            testdb::insert_slot(&mut conn, room, 1, None).await;

            execute(&ctx, &action(&pool, room, Step::Provision).await)
                .await
                .expect("provision");
            execute(&ctx, &action(&pool, room, Step::Start).await)
                .await
                .expect("start");
            let port = testdb::reservation(&mut conn, room).await.expect("a port");

            let deletion = action(&pool, room, Step::Delete).await;
            execute(&ctx, &deletion).await.expect("delete");

            assert!(
                testdb::observed(&mut conn, room).await.is_none(),
                "the row is gone"
            );
            assert!(cluster.object_names().is_empty(), "and so are its objects");
            assert!(!layout.room(room).exists(), "the directory moved");
            assert!(
                std::fs::read_dir(layout.trash())
                    .expect("trash")
                    .filter_map(std::result::Result::ok)
                    .any(|entry| entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&room.to_string())),
                "the directory is recoverable from the trash, not destroyed"
            );

            // The pair is free again -- released by the FK's ON DELETE SET NULL -- and its place in
            // the LRU order is untouched, so it is not handed out ahead of never-used pairs.
            #[derive(diesel::QueryableByName)]
            struct Row {
                #[diesel(sql_type = Bool)]
                free: bool,
                #[diesel(sql_type = Bool)]
                never_used: bool,
            }
            let rows: Vec<Row> = diesel::sql_query(
                "SELECT room_id IS NULL AS free, last_activity > '-infinity' AS never_used
                   FROM port_reservations WHERE environment = 'dev' AND base_port = $1",
            )
            .bind::<Integer, _>(port)
            .load(&mut conn)
            .await
            .expect("read the reservation");
            let row = rows.into_iter().next().expect("the pair");
            assert!(row.free);
            assert!(row.never_used, "a released pair keeps its last_activity");
        })
        .await;
    }

    /// Failure, backoff, and the retry that clears it.
    #[tokio::test]
    async fn a_failure_backs_off_and_a_retry_returns_the_room_to_idle() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let cluster = FakeCluster::new();
            let site = site();
            let ctx = context(&pool, &cluster, &layout, &site);

            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 4).await;
            let room = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "starting",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;

            execute(&ctx, &action(&pool, room, Step::FailStart).await)
                .await
                .expect("fail");

            let failed = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(failed.state, "failed");
            assert_eq!(failed.failure_count, 1);
            let retry_after = failed.retry_after.expect("a backoff");
            assert!(
                retry_after > chrono::Utc::now(),
                "the backoff is in the future"
            );

            execute(&ctx, &action(&pool, room, Step::Retry).await)
                .await
                .expect("retry");

            let retried = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(retried.state, "idle");
            assert_eq!(retried.retry_after, None);
            assert_eq!(
                retried.failure_count, 1,
                "the count survives, so the backoff keeps growing until a start succeeds"
            );
        })
        .await;
    }

    /// The degraded counter lives on the row, so a leader handover does not reset it.
    #[tokio::test]
    async fn not_ready_sweeps_accumulate_and_then_degrade() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let cluster = FakeCluster::new();
            let site = site();
            let ctx = context(&pool, &cluster, &layout, &site);

            let mut conn = pool.get().await.expect("connection");
            let generation = testdb::insert_generation(&mut conn, &layout, 4).await;
            let room = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "running",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;

            execute(&ctx, &action(&pool, room, Step::NotReady).await)
                .await
                .expect("not ready");
            execute(&ctx, &action(&pool, room, Step::NotReady).await)
                .await
                .expect("not ready");
            let counted = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(counted.not_ready_sweeps, 2);
            assert_eq!(counted.state, "running", "not degraded yet");

            execute(&ctx, &action(&pool, room, Step::MarkDegraded).await)
                .await
                .expect("degraded");
            let degraded = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(degraded.state, "degraded");
            assert_eq!(degraded.not_ready_sweeps, 3);

            // Coming back clears the counter and the failure history both.
            execute(&ctx, &action(&pool, room, Step::MarkRunning).await)
                .await
                .expect("running");
            let running = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(running.state, "running");
            assert_eq!(running.not_ready_sweeps, 0);
            assert_eq!(running.failure_count, 0);
        })
        .await;
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;
    use crate::cluster::fake::FakeCluster;
    use crate::plan::{Action, Step};
    use crate::testdb::{self, NewRoom};

    /// A reclaimed port leaves a record **against the room that lost it**.
    ///
    /// The victim is the one who will be surprised: every patch its players have already downloaded
    /// carries an address that now belongs to somebody else. The reservation is a weak claim by
    /// design, and this is what keeps its expiry from being silent — the room page maps this event
    /// kind to a sentence, so "why does my client connect to another room" has an answer.
    #[tokio::test]
    async fn reclaiming_a_port_is_recorded_against_the_room_that_lost_it() {
        testdb::with_db(|pool| async move {
            let tmp = tempfile::tempdir().expect("tempdir");
            let layout = Layout::new(tmp.path());
            let cluster = FakeCluster::new();
            let site = crate::spec::Site {
                namespace: "puna-dev".into(),
                lb_ip: "38.246.56.121".into(),
                lb_sharing_key: "ap-lobby-public".into(),
                tls_secret: "puna-room-tls".into(),
                data_pvc: "puna-data".into(),
            };
            let ctx = Context {
                pool: &pool,
                cluster: &cluster,
                layout: &layout,
                site: &site,
                environment: Environment::Dev,
                advertise_host: "mw.example",
                orchestrator: Orchestrator::assume_leader(),
                pahoa_image: "pahoa:test",
            };

            let mut conn = pool.get().await.expect("connection");
            // One pair in the whole range, so the second room has nowhere else to go.
            testdb::shrink_range(&mut conn, 1).await;

            let generation = testdb::insert_generation(&mut conn, &layout, 2).await;
            let idle = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "idle",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;
            testdb::insert_slot(&mut conn, idle, 1, None).await;

            let start = async |room| Action {
                room,
                lock_key: testdb::lock_key(&mut pool.get().await.expect("connection"), room).await,
                step: Step::Start,
            };

            execute(&ctx, &start(idle).await)
                .await
                .expect("first start");
            let port = testdb::reservation(&mut conn, idle).await.expect("a port");

            // Put it back to idle, which is what makes it reclaimable: a live room is never a
            // victim, which the allocator enforces and the D4 test in puna-core pins.
            execute(
                &ctx,
                &Action {
                    room: idle,
                    lock_key: testdb::lock_key(&mut conn, idle).await,
                    step: Step::MarkIdle(crate::plan::IdleReason::StopComplete),
                },
            )
            .await
            .expect("idle");

            let newcomer = testdb::insert_room(
                &mut conn,
                generation,
                NewRoom {
                    state: "idle",
                    desired: "running",
                    ..Default::default()
                },
            )
            .await;
            testdb::insert_slot(&mut conn, newcomer, 1, None).await;

            execute(&ctx, &start(newcomer).await)
                .await
                .expect("second start");

            assert_eq!(
                testdb::reservation(&mut conn, newcomer).await,
                Some(port),
                "the only pair in the range moved to the newcomer"
            );
            assert_eq!(
                testdb::reservation(&mut conn, idle).await,
                None,
                "and the victim lost it"
            );

            let victim_events = testdb::event_kinds(&mut conn, idle).await;
            assert!(
                victim_events.contains(&"port_reclaimed".to_string()),
                "the room that lost its port has no record of it: {victim_events:?}"
            );
            // The taker's history says nothing about it: from its side this was an ordinary start.
            let taker_events = testdb::event_kinds(&mut conn, newcomer).await;
            assert!(!taker_events.contains(&"port_reclaimed".to_string()));

            // The victim's room and its state directory are untouched -- losing a port must never
            // mean losing a room.
            let observed = testdb::observed(&mut conn, idle)
                .await
                .expect("still there");
            assert_eq!(observed.state, "idle");
        })
        .await;
    }
}
