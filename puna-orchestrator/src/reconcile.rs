//! The reconcile tick.
//!
//! **The sweep IS the reconcile loop.** One level-triggered pass that reads the world, diffs it
//! against the desired state, and applies -- rather than an edge-triggered worker plus a
//! reconciler to catch the edges it missed. Two loops over one room is the shape that produces
//! bugs nobody can reproduce, so there is one.
//!
//! Being level-triggered is what makes everything else cheap: re-running a tick is a no-op,
//! "requested twice while starting" is not a special case, and a lost `NOTIFY` costs latency
//! rather than correctness. **`NOTIFY` is latency; the tick is the contract.**
//!
//! M6 scope: `provisioning -> idle`, plus the filesystem sweeps. No Kubernetes call exists yet, so
//! nothing here starts a room.
//!
//! **The decisions this module makes inline are the ones [`crate::plan`] already expresses**, and
//! M7 rewires the tick through it: load every room, `plan()`, then apply each `Step` under its
//! room's advisory lock. It is deliberately not rewired yet, because at M6 there is no cluster to
//! snapshot and a planner handed an empty one would read every live room as vanished. The two
//! things that stay here either way are the ones that are not decisions about a room's state:
//! the integrity check and the orphan report, both of which are filesystem reads where a failure
//! must not be mistaken for an answer.

use std::time::Duration;

use puna_core::db::Pool;
use puna_core::ids::RoomId;
use puna_core::model::Orchestrator;

use crate::leader::{self, LeaderLock};
use crate::storage::{self, Layout};

/// What one tick did, for the log line and the metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickReport {
    pub provisioned: usize,
    pub deleted: usize,
    /// Directories with no row. **Counted, never removed** -- see `report_orphan_dirs`.
    pub orphan_dirs: usize,
    pub integrity_faults: usize,
    pub skipped_locked: usize,
    pub errors: usize,
}

/// Abandoned provisioning attempts older than this are swept.
const TEMP_DIR_MAX_AGE: Duration = Duration::from_secs(3600);

/// One pass.
///
/// Takes the [`LeaderLock`] by reference rather than trusting a flag: holding one is what
/// authorizes writing an observed column, and passing it in makes that visible at the call site
/// rather than assumed inside.
pub async fn tick(
    lock: &LeaderLock,
    pool: &Pool,
    layout: &Layout,
    orchestrator: Orchestrator,
) -> anyhow::Result<TickReport> {
    let mut report = TickReport::default();

    // A leader whose session died must stop rather than finish the pass: another process may
    // already have taken the lock, and the second half of this tick would be a second actor.
    if !lock.is_held() {
        anyhow::bail!("the leader lock was lost; refusing to reconcile");
    }

    let mut conn = pool.get().await?;

    for room in pending_provision(&mut conn).await? {
        match provision_one(&mut conn, layout, orchestrator, &room).await {
            Ok(true) => report.provisioned += 1,
            Ok(false) => report.skipped_locked += 1,
            Err(e) => {
                report.errors += 1;
                tracing::error!(room = %room.id, error = ?e, "provisioning failed");
            }
        }
    }

    report.deleted = process_deletions(&mut conn, layout, orchestrator).await?;
    report.integrity_faults = detect_integrity_faults(&mut conn, layout, orchestrator).await?;
    report.orphan_dirs = report_orphan_dirs(&mut conn, layout).await?;

    match storage::sweep_temp_dirs(layout, TEMP_DIR_MAX_AGE) {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "swept abandoned provisioning directories"),
        Err(e) => tracing::warn!(error = ?e, "sweeping temp directories failed"),
    }

    Ok(report)
}

/// A room waiting for its state directory, with the generation hash that fills it.
#[derive(Debug)]
struct Pending {
    id: RoomId,
    lock_key: i32,
    sha256: Vec<u8>,
}

async fn pending_provision(
    conn: &mut diesel_async::AsyncPgConnection,
) -> Result<Vec<Pending>, diesel::result::Error> {
    use diesel::sql_types::{Bytea, Integer, Uuid as SqlUuid};
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
        #[diesel(sql_type = Integer)]
        lock_key: i32,
        #[diesel(sql_type = Bytea)]
        sha256: Vec<u8>,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT r.id, r.lock_key, g.sha256
           FROM rooms r
           JOIN generations g ON g.id = r.generation_id
          WHERE r.state = 'provisioning' AND r.desired_state <> 'deleted'
          ORDER BY r.created_at",
    )
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Pending {
            id: row.id,
            lock_key: row.lock_key,
            sha256: row.sha256,
        })
        .collect())
}

/// Materialize one room's directory and move it to `idle`.
///
/// Returns `false` when another actor holds the room's lock, which is a skip rather than a
/// failure: the tick is level-triggered, so the next pass picks it up.
async fn provision_one(
    conn: &mut diesel_async::AsyncPgConnection,
    layout: &Layout,
    orchestrator: Orchestrator,
    room: &Pending,
) -> anyhow::Result<bool> {
    use diesel::sql_types::Uuid as SqlUuid;
    use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};

    let id = room.id;
    let lock_key = room.lock_key;
    let sha = puna_core::hash::hex(&room.sha256);
    let layout = layout.clone();

    let done = conn
        .transaction::<bool, anyhow::Error, _>(|conn| {
            async move {
                // A transaction-scoped lock, so it releases with the commit and cannot be leaked
                // by an early return. Skipping on contention rather than waiting keeps one slow
                // room from serializing the whole sweep behind it.
                if !leader::try_lock_room(conn, lock_key).await? {
                    return Ok(false);
                }

                let nonce = uuid::Uuid::new_v4().simple().to_string();
                let outcome = storage::provision(&layout, id, &sha, &nonce)?;

                // Only after the directory is on disk and fsynced. The reverse order is what
                // produces a row asserting a directory that is not there.
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

                diesel::sql_query(
                    "INSERT INTO room_events (room_id, actor, kind, detail)
                     VALUES ($1, 'orchestrator', 'provisioned', $2)",
                )
                .bind::<SqlUuid, _>(id)
                .bind::<diesel::sql_types::Jsonb, _>(serde_json::json!({
                    "outcome": format!("{outcome:?}"),
                }))
                .execute(conn)
                .await?;

                tracing::info!(room = %id, ?outcome, "provisioned");
                Ok(true)
            }
            .scope_boxed()
        })
        .await?;

    let _ = orchestrator; // the capability token; see `puna_core::model::Orchestrator`
    Ok(done)
}

/// Carry out `desired_state = 'deleted'`.
///
/// Order matters and is the reverse of provisioning: the **directory moves first**, then the row
/// goes. A crash between the two leaves a room whose row points at a directory that is now in the
/// trash, which the integrity check catches and an operator can undo. The other order would delete
/// the row and orphan the directory, which nothing would ever notice.
///
/// M6 has no Kubernetes call, so this runs only for rooms that never started. M7 puts the
/// Deployment teardown ahead of it.
async fn process_deletions(
    conn: &mut diesel_async::AsyncPgConnection,
    layout: &Layout,
    _orchestrator: Orchestrator,
) -> anyhow::Result<usize> {
    use diesel::sql_types::Uuid as SqlUuid;
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT id FROM rooms
          WHERE desired_state = 'deleted'
            AND state IN ('provisioning', 'idle', 'failed', 'integrity_fault')",
    )
    .load(conn)
    .await?;

    let mut deleted = 0;
    for row in rows {
        // A timestamp in the name, so deleting and recreating a room twice in a day does not
        // collide in the trash -- and so an operator can tell which copy is which.
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let moved = storage::trash(layout, row.id, &stamp)?;

        // Members, slots, commands and events cascade; the port reservation is released by the
        // FK's ON DELETE SET NULL, which deliberately leaves `last_activity` alone so the pair
        // keeps its place in the LRU order.
        diesel::sql_query("DELETE FROM rooms WHERE id = $1")
            .bind::<SqlUuid, _>(row.id)
            .execute(conn)
            .await?;

        deleted += 1;
        tracing::info!(
            room = %row.id,
            trashed = ?moved,
            "room deleted; its state directory is recoverable from the trash until the retention \
             window expires"
        );
    }

    Ok(deleted)
}

/// Count room directories with no row, and name them in the log.
///
/// **Reported, never deleted.** A directory with no row is either a bug or a database restored
/// from an older backup, and in the second case the directory holds the only copy of a player's
/// progress -- deleting it would destroy exactly the state that could repair the room.
/// `PUNA_ORPHAN_DELETE_AFTER` exists in the design and defaults to disabled for this reason.
async fn report_orphan_dirs(
    conn: &mut diesel_async::AsyncPgConnection,
    layout: &Layout,
) -> anyhow::Result<usize> {
    use diesel::sql_types::Uuid as SqlUuid;
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
    }

    let on_disk = storage::list_room_dirs(layout)?;
    if on_disk.is_empty() {
        return Ok(0);
    }

    let known: std::collections::HashSet<RoomId> = diesel::sql_query("SELECT id FROM rooms")
        .load::<Row>(conn)
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect();

    let orphans: Vec<RoomId> = on_disk
        .into_iter()
        .filter(|id| !known.contains(id))
        .collect();

    if !orphans.is_empty() {
        tracing::warn!(
            count = orphans.len(),
            rooms = ?orphans.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "room directories with no database row. NOT removed: if this database was restored \
             from a backup, these hold the only copy of that progress."
        );
    }
    Ok(orphans.len())
}

/// Rooms whose row claims a directory that is not there.
///
/// **Never auto-repaired.** Recreating the directory would replace a player's progress with an
/// empty room and look like a successful start, which is the one failure mode worth a loud,
/// terminal state instead of a retry.
async fn detect_integrity_faults(
    conn: &mut diesel_async::AsyncPgConnection,
    layout: &Layout,
    _orchestrator: Orchestrator,
) -> Result<usize, diesel::result::Error> {
    use diesel::sql_types::Uuid as SqlUuid;
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = SqlUuid)]
        id: RoomId,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT id FROM rooms
          WHERE provisioned_at IS NOT NULL
            AND state NOT IN ('deleting', 'integrity_fault')
            AND desired_state <> 'deleted'",
    )
    .load(conn)
    .await?;

    let mut faults = 0;
    for row in rows {
        if storage::room_exists(layout, row.id) {
            continue;
        }

        faults += 1;
        tracing::error!(
            room = %row.id,
            "INTEGRITY FAULT: provisioned_at is set but the room directory is missing. This is \
             not auto-repaired -- recreating it would replace saved progress with an empty room."
        );

        diesel::sql_query(
            "UPDATE rooms SET state = 'integrity_fault', state_changed_at = now() WHERE id = $1",
        )
        .bind::<SqlUuid, _>(row.id)
        .execute(conn)
        .await?;

        diesel::sql_query(
            "INSERT INTO room_events (room_id, actor, kind) VALUES ($1, 'reconcile', 'integrity_fault')",
        )
        .bind::<SqlUuid, _>(row.id)
        .execute(conn)
        .await?;
    }

    Ok(faults)
}
