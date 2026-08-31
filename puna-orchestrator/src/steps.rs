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
use puna_core::probe::RoomProbe;
use puna_core::room::{RoomEndpoint, Route};

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
    /// How rooms are reached, so a stop can ask before it deletes. Behind the same trait the probe
    /// pass uses, so a room on an old image degrades rather than erroring.
    pub probe: &'a dyn RoomProbe,
    pub room_route: &'a Route,
    pub probe_timeout: std::time::Duration,
}

impl Context<'_> {
    /// One room, as something that can be dialed.
    pub fn endpoint(&self, room: RoomId, base_port: u16) -> RoomEndpoint {
        RoomEndpoint {
            room,
            base_port,
            advertise_host: self.advertise_host.to_string(),
            route: self.room_route.clone(),
            timeout: self.probe_timeout,
        }
    }
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
        Step::Reap => reap(ctx, action).await,
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

    let rendered = match render_spec(&mut conn, ctx.pahoa_image, id, base_port).await {
        Ok(Some(rendered)) => rendered,
        // The room or its secrets are gone from under the tick. Nothing to start, and nothing worth
        // recording against a row that may not exist.
        Ok(None) => return Ok(Outcome::Done),
        // Fail closed, loudly. An incomplete `PAHOA_SLOT_PASSWORDS` is a room nobody can join, so
        // the builder refuses and the room lands in `failed` with the slots named -- which is
        // recoverable, where a room serving with the wrong door open is not.
        Err(e) => {
            fail(ctx, action, &e.to_string()).await?;
            return Ok(Outcome::Done);
        }
    };

    let outcome = {
        let mut recorder = RowRecorder { conn: &mut conn };
        apply::ensure_room_running(
            ctx.cluster,
            &StartRequest {
                spec: &rendered.spec,
                secret: &rendered.secret,
                site: ctx.site,
            },
            &mut recorder,
        )
        .await
    };

    match outcome {
        Ok(Started::Converged { .. }) => {
            let filtered = rendered
                .spec
                .wants_filtered
                .then(|| i32::from(base_port) + 1);
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

            // A room that has just been started IS running its current spec, so any request that
            // was waiting on it is satisfied -- including one made while it sat idle, where there
            // was no Deployment to recreate.
            clear_redeploy_request(&mut conn, id).await?;

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

        Ok(Started::AwaitingTeardown) => {
            // Left in `idle`, holding its reservation, and deliberately not an error: the previous
            // pod is still flushing its final save, which is the behavior the 45-second grace period
            // exists to protect. Nothing to do but let it finish.
            //
            // The planner normally catches this from the snapshot, so reaching here means the
            // snapshot was stale -- a live read is what the applier has that the planner does not.
            // At `debug` because it is expected and self-clearing; the room's own state says more.
            tracing::debug!(
                room = %id,
                "the previous Deployment is still draining; starting once it is gone"
            );
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

        // **Refused for a reason a different port cannot fix.** `no_pool`, a lost
        // `onelemur.com/lb-pool` label, a wrong sharing key, an ExternalTrafficPolicy the holder
        // will not share with -- all properties of the Service template or of `PUNA_LB_IP`, which
        // `spec::service` renders identically for every room in the environment. So this is never
        // one room's problem, and reallocating would have every room in the namespace quarantine a
        // pair an hour until the range drained, with nothing that was going to start starting.
        //
        // The reservation is therefore LEFT BOUND and nothing is quarantined: the room comes back on
        // its own port once somebody fixes the cause. It still fails, so the backoff paces it and
        // the reason lands on the room page -- which is the whole remedy here, since the fix is a
        // human editing a manifest.
        Ok(Started::AddressRefused { refusal }) if !refusal.is_port_collision() => {
            tracing::error!(
                room = %id,
                port = base_port,
                reason = %refusal.reason,
                detail = %refusal.message,
                "the load balancer refused this room's address for a reason that is not a port \
                 conflict. Every room here renders the same Service template, so this very likely \
                 affects the whole environment. The port pair is kept, not quarantined."
            );
            event(
                &mut conn,
                id,
                "address_unsatisfiable",
                serde_json::json!({
                    "port": base_port,
                    "reason": refusal.reason,
                    "detail": refusal.message,
                }),
            )
            .await?;

            return fail_with(
                ctx,
                action,
                &format!(
                    "the load balancer will not give this room an address: {} ({}). This is not a \
                     port conflict. Check PUNA_LB_IP, PUNA_LB_SHARING_KEY and the Service's \
                     lb-pool label, which are the same for every room here.",
                    refusal.message, refusal.reason
                ),
                "address_unsatisfiable",
            )
            .await;
        }

        Ok(Started::AddressRefused { refusal }) => {
            // Whose Service holds the port. `external` is operations -- somebody took a port on the
            // sharing key, and Puna's answer is to use a different one. `internal` is a Puna bug:
            // a Service of ours outliving its room, which the sweep reports and which no amount of
            // reallocating will fix. **Both quarantine and both move on** -- the label exists so an
            // alert can treat them differently, not so this code can.
            let conflict = match conflicting_puna_service(ctx, id, base_port).await {
                Ok(Some(name)) => {
                    tracing::error!(
                        room = %id,
                        port = base_port,
                        holder = %name,
                        "a Service Puna manages is holding this room's port. That is a leak, not a \
                         collision: reallocating moves this room and leaves the stale object behind."
                    );
                    "internal"
                }
                Ok(None) => "external",
                // Classification is diagnostic. Failing the start over it would turn a room that
                // needs a different port into a room that does not get one at all.
                Err(e) => {
                    tracing::warn!(room = %id, error = ?e, "could not attribute the port conflict");
                    "external"
                }
            };

            // `puna_room_starts_total` is NOT touched here: `fail_with` below owns that increment,
            // and counting it in both places would make the result labels sum to more than the
            // attempts.
            puna_core::metrics::PORT_REFUSALS
                .with_label_values(&[conflict])
                .inc();
            tracing::error!(
                room = %id,
                port = base_port,
                conflict,
                reason = %refusal.reason,
                detail = %refusal.message,
                "the load balancer refused to allocate this room's address. The port is already \
                 held on the shared address, so this pair can never be satisfied. Quarantining it \
                 and allocating another."
            );

            port::quarantine(
                &ctx.orchestrator,
                &mut conn,
                ctx.environment,
                base_port,
                Utc::now() + QUARANTINE,
            )
            .await?;
            event(
                &mut conn,
                id,
                "address_refused",
                serde_json::json!({
                    "port": base_port,
                    "conflict": conflict,
                    "reason": refusal.reason,
                    "detail": refusal.message,
                }),
            )
            .await?;
            drop(conn);

            // **`failed`, not `idle`, and this is the pacing.** An idle room that wants to run is
            // what `plan::converging` counts, so going back to `idle` would re-plan `Start` every
            // `PUNA_CONVERGE_INTERVAL` -- and every attempt quarantines another pair. One room could
            // take the whole range inside a couple of hours and put every OTHER room into port
            // exhaustion, which is a fleet-wide outage caused by one contested port.
            //
            // The existing backoff already means "keep trying, paced": 15s doubling to a ten-minute
            // cap, per room, cleared entirely by a successful start. So a one-off conflict costs the
            // room fifteen seconds and a persistent one settles at six attempts an hour instead of
            // six hundred. Nothing new to tune, and the reason lands on the room page where somebody
            // can read it.
            //
            // The backoff cannot be short-circuited here either: `attach_desired_spec_hashes` skips
            // a room with no reservation, and the quarantine above unbound this one -- so M10c's
            // "a changed spec interrupts the wait" needs a hash it cannot compute, and stays out of
            // the way until the room retries on its own.
            return fail_with(
                ctx,
                action,
                &format!(
                    "the load balancer refused this room's address on port {base_port}: \
                     {} ({})",
                    refusal.message, refusal.reason
                ),
                "address_refused",
            )
            .await;
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

/// Find a Service Puna manages that is already publishing this room's pair, if there is one.
///
/// Attribution for a refusal, and the reason [`crate::cluster::RoomService`] carries its ports at
/// all. The list is label-selected to `managed-by=puna`, so anything it returns is ours by
/// definition — the question is only whether one of them is sitting on the port this room was just
/// refused. The room's *own* Service is excluded by name: its Deployment has been deleted but the
/// garbage collector may not have caught up, and finding ourselves would report every refusal as a
/// leak.
///
/// A read rather than a decision. Both answers quarantine and reallocate.
async fn conflicting_puna_service(
    ctx: &Context<'_>,
    room: RoomId,
    base_port: u16,
) -> crate::cluster::Result<Option<String>> {
    let ours = crate::cluster::object_name(room);
    Ok(ctx
        .cluster
        .list_services()
        .await?
        .into_iter()
        .find(|service| {
            service.name != ours
                && service
                    .ports
                    .iter()
                    .any(|port| *port == base_port || *port == base_port + 1)
        })
        .map(|service| service.name))
}

/// Record that a pair was taken from an idle room and given to another.
///
/// **The victim gets the row, not the taker**, because the victim is the one who will be surprised:
/// a reclaimed port invalidates the address embedded in every patch its players have already
/// downloaded, so "why does my client connect to somebody else's room" has an answer in the room's
/// own history. The reservation is a weak claim by design — honored while nothing else needs the
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

/// A room's spec and the environment it is fingerprinted against, rendered together.
///
/// The two travel as one because the hash covers `slot_auth`, which reaches pahoa through the
/// Secret — so a spec handed around without the environment that produced it is a spec whose hash
/// cannot be explained.
pub(crate) struct RenderedSpec {
    pub spec: crate::cluster::RoomSpec,
    pub secret: spec::secret::SecretData,
}

/// Render a room's spec exactly as a start would, for a pair already reserved.
///
/// **One renderer, two callers, on purpose.** [`start`] renders to apply, and [`desired_spec_hash`]
/// renders to ask whether anything has changed. If those two ever disagreed the backoff interrupt
/// would either never fire or never stop firing — and both failures are silent, because each looks
/// exactly like the room being fine. Sharing the rendering makes the drift unrepresentable rather
/// than merely unlikely.
///
/// `Ok(None)` is a room or a secret that is no longer there: nothing to render, and nothing to
/// record against a row that may already be gone. `Err` is a spec that *cannot* be rendered — today
/// only the fail-closed Secret builder — which is the room's own configuration being wrong, and is
/// the one case worth a `failed` row.
pub(crate) async fn render_spec(
    conn: &mut AsyncPgConnection,
    pahoa_image: &str,
    id: RoomId,
    base_port: u16,
) -> anyhow::Result<Option<RenderedSpec>> {
    let Some(room) = room::get(conn, id).await? else {
        return Ok(None);
    };
    let Some(secrets) = room::secrets(conn, id).await? else {
        return Ok(None);
    };
    let slots = slot::list(conn, id).await?;
    let inputs = start_inputs(conn, id).await?;

    let secret = spec::secret::build(&room, &secrets, &slots)?;
    let spec = spec::room::Draft {
        room_id: id,
        image: pahoa_image.to_string(),
        base_port,
        wants_filtered: room.wants_filtered,
        slot_count: inputs.slot_count,
        save_interval_secs: inputs.save_interval_secs,
        use_embedded_options: inputs.use_embedded_options,
    }
    .build(room.slot_auth, &secret);

    Ok(Some(RenderedSpec { spec, secret }))
}

/// What this room's spec would hash to if it were started right now.
///
/// The planner's backoff interrupt compares this against the hash on the row. **Every failure
/// collapses to `None`, deliberately:** a room whose spec cannot be rendered has not "changed" in
/// any sense worth acting on — it would fail the same way again — so the honest answer is *no
/// opinion*, which leaves the backoff to do the job it is good at. `None` must never read as "the
/// spec changed", or one unrenderable room would retry on every tick forever.
pub(crate) async fn desired_spec_hash(
    conn: &mut AsyncPgConnection,
    pahoa_image: &str,
    id: RoomId,
) -> Option<String> {
    // A failed room keeps its reservation, so the port it would come back on is the port it had.
    // No reservation means nothing to render against, not a change.
    let base_port = port::reserved_pair(conn, id).await.ok().flatten()?;

    match render_spec(conn, pahoa_image, id, base_port).await {
        Ok(Some(rendered)) => Some(rendered.spec.spec_hash),
        _ => None,
    }
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
        // `desired_spec_hash` is cleared, not set to the value just recorded. It answers *what
        // would this room render to now*, and the hourly lane is what answers it -- so anything
        // written here would be an answer from a different question's clock.
        //
        // Leaving the old one in place is the bug this prevents: after a legitimate recreate the
        // room's `spec_hash` moves and the stale desired hash does not, so the admin table would
        // report spec drift on a room that had *just* been brought exactly up to date, for up to
        // an hour. NULL is "not computed yet", which is true and renders as no drift.
        diesel::sql_query(
            "UPDATE rooms
                SET deployment_uid = $2, spec_hash = $3, desired_spec_hash = NULL
              WHERE id = $1",
        )
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
    // **Asked, exactly as a stop is, and for a reason that is not politeness.** A redeploy used to
    // be the one teardown that let SIGTERM do the talking, which put `(SIGTERM)` in the journal for
    // the single most operator-driven action in the system while a reap — which nobody performs —
    // read as an admin request.
    //
    // It costs no downtime, which is the whole reason it is here: this is the room's start pistol
    // for the same quiesce the signal would have triggered, fired at an in-cluster round trip
    // instead of after a delete has reached a kubelet. The pod finishes draining no later than it
    // would have, and foreground deletion means the new one starts as soon as it does.
    ask_to_stop(
        ctx,
        &mut conn,
        action.room,
        "an operator redeployed this room",
    )
    .await;

    // Not a rolling update: the port is in the args and the Service, and pahoa holds an exclusive
    // flock on the save directory, so two pods cannot overlap. Dropping to `idle` lets the next tick
    // take the ordinary Start path -- one code path for creating a room's objects.
    ctx.cluster
        .delete_deployment(&object_name(action.room))
        .await?;
    clear_deployment(&mut conn, action.room, "the room's spec changed").await?;
    // **Consume the request in the same pass that acts on it.** A redeploy is an instruction, not
    // a state: left set, the planner would see it again on the next tick and recreate the room
    // again, forever, with no error anywhere and no way to tell from the outside that anything is
    // wrong beyond players being disconnected every thirty seconds.
    clear_redeploy_request(&mut conn, action.room).await?;
    Ok(Outcome::Done)
}

/// Clear a satisfied redeploy request.
///
/// Called from every step that leaves the room running its freshly-rendered spec: the recreate that
/// a request triggered, and `start`, which covers a request made against a room that was already
/// idle -- there is nothing to recreate there, and leaving the request set would bounce the room
/// the moment it came up.
async fn clear_redeploy_request(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query("UPDATE rooms SET redeploy_requested_at = NULL WHERE id = $1")
        .bind::<SqlUuid, _>(room)
        .execute(conn)
        .await?;
    Ok(())
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

    reapply_locks(ctx, &mut conn, action.room).await;
    reapply_filters(ctx, &mut conn, action.room).await;
    Ok(Outcome::Done)
}

/// Re-assert every locked slot on a room that has just come up.
///
/// **pahoa persists a lock in `room.save`, and Puna's row is the authority.** That split is
/// deliberate: the save is the one thing a recovery might reset, and a PVC recreated or a save
/// cleared to get a room started again would take every lock with it — silently, leaving somebody
/// barred in Puna's own records and able to connect. The Secret-based lock this replaced survived
/// that, because it lived in Puna's state; this is what buys the property back.
///
/// **Almost always a no-op**, which is what makes it affordable to run on every start: rooms with a
/// locked slot are rare, and a room with none makes no calls at all. It is idempotent besides —
/// locking an already-locked slot is the same answer twice.
///
/// Failures are logged and never propagated. The room *is* running, and refusing to record that
/// because a moderation action could not be re-asserted would be the worse trade — but it is logged
/// at ERROR rather than WARN, because the state it leaves is somebody who should be shut out and is
/// not, and nothing else in the system will notice.
async fn reapply_locks(ctx: &Context<'_>, conn: &mut AsyncPgConnection, room_id: RoomId) {
    let locked: Vec<i32> = match puna_core::model::slot::list(conn, room_id).await {
        Ok(slots) => slots
            .iter()
            .filter(|s| s.is_locked())
            .map(|s| s.slot_number)
            .collect(),
        Err(e) => {
            tracing::error!(room = %room_id, error = ?e, "could not read slots to re-apply locks");
            return;
        }
    };
    if locked.is_empty() {
        return;
    }

    let (Ok(Some(base_port)), Ok(Some(secrets))) = (
        port::reserved_pair(conn, room_id).await,
        room::secrets(conn, room_id).await,
    ) else {
        tracing::error!(
            room = %room_id,
            slots = ?locked,
            "could not reach this room to re-apply its locks; those slots may be able to connect"
        );
        return;
    };

    let endpoint = ctx.endpoint(room_id, base_port);
    for slot in locked {
        let command = puna_core::model::command::RoomCommand::LockSlot { slot, locked: true };
        match ctx
            .probe
            .execute(&endpoint, &secrets.admin_token, &command)
            .await
        {
            Ok(output) if output.ok => {
                tracing::info!(room = %room_id, slot, "re-applied a lock after the room started");
            }
            // A refusal is an answer, and here it means the room disagrees about a slot Puna has
            // barred -- worth the same volume as a transport failure, because the outcome is the
            // same: somebody who should be shut out is not.
            Ok(output) => tracing::error!(
                room = %room_id,
                slot,
                answer = ?output.output,
                "the room refused to re-apply a lock; that slot may be able to connect"
            ),
            Err(e) => tracing::error!(
                room = %room_id,
                slot,
                error = %e,
                "could not re-apply a lock; that slot may be able to connect"
            ),
        }
    }
}

/// Push Puna's traffic filters at a room that has just come up.
///
/// **The same reasoning as [`reapply_locks`] and a stronger case for it.** pahoa persists filters
/// into `room.save`, so a save reset or a recreated PVC takes every one of them silently — and
/// unlike a lock, a filter that has quietly stopped applying is invisible from every angle: the
/// room looks healthy, the slot is connected, and only the traffic is different.
///
/// `PUT` rather than `PATCH`, so this converges on Puna's intent whatever the room currently
/// believes. `PATCH` merges, and a re-assert loop that merges can never remove a rule.
///
/// **What it deliberately does NOT do: scrub filters Puna does not know about.** A slot Puna
/// believes follows the room gets no call, so a ruleset set directly through pahoa's API — which
/// that API exists to allow — survives here. Scrubbing would cost one call per slot on every start
/// of every room, to undo something nobody has yet done by accident. If it ever bites, the cheap
/// version is `/admin/v1/status`'s per-slot `filtered` flag, provided it reports a slot's OWN state
/// rather than its effective one.
///
/// Failures log at ERROR rather than WARN, for the reason locks do: the state they leave is a room
/// carrying traffic somebody decided it should not carry.
async fn reapply_filters(ctx: &Context<'_>, conn: &mut AsyncPgConnection, room_id: RoomId) {
    use puna_core::model::filter;

    let (room_rules, slots) = match (
        filter::room_filter(conn, room_id).await,
        filter::slot_filters(conn, room_id).await,
    ) {
        (Ok(room_rules), Ok(slots)) => (room_rules, slots),
        _ => {
            tracing::error!(room = %room_id, "could not read filters to re-apply them");
            return;
        }
    };

    // Nothing to say, so nothing is said -- and no call is made on the overwhelmingly common start
    // of a room that has never been filtered.
    if room_rules.is_none() && slots.is_empty() {
        return;
    }

    let (Ok(Some(base_port)), Ok(Some(secrets))) = (
        port::reserved_pair(conn, room_id).await,
        room::secrets(conn, room_id).await,
    ) else {
        tracing::error!(
            room = %room_id,
            "could not reach this room to re-apply its filters; it may be carrying traffic it \
             was configured to drop"
        );
        return;
    };

    let endpoint = ctx.endpoint(room_id, base_port);

    // The room's, first. A `None` here is a DELETE rather than an empty PUT: for a room the two
    // mean the same thing, and the delete also clears whatever a reset save left behind.
    let outcome = ctx
        .probe
        .set_filter(&endpoint, &secrets.admin_token, None, room_rules.as_deref())
        .await;
    if let Err(e) = outcome {
        tracing::error!(
            room = %room_id,
            error = %e,
            "could not re-apply the room's filter; the room may be carrying traffic it was \
             configured to drop"
        );
    }

    for (slot, state) in slots {
        // **The three states, and `Follows` is not in this list.** Only divergent slots have rows,
        // so every entry here is either its own ruleset or an explicit exemption -- and `to_stored`
        // is what turns the exemption into the `[]` that means "filtered by nothing", rather than
        // the delete that would make it follow the room again.
        let rules = state.to_stored();
        if let Err(e) = ctx
            .probe
            .set_filter(
                &endpoint,
                &secrets.admin_token,
                Some(slot),
                rules.as_deref(),
            )
            .await
        {
            tracing::error!(
                room = %room_id,
                slot,
                error = %e,
                "could not re-apply a slot's filter; that slot's traffic may not match what was \
                 configured"
            );
        }
    }

    tracing::info!(room = %room_id, "re-applied traffic filters after the room started");
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

/// Ask a room to stop, before its Deployment is deleted.
///
/// **Ask first, then delete anyway.** `POST /admin/v1/shutdown` lets the room quiesce, flush a final
/// save and release its `flock` on its own schedule rather than inside a 45-second grace period —
/// but it is a nicety on top of the delete, never a replacement for it. Deleting the Deployment is
/// what actually stops a room, and a room that cannot be asked (an old image, a missing Secret, a
/// wedged process) must still stop. So this returns nothing and never fails the caller.
///
/// The answer is `202` = **accepted, not finished**: quiescing closes every connection including the
/// one that asked, so a room that only answered when it was done could not answer at all. Nothing
/// here waits on completion; the planner watches for the Deployment to go away.
///
/// **It is not slower than letting SIGTERM do it, which is why every teardown path uses it.** pahoa
/// notifies its waiters and answers `202` before quiescing (`http/mod.rs`'s `shutdown`), and every
/// way out of a running room converges on the same quiesce and the same final save (`serve.rs`). So
/// the work is identical and this only starts it EARLIER — at the moment of an in-cluster round trip
/// rather than after a delete has propagated to a kubelet and become a signal.
///
/// `reason` reaches pahoa's log and nothing else: the journal's `stopped` record carries the literal
/// `"admin request"` for any admin-API shutdown, chosen on pahoa's side. Which is the point of
/// calling this from a redeploy — the alternative reads as `SIGTERM`, and a redeploy is the most
/// operator-driven thing in the system.
async fn ask_to_stop(
    ctx: &Context<'_>,
    conn: &mut diesel_async::AsyncPgConnection,
    room: RoomId,
    reason: &str,
) {
    if !ctx.probe.capabilities().graceful_shutdown {
        return;
    }
    let Ok(Some(base_port)) = port::reserved_pair(conn, room).await else {
        return;
    };
    let Ok(Some(secrets)) = room::secrets(conn, room).await else {
        return;
    };

    let endpoint = ctx.endpoint(room, base_port);
    match ctx
        .probe
        .request_shutdown(&endpoint, &secrets.admin_token, reason)
        .await
    {
        Ok(()) => tracing::info!(room = %room, reason, "the room accepted a graceful shutdown"),
        // Logged at debug: the delete that follows is the real mechanism, and a room that will not
        // answer is the ordinary case this degrades for rather than an incident.
        Err(e) => tracing::debug!(
            room = %room,
            error = %e,
            "the room did not accept a graceful shutdown; deleting its Deployment"
        ),
    }
}

async fn stop(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;

    ask_to_stop(ctx, &mut conn, action.room, "an operator stopped this room").await;

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

/// Record a failure and when to try again — and take the room's Deployment down with it.
///
/// **Every path into `failed` comes through here**, which is why the deletion belongs here rather
/// than beside [`Step::FailStart`]: the progress deadline, a port range with nothing left, and a
/// Secret that refuses to render all end in the same place, and all three leave a pod that is
/// either crashlooping or about to.
async fn fail(ctx: &Context<'_>, action: &Action, error: &str) -> anyhow::Result<Outcome> {
    fail_with(ctx, action, error, "failed").await
}

/// [`fail`], with the start-outcome label spelled out.
///
/// Exists because a refusal is a start failure that deserves its own `puna_room_starts_total`
/// series: it is paced and diagnosed differently from a room that will not come up, and folding it
/// into `failed` would hide the one outcome an operator can act on directly. **One writer either
/// way** — the alternative was incrementing a second label at the call site, which would have made
/// the result labels sum to more than the attempts.
async fn fail_with(
    ctx: &Context<'_>,
    action: &Action,
    error: &str,
    result: &str,
) -> anyhow::Result<Outcome> {
    // **Delete before recording, not after.** Left alone, a failed room's Deployment crashloops for
    // the entire backoff against a spec nothing will use -- burning restarts, holding a scheduling
    // slot, and making `kubectl delete pod` look broken, because the Deployment recreates the pod
    // from the same unusable spec the moment it goes.
    //
    // The order is chosen by what each crash window costs. Deleting first and crashing leaves the
    // room in `starting` with no Deployment, which the next tick resolves down the ordinary vanish
    // path for the price of one more start attempt. Recording first and crashing leaves the
    // crashlooping pod in place for the whole backoff with nothing scheduled to remove it -- which
    // is precisely the state this exists to end.
    //
    // The reservation is a database row and is untouched, so the room comes back on its own port.
    // The Service and the Secret are owned by the Deployment, so garbage collection takes them.
    if let Err(e) = ctx
        .cluster
        .delete_deployment(&object_name(action.room))
        .await
    {
        // Not fatal, and not propagated: recording *why* the room failed is the more valuable half,
        // and a Deployment that outlives the record is exactly what happened before this existed.
        // Warned rather than swallowed, because the next tick will not come back for it.
        tracing::warn!(
            room = %action.room,
            error = %e,
            "could not delete the Deployment of a room entering `failed`; it will keep restarting \
             until the room is retried"
        );
    }

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
        .with_label_values(&[result])
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
/// Take a room down because nobody has spoken in it for the configured window.
///
/// **It writes a REQUEST and nothing else.** `desired_state` goes to `stopped` and the ordinary
/// [`Step::Stop`] path does the teardown on a following pass, so there is exactly one code path
/// that stops a room and the reaper cannot drift from it.
///
/// That also settles what a reaped room looks like afterwards: identical to one somebody stopped.
/// Same page, same Start button, same port, same save — which is right, because from a player's
/// side that is what happened, and a room that idles out and returns on a URL hit is the design the
/// whole reservation table exists to support.
///
/// **This is the orchestrator writing a column §2 calls web-owned**, and it is the second place
/// that happens — `recreate` already clears `redeploy_requested_at`. The rule's purpose is that two
/// writers must not fight over one value, and these two do not: the web tier says `running` when
/// somebody opens the room, this says `stopped` when it falls quiet, and each is a level-triggered
/// statement about the present rather than a claim on the column. Guarded anyway, in the `WHERE`:
/// the update only fires while the row still says `running`, so a stop, close or delete that
/// arrived between the plan and the apply wins rather than being overwritten.
async fn reap(ctx: &Context<'_>, action: &Action) -> anyhow::Result<Outcome> {
    let mut conn = ctx.pool.get().await?;

    let marked = diesel::sql_query(
        "UPDATE rooms
            SET desired_state = 'stopped', desired_at = now()
          WHERE id = $1 AND desired_state = 'running'",
    )
    .bind::<SqlUuid, _>(action.room)
    .execute(&mut conn)
    .await?;

    if marked == 0 {
        // Somebody asked for something else in the window between planning and applying. Their
        // instruction is newer than this one and stands.
        tracing::debug!(room = %action.room, "reap skipped; the room is no longer wanted running");
        return Ok(Outcome::Done);
    }

    // Recorded against the room so the page can say WHY it stopped. Without this the room reads as
    // one somebody stopped by hand, and the organizer who did not stop it has no way to find out
    // what did -- which is the support conversation this row exists to end.
    puna_core::model::event::record(
        &mut conn,
        action.room,
        puna_core::model::event::Actor::Orchestrator,
        "reaped",
        serde_json::json!({}),
    )
    .await?;

    tracing::info!(
        room = %action.room,
        "no client has spoken for the idle timeout; asking the room to stop"
    );
    Ok(Outcome::Done)
}

pub(crate) async fn clear_deployment(
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
                -- Observed state describes a Deployment that no longer exists. Left behind, the
                -- admin table would report an image and two ages for a room that is not running --
                -- stale numbers that read as live ones, which is the failure mode a dashboard is
                -- worst at showing you.
                running_image = NULL, deployment_created_at = NULL, process_started_at = NULL,
                -- CLEARED, not set to the caller's note. This column used to receive the reason a
                -- room went idle, and every reason reaching here is benign: a stop finishing, a
                -- spec changing, a recreate. But the column is named last_error, the room page
                -- renders it in red, and /status publishes it under that name -- so an ordinary
                -- stop printed a red line saying the room stopped, underneath a line already
                -- saying the room is not running, and told every API consumer it had errored.
                --
                -- Genuine failures belong to fail(), which writes them here. Why a room went idle
                -- belongs in room_events, which is where the page's sentence comes from anyway,
                -- and in the log line below.
                last_error = NULL
          WHERE id = $1",
    )
    .bind::<SqlUuid, _>(room)
    .execute(conn)
    .await?;
    // The note's remaining job. It is the only record of WHY for the recreate paths, which write no
    // event of their own -- previously it survived only by sitting in a column that made it look
    // like a failure.
    tracing::info!(room = %room, "room is idle: {note}");
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

    /// **Every path that tears a room down asks it to stop first, and asks BEFORE deleting.**
    ///
    /// A source lint because nothing observable would change: the steps tests run against
    /// `TcpProbe`, whose `graceful_shutdown` is false precisely so they never dial, so deleting
    /// either call leaves the whole suite green. The room still stops — the delete is the real
    /// mechanism — and the only symptom is a word in a file: the `stopped` record says `SIGTERM`
    /// where it should say `admin request`.
    ///
    /// That word is the reason this exists. A redeploy was the one teardown that skipped the ask,
    /// so the most operator-driven action in the system read as an anonymous signal while an idle
    /// reap — which nobody performs — read as an admin request. Exactly backwards, and invisible
    /// from anywhere but the journal.
    ///
    /// The ordering is asserted too. Asking after the delete would be a request racing a SIGTERM
    /// for a process that is already going, which is not the head start the ask exists to give.
    #[test]
    fn every_teardown_asks_the_room_to_stop_before_deleting_it() {
        let source = include_str!("steps.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("the file has a non-test half");

        for step in ["async fn stop(", "async fn recreate("] {
            let body = source
                .split_once(step)
                .unwrap_or_else(|| panic!("{step} was renamed; re-point this lint"))
                .1;
            let body = body.split_once("\n}").expect("a terminated function").0;

            let ask = body.find("ask_to_stop(").unwrap_or_else(|| {
                panic!("{step} no longer asks the room to stop, so its teardown reads as SIGTERM")
            });
            let delete = body
                .find(".delete_deployment(")
                .unwrap_or_else(|| panic!("{step} no longer deletes the Deployment"));

            assert!(
                ask < delete,
                "{step} asks after deleting, which races the SIGTERM it exists to pre-empt"
            );
        }
    }

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
            namespace: "rooms-test".into(),
            lb_ip: "192.0.2.10".into(),
            lb_sharing_key: "shared-public".into(),
            tls_secret: "puna-room-tls".into(),
            data_pvc: "puna-data".into(),
            naming: crate::spec::Naming {
                room_key: "example.test/room".into(),
                lb_pool_key: "example.test/lb-pool".into(),
                lb_pool: "public".into(),
                spec_hash_annotation: "puna.example.test/spec-hash".into(),
            },
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
            // The fallback on purpose: these tests have no room to dial, and it is the probe whose
            // `request_shutdown` refuses -- so a stop here takes exactly the degrade path a room on
            // an old image would.
            probe: &puna_core::probe::TcpProbe,
            room_route: &Route::Public,
            probe_timeout: std::time::Duration::from_millis(50),
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

    /// A refused address is PACED, and the pacing is the point.
    ///
    /// Landing the room back in `idle` would be the obvious spelling — it keeps its objects gone
    /// and the next pass allocates a different pair, which is exactly what should happen. It is
    /// also what `plan::converging` counts, so the next pass is three seconds away and each one
    /// quarantines another pair. One contested port would take the whole range inside a couple of
    /// hours and put every other room into exhaustion.
    ///
    /// So this asserts the room is `failed` with a `retry_after`, which is the same "keep trying,
    /// paced" the backoff has always meant — and that the pair it could not have is quarantined
    /// rather than handed straight back.
    #[tokio::test]
    async fn a_refused_address_paces_the_room_rather_than_retrying_at_the_convergence_cadence() {
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

            // Cilium's verbatim shape. The `Reason:` suffix is what distinguishes a genuine
            // collision from a template fault, so an invented message would exercise the wrong arm.
            cluster.refuse_ingress(
                "already_allocated_incompatible_service",
                "The IP '38.246.56.121' is already allocated to an incompatible service. \
                 Reason: same port and protocol",
            );
            execute(&ctx, &action(&pool, room, Step::Start).await)
                .await
                .expect("start");

            let observed = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(
                observed.state, "failed",
                "an idle room that wants to run is re-planned every convergence pass, so this has \
                 to rest in `failed` or it burns a port pair every three seconds"
            );
            assert!(
                observed.retry_after.is_some(),
                "the wait is what paces it; without one the planner retries immediately"
            );
            assert_eq!(observed.failure_count, 1);
            // Cilium's own words, so the room page can explain itself.
            let error = observed.last_error.unwrap_or_default();
            assert!(
                error.contains("already_allocated_incompatible_service"),
                "the reason belongs on the room, got {error:?}"
            );

            // The pair is held out rather than handed straight back to the next allocation.
            assert_eq!(
                testdb::reservation(&mut conn, room).await,
                None,
                "a refused pair is unbound and quarantined"
            );
            // And nothing is left running on an address that was never assigned.
            let snapshot = cluster.snapshot().await.expect("snapshot");
            assert_eq!(snapshot.deployment(room), None);
        })
        .await;
    }

    /// A refusal that is not about the port must NOT spend one.
    ///
    /// `no_pool`, a missing `lb-pool` label, a wrong sharing key — all rendered identically for
    /// every room by `spec::service`, so they are never one room's problem. Quarantining here would
    /// have every room in the namespace burn a pair an hour until the range drained, with nothing
    /// that was going to start starting: a configuration mistake laundered into port exhaustion,
    /// pointing whoever reads the alert at two port ranges that are perfectly correct.
    #[tokio::test]
    async fn a_refusal_that_is_not_a_port_conflict_keeps_the_rooms_reservation() {
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

            cluster.refuse_ingress(
                "pool_selector_mismatch",
                "no pool selects this service, check its labels",
            );
            execute(&ctx, &action(&pool, room, Step::Start).await)
                .await
                .expect("start");

            let observed = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(observed.state, "failed");
            assert!(observed.retry_after.is_some());

            // The whole point: the pair is still this room's, so it comes back on its own address
            // the moment somebody fixes the manifest.
            assert!(
                testdb::reservation(&mut conn, room).await.is_some(),
                "a refusal that a different port cannot fix must not cost a port"
            );

            // And the operator is not sent looking for a port conflict there is none of.
            let error = observed.last_error.unwrap_or_default();
            assert!(
                error.contains("not a port conflict"),
                "the room has to say what kind of refusal this was, got {error:?}"
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

            // **Going idle is not an error, and `last_error` is where the page reads one from.**
            // This used to receive the reason the room went idle, so an ordinary stop rendered a
            // red line saying the room stopped underneath a line already saying it is not running
            // -- and told every `/status` consumer the room had errored. The reason lives in
            // `room_events` and the log; this column is for genuine failures, which `fail()` owns.
            assert_eq!(
                observed.last_error, None,
                "a benign transition left a note in the error column"
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

    /// **The one catastrophic-and-silent failure in M17.** A redeploy is a request, so it has to be
    /// consumed by the step that acts on it. Left set, the planner sees it again on the very next
    /// tick and recreates the room again — forever, with no error anywhere, presenting only as
    /// players being disconnected every thirty seconds for reasons nothing in Puna can explain.
    #[tokio::test]
    async fn a_redeploy_request_is_consumed_by_the_recreate_it_causes() {
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

            // Somebody presses the button.
            diesel::sql_query("UPDATE rooms SET redeploy_requested_at = now() WHERE id = $1")
                .bind::<SqlUuid, _>(room)
                .execute(&mut conn)
                .await
                .expect("request a redeploy");

            execute(&ctx, &action(&pool, room, Step::Recreate).await)
                .await
                .expect("recreate");

            assert_eq!(
                pending_redeploy(&mut conn, room).await,
                None,
                "the request outlived the recreate it caused, so the room will bounce every tick"
            );
            assert!(
                cluster
                    .snapshot()
                    .await
                    .expect("snapshot")
                    .deployment(room)
                    .is_none(),
                "and the recreate did delete the Deployment"
            );

            // A request made while the room is idle is satisfied by the start, since a room that
            // has just started is by definition running its current spec. Without this the room
            // would come up and be torn straight back down.
            diesel::sql_query("UPDATE rooms SET redeploy_requested_at = now() WHERE id = $1")
                .bind::<SqlUuid, _>(room)
                .execute(&mut conn)
                .await
                .expect("request a redeploy");

            execute(&ctx, &action(&pool, room, Step::Start).await)
                .await
                .expect("start");

            assert_eq!(
                pending_redeploy(&mut conn, room).await,
                None,
                "a start satisfies a request too -- there was nothing to recreate"
            );
        })
        .await;
    }

    async fn pending_redeploy(
        conn: &mut AsyncPgConnection,
        room: RoomId,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        #[derive(diesel::QueryableByName)]
        struct Row {
            #[diesel(sql_type = Nullable<Timestamptz>)]
            redeploy_requested_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let rows: Vec<Row> =
            diesel::sql_query("SELECT redeploy_requested_at FROM rooms WHERE id = $1")
                .bind::<SqlUuid, _>(room)
                .load(conn)
                .await
                .expect("read the room");
        rows.into_iter()
            .next()
            .and_then(|r| r.redeploy_requested_at)
    }

    /// **M10c, half one.** A room entering `failed` takes its Deployment with it. Left in place it
    /// crashloops for the whole backoff against a spec nothing will use — and it makes the obvious
    /// operator reflex look broken, because `kubectl delete pod` is answered by the Deployment
    /// putting the pod straight back.
    #[tokio::test]
    async fn entering_failed_deletes_the_rooms_deployment() {
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
            assert!(
                cluster
                    .snapshot()
                    .await
                    .expect("snapshot")
                    .deployment(room)
                    .is_some(),
                "the room needs a Deployment for this test to be able to lose one"
            );

            execute(&ctx, &action(&pool, room, Step::FailStart).await)
                .await
                .expect("fail");

            assert!(
                cluster
                    .snapshot()
                    .await
                    .expect("snapshot")
                    .deployment(room)
                    .is_none(),
                "the failed room kept its Deployment, which will crashloop for the whole backoff"
            );

            // **The reservation survives, and that is what makes the deletion safe**: the room comes
            // back on the same port, so nothing a player has already downloaded is invalidated.
            let failed = testdb::observed(&mut conn, room).await.expect("the room");
            assert_eq!(failed.state, "failed");
            assert!(
                testdb::reservation(&mut conn, room).await.is_some(),
                "the port pair was released along with the Deployment"
            );
        })
        .await;
    }

    /// **M10c, half two — the drift guard.** The backoff interrupt compares the hash `start`
    /// recorded against the hash `desired_spec_hash` recomputes, so those two renderings agreeing is
    /// **Starting a room clears the stale answer to "what would it render to now".**
    ///
    /// `desired_spec_hash` is written by the hourly lane and `spec_hash` by every start, so after a
    /// recreate the second moves and the first does not. Left in place, the admin table compares
    /// them, finds them different, and reports spec drift on the one room that was *just* brought
    /// exactly up to date — for up to an hour, and most visibly right after somebody pressed
    /// Restart to fix drift. NULL is "not computed yet", which renders as no drift.
    #[tokio::test]
    async fn starting_a_room_clears_the_stale_desired_spec_hash() {
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

            // An hour-old answer from before whatever changed.
            diesel::sql_query("UPDATE rooms SET desired_spec_hash = 'stale-hash' WHERE id = $1")
                .bind::<SqlUuid, _>(room)
                .execute(&mut conn)
                .await
                .expect("seed a stale hash");

            execute(&ctx, &action(&pool, room, Step::Start).await)
                .await
                .expect("start");

            #[derive(diesel::QueryableByName)]
            struct Row {
                #[diesel(sql_type = Nullable<Text>)]
                desired_spec_hash: Option<String>,
            }
            let rows: Vec<Row> =
                diesel::sql_query("SELECT desired_spec_hash FROM rooms WHERE id = $1")
                    .bind::<SqlUuid, _>(room)
                    .load(&mut conn)
                    .await
                    .expect("read the room");

            assert_eq!(
                rows.into_iter().next().and_then(|r| r.desired_spec_hash),
                None,
                "the stale hash survived the start, so the room will report phantom drift"
            );
        })
        .await;
    }

    /// the entire property. If they ever disagreed for a room nobody had touched, every failed room
    /// would retry on every tick and the backoff would be gone — silently, because a retrying room
    /// looks like a room being fixed.
    #[tokio::test]
    async fn the_recomputed_spec_hash_matches_the_one_start_recorded() {
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

            let recorded = testdb::observed(&mut conn, room)
                .await
                .expect("the room")
                .spec_hash
                .expect("start records a hash");

            assert_eq!(
                desired_spec_hash(&mut conn, "pahoa:test", room).await,
                Some(recorded.clone()),
                "nothing changed, so the recomputed hash must equal the recorded one"
            );

            // And it moves when the thing it fingerprints moves. This is the operator changing
            // `PUNA_PAHOA_IMAGE`, which is exactly the case M10c exists for.
            let after_a_new_image = desired_spec_hash(&mut conn, "pahoa:newer", room)
                .await
                .expect("a hash");
            assert_ne!(
                after_a_new_image, recorded,
                "a changed image left the spec hash alone, so a re-pinned room would sit out its \
                 backoff for nothing"
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
                namespace: "rooms-test".into(),
                lb_ip: "192.0.2.10".into(),
                lb_sharing_key: "shared-public".into(),
                tls_secret: "puna-room-tls".into(),
                data_pvc: "puna-data".into(),
                naming: crate::spec::Naming {
                    room_key: "example.test/room".into(),
                    lb_pool_key: "example.test/lb-pool".into(),
                    lb_pool: "public".into(),
                    spec_hash_annotation: "puna.example.test/spec-hash".into(),
                },
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
                probe: &puna_core::probe::TcpProbe,
                room_route: &Route::Public,
                probe_timeout: std::time::Duration::from_millis(50),
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
