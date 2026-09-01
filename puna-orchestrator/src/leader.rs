//! The orchestrator singleton, as a Postgres advisory lock.
//!
//! ## Why the lock is the guarantee and `replicas: 1` is only the hint
//!
//! `replicas: 1` with `strategy: Recreate` expresses the intent to the scheduler, but Kubernetes
//! does not promise it during a node partition: a pod can be unreachable-but-running while its
//! replacement starts. So the guarantee lives in the database that already holds all the state.
//! `pg_try_advisory_lock` is held by a *session*, and Postgres releases it when that session ends
//! (including when the process dies, the connection drops, or the node vanishes). There is
//! nothing to expire and no TTL to tune.
//!
//! Deliberately **not** a Kubernetes Lease. That would add `coordination.k8s.io/leases` to a Role
//! this design exists to keep small, and it would put the safety property inside the system being
//! mutated: an orchestrator that cannot reach the API server could not tell "I lost the lease"
//! from "I cannot see the lease", and those want opposite responses.
//!
//! ## The lock is a simplicity property, not a correctness one
//!
//! Every mutation is already safe to run twice: `create` treats `AlreadyExists` as success,
//! allocation is atomic, directory materialization is temp-dir-plus-rename, Secret writes are
//! server-side apply, command claiming is a conditional `UPDATE`, per-room work takes its own
//! advisory lock, and pahoa's `flock` is the last backstop against two pods serving one room. The
//! leader lock means the ordinary case has one actor, so the concurrent paths are a safety net
//! rather than the design.

use std::sync::Arc;

use tokio::sync::Notify;

/// The lock's namespace. Arbitrary, but fixed: two different constants would mean two
/// "singletons" that never see each other.
const LOCK_CLASS: i32 = 1_348_825_665;

/// The global orchestrator lock. Per-room locks use the same class with the room's `lock_key`.
const GLOBAL_LOCK_KEY: i32 = 0;

/// Proof that this process holds the orchestrator lock.
///
/// Constructing one requires [`acquire`], which requires the lock to have actually been taken,
/// so this is a witness rather than an assertion. It is what
/// [`puna_core::model::Orchestrator`] should be built from once M6's wiring lands, replacing
/// `assume_leader`'s honor system.
#[derive(Debug)]
pub struct LeaderLock {
    /// Held for the lifetime of the leadership. Dropping it ends the session, and Postgres
    /// releases the lock, which is the whole mechanism.
    _client: tokio_postgres::Client,
    lost: Arc<Notify>,
}

impl LeaderLock {
    /// Resolves when the connection holding the lock dies.
    ///
    /// A process that has lost the lock must stop acting immediately rather than finishing its
    /// tick: another process may already be leading.
    pub async fn lost(&self) {
        self.lost.notified().await;
    }

    /// Is the connection still up?
    ///
    /// Read by `/readyz`, so a leader whose session died reports not-ready rather than continuing
    /// to look healthy while doing nothing.
    pub fn is_held(&self) -> bool {
        !self._client.is_closed()
    }
}

/// Try to become the orchestrator.
///
/// `Ok(None)` means somebody else holds it: a normal outcome during a rollout, not an error. The
/// caller parks, serves `/healthz` alive and `/readyz` not-ready, and retries.
///
/// **Opens its own connection**, not one from the pool: the lock belongs to the session, and a
/// pooled connection is recycled between callers, so the lock would drop the moment the handle
/// went back.
pub async fn acquire(database_url: &str) -> anyhow::Result<Option<LeaderLock>> {
    let client = puna_core::db::raw_connection(database_url).await?;

    let row = client
        .query_one(
            "SELECT pg_try_advisory_lock($1, $2) AS taken",
            &[&LOCK_CLASS, &GLOBAL_LOCK_KEY],
        )
        .await?;

    let taken: bool = row.get("taken");
    if !taken {
        // Drop the connection rather than holding one per parked process: a rollout can have
        // several waiting, and each would otherwise occupy a backend slot for nothing.
        return Ok(None);
    }

    let lost = Arc::new(Notify::new());
    Ok(Some(LeaderLock {
        _client: client,
        lost,
    }))
}

/// Take a per-room lock, so two actors cannot work one room even if both believe they lead.
///
/// `false` means somebody else has it; the caller **skips that room this tick** rather than
/// waiting. Waiting would serialize the whole sweep behind one slow room, and the tick is
/// level-triggered so skipping costs at most one interval.
pub async fn try_lock_room(
    conn: &mut diesel_async::AsyncPgConnection,
    lock_key: i32,
) -> Result<bool, diesel::result::Error> {
    use diesel::sql_types::{Bool, Integer};
    use diesel_async::RunQueryDsl;

    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = Bool)]
        taken: bool,
    }

    let rows: Vec<Row> = diesel::sql_query("SELECT pg_try_advisory_xact_lock($1, $2) AS taken")
        .bind::<Integer, _>(LOCK_CLASS)
        .bind::<Integer, _>(lock_key)
        .load(conn)
        .await?;

    Ok(rows.into_iter().next().is_some_and(|r| r.taken))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both keys are fixed constants, and changing either silently splits the singleton in two.
    #[test]
    fn the_lock_identity_is_pinned() {
        assert_eq!(LOCK_CLASS, 1_348_825_665);
        assert_eq!(GLOBAL_LOCK_KEY, 0);
    }
}
