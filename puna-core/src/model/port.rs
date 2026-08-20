//! Port pair allocation.
//!
//! Each room gets an ADJACENT PAIR -- `base` and `base + 1` -- published as `game-full` and
//! `game-filtered`. One row per pair keyed on the even base port, never one row per port: two
//! rows would let a third allocation land on `base + 1` between the two inserts, whereas one row
//! makes the primary key itself protect the pair.
//!
//! Rows deliberately outlive the Services they describe. This is a RESERVATION table, not an
//! allocation table: a torn-down room must come back on the same port, and that requirement is
//! the reason Puna needs a database at all -- the Kubernetes API cannot answer it, because the
//! Service is deleted when the room is torn down.
//!
//! The allocator, in order:
//!   1. the room's own previous pair, if the reservation still points at that room
//!   2. any pair never allocated, chosen at RANDOM among them
//!   3. otherwise the least recently used reservation
//!
//! Steps 2 and 3 share one `ORDER BY`, because the table is pre-seeded with every pair and
//! `last_activity` defaults to `'-infinity'` -- so "never allocated" sorts first under the same
//! expression that means "least recently used". That collapse is why there is no sentinel and no
//! `COALESCE` here.
//!
//! ## Why the tie is broken randomly rather than by port
//!
//! Every never-allocated pair carries the same `'-infinity'`, so the tiebreak decides the whole
//! order in a fresh environment. Breaking it on `base_port` filled the range from the bottom
//! upward, which made the highest live port a **room counter**: see one room on 40036 and you know
//! roughly nineteen have ever existed. That is not information a room's address should carry,
//! least of all early in an environment's life when the number is small and telling.
//!
//! `random()` costs nothing here -- the ordering only ever decides *which* equally-good pair is
//! taken -- and it leaves both real properties intact, because `last_activity` still dominates:
//! never-allocated pairs are still taken before released ones, and released ones are still taken
//! oldest-first. It also breaks ties fairly in step 3, where `touch_live_rooms` stamps every live
//! room with the same `now()` and a port-ordered tiebreak would always pick the lowest.
//!
//! The cost is that allocation is no longer reproducible from the table alone. If that is ever
//! wanted back, a persistent random column ordered on instead of `random()` buys the same
//! unpredictability while keeping the sequence stable.
//!
//! Reservations are a WEAK claim, not a lease: honored while nothing else needs the port, taken
//! away when something does. There is deliberately no TTL and no expiry sweep -- reclaiming on
//! demand needs no constant, releases nothing while there is headroom, and degrades gracefully.

use diesel::sql_types::{Integer, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::Environment;
use crate::ids::RoomId;
use crate::model::Orchestrator;

#[derive(Debug, thiserror::Error)]
pub enum AllocError {
    /// Every pair in this environment's range is either quarantined or bound to a room that is
    /// currently serving players.
    ///
    /// A first-class, user-visible outcome rather than an error to retry: the UI says so, a
    /// metric counts it, and an operator frees capacity. Retrying would spin.
    #[error(
        "no ports available in the {environment} range: every pair is in use by a live room or \
         quarantined"
    )]
    Exhausted { environment: &'static str },

    #[error(transparent)]
    Db(#[from] diesel::result::Error),
}

#[derive(diesel::QueryableByName)]
struct BasePort {
    #[diesel(sql_type = Integer)]
    base_port: i32,
}

#[derive(diesel::QueryableByName)]
struct ReclaimedPort {
    #[diesel(sql_type = Integer)]
    base_port: i32,
    #[diesel(sql_type = diesel::sql_types::Nullable<SqlUuid>)]
    previous_room_id: Option<uuid::Uuid>,
}

/// States in which a room is actively serving players and must never lose its port.
///
/// THE DESIGN DOC OMITS THIS EXCLUSION and it matters: its ordering is own-port, never-allocated,
/// oldest `last_activity`. In normal operation a running room's `last_activity` is recent so it
/// sorts last, but under genuine exhaustion the allocator would take a port out from under
/// connected clients -- and Cilium does not report that as an error, so the symptom would be
/// players dropped from a room that still looks healthy. Failing loudly is the correct
/// degradation.
const LIVE_STATES: &str = "'starting','running','degraded'";

/// How many times to re-pick a candidate after losing it to a concurrent allocator.
///
/// Each loss means another allocator committed first, so it made progress; the range would have
/// to be under extraordinary contention for this many consecutive losses. Bounded rather than
/// unbounded so a genuine bug surfaces as an error instead of a spin.
const MAX_CONTENTION_RETRIES: usize = 16;

/// The outcome of a successful allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    /// The base of the pair; the room also owns `base_port + 1`.
    pub base_port: u16,
    /// The room this pair was taken from, if it was a reclaim.
    ///
    /// A reclaim silently invalidates the address embedded in patches the victim's players have
    /// already downloaded, so the caller records it against the victim's event log rather than
    /// only counting it.
    pub reclaimed_from: Option<RoomId>,
}

/// Allocate a base port for `room_id`.
///
/// The pair is `base` and `base + 1`. Idempotent for a room that already holds a reservation:
/// calling twice returns the same port and only refreshes `last_activity`.
///
/// ## Why this is three statements rather than the one the design sketches
///
/// The design collapses "any pair never allocated" and "least recently used" into a single
/// `ORDER BY`, because pre-seeding plus `'-infinity'` makes never-allocated sort first. That is
/// correct in isolation and WRONG under concurrency: the subquery orders on a READ COMMITTED
/// snapshot, and by the time a row is locked another allocator may already have taken it. The row
/// then still qualifies -- a just-allocated *idle* room is not live, so it is reclaimable -- and
/// the second allocator takes the port straight back out of the first one's hands.
///
/// The 64-way concurrency test caught exactly this: 59 distinct ports out of 64.
///
/// So the two rules are separate phases. An unbound pair is always preferred, and a bound pair is
/// only ever reclaimed when no unbound pair exists at all. Each phase re-checks the row's
/// eligibility in the OUTER statement, after the lock is taken, and retries on a lost race.
pub async fn allocate_pair(
    orchestrator: &Orchestrator,
    conn: &mut AsyncPgConnection,
    environment: Environment,
    room_id: RoomId,
) -> Result<u16, AllocError> {
    allocate(orchestrator, conn, environment, room_id)
        .await
        .map(|a| a.base_port)
}

/// [`allocate_pair`], but reporting whether the pair was reclaimed and from whom.
pub async fn allocate(
    _orchestrator: &Orchestrator,
    conn: &mut AsyncPgConnection,
    environment: Environment,
    room_id: RoomId,
) -> Result<Allocation, AllocError> {
    // Step 1: the room's own previous pair. The common case, and the reason a torn-down room
    // comes back on the address players already have.
    //
    // **Unless that pair is no longer inside the configured range**, in which case the room gets a
    // new one instead. Startup reconciliation already deletes out-of-range rows, so this is the
    // second guard rather than the first -- but the cost of missing it is a room brought back onto
    // a port the deployment does not own, which collides silently on a shared address rather than
    // erroring. Falling through to a fresh allocation is always safe; returning a stale port is
    // not.
    let existing: Vec<BasePort> = diesel::sql_query(
        "UPDATE port_reservations
            SET last_activity = now()
          WHERE environment = $1::puna_environment
            AND room_id = $2
            AND (quarantined_until IS NULL OR quarantined_until <= now())
            AND EXISTS (
                SELECT 1 FROM port_ranges r
                 WHERE r.environment = port_reservations.environment
                   AND port_reservations.base_port BETWEEN r.base_low AND r.base_high)
        RETURNING base_port",
    )
    .bind::<Text, _>(environment.as_str())
    .bind::<SqlUuid, _>(room_id)
    .load(conn)
    .await?;

    // `into_iter().next()` rather than `.first()`: `RunQueryDsl` is in scope and defines its own
    // `first()`, which takes a connection and shadows the slice method.
    // `into_iter().next()` rather than `.first()`: `RunQueryDsl` is in scope and defines its own
    // `first()`, which takes a connection and shadows the slice method.
    if let Some(row) = existing.into_iter().next() {
        return Ok(Allocation {
            base_port: row.base_port as u16,
            reclaimed_from: None,
        });
    }

    // The room may still hold a QUARANTINED reservation, which step 1 skipped. Release it before
    // allocating, or the partial unique index on room_id rejects the new binding -- and the room
    // would be stuck for as long as the quarantine lasts.
    release(_orchestrator, conn, room_id).await?;

    // **`port_ranges` is the authority in every phase below, not the rows.** Reservation rows are
    // the working set and can lag the configured range -- a range narrowed while the orchestrator
    // is running leaves rows behind until the next startup reconciles them. Selecting from rows
    // alone would then hand out a port the deployment no longer owns, which does not error: it
    // collides on the shared address and leaves the room reachable at a name DNS never mentions.
    // Filtering here means an inconsistent table can produce *no* port, never an invalid one.
    for _ in 0..MAX_CONTENTION_RETRIES {
        // Phase 2: an unbound pair. Ordered by `last_activity`, so never-allocated ('-infinity')
        // comes first and a recently *released* pair comes last -- which is what lets a room torn
        // down and restarted land back on its own port. Among the never-allocated, which all tie,
        // the pick is random; see the module note on why not `base_port`.
        //
        // `AND r.room_id IS NULL` on the OUTER update is the race guard: READ COMMITTED
        // re-evaluates it against the committed row version after locking, so a row taken by a
        // concurrent allocator updates zero rows and we pick again.
        let claimed: Vec<BasePort> = diesel::sql_query(
            "WITH candidate AS (
                 SELECT c.base_port
                   FROM port_reservations c
                  WHERE c.environment = $1::puna_environment
                    AND c.room_id IS NULL
                    AND (c.quarantined_until IS NULL OR c.quarantined_until <= now())
                    AND EXISTS (SELECT 1 FROM port_ranges pr
                                 WHERE pr.environment = c.environment
                                   AND c.base_port BETWEEN pr.base_low AND pr.base_high)
                  ORDER BY c.last_activity ASC, random()
                  LIMIT 1
                    FOR UPDATE SKIP LOCKED)
             UPDATE port_reservations r
                SET room_id = $2, last_activity = now(), quarantined_until = NULL
               FROM candidate c
              WHERE r.environment = $1::puna_environment
                AND r.base_port = c.base_port
                AND r.room_id IS NULL
            RETURNING r.base_port",
        )
        .bind::<Text, _>(environment.as_str())
        .bind::<SqlUuid, _>(room_id)
        .load(conn)
        .await?;

        if let Some(row) = claimed.into_iter().next() {
            return Ok(Allocation {
                base_port: row.base_port as u16,
                reclaimed_from: None,
            });
        }

        // Phase 3: no unbound pair exists, so reclaim the least recently used one that is not
        // serving players. Weak claim, not a lease -- honored while nothing else needs the port.
        //
        // The victim's room row and on-disk state are untouched: only the binding moves. Losing a
        // port must never mean losing a room.
        let reclaimed: Vec<ReclaimedPort> = diesel::sql_query(format!(
            "WITH victim AS (
                 SELECT c.base_port, c.room_id
                   FROM port_reservations c
                  WHERE c.environment = $1::puna_environment
                    AND c.room_id IS NOT NULL
                    AND (c.quarantined_until IS NULL OR c.quarantined_until <= now())
                    AND EXISTS (SELECT 1 FROM port_ranges pr
                                 WHERE pr.environment = c.environment
                                   AND c.base_port BETWEEN pr.base_low AND pr.base_high)
                    AND NOT EXISTS (SELECT 1 FROM rooms x
                                     WHERE x.id = c.room_id
                                       AND x.state IN ({LIVE_STATES}))
                  ORDER BY c.last_activity ASC, random()
                  LIMIT 1
                    FOR UPDATE SKIP LOCKED)
             UPDATE port_reservations r
                SET room_id = $2, last_activity = now(), quarantined_until = NULL
               FROM victim v
              WHERE r.environment = $1::puna_environment
                AND r.base_port = v.base_port
                AND r.room_id IS NOT DISTINCT FROM v.room_id
            RETURNING r.base_port, v.room_id AS previous_room_id"
        ))
        .bind::<Text, _>(environment.as_str())
        .bind::<SqlUuid, _>(room_id)
        .load(conn)
        .await?;

        if let Some(row) = reclaimed.into_iter().next() {
            return Ok(Allocation {
                base_port: row.base_port as u16,
                reclaimed_from: row.previous_room_id.map(RoomId::from),
            });
        }

        // Both phases found nothing. Either the range is genuinely full of live rooms, or every
        // candidate was taken between the pick and the lock. Distinguish by asking once whether
        // any candidate exists at all; if none does, this is exhaustion rather than contention.
        if !any_candidate_exists(conn, environment).await? {
            return Err(AllocError::Exhausted {
                environment: environment.as_str(),
            });
        }
    }

    Err(AllocError::Exhausted {
        environment: environment.as_str(),
    })
}

/// Is there any pair this environment could allocate, ignoring who wins the race?
async fn any_candidate_exists(
    conn: &mut AsyncPgConnection,
    environment: Environment,
) -> Result<bool, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    let rows: Vec<Row> = diesel::sql_query(format!(
        "SELECT count(*) AS n FROM port_reservations c
          WHERE c.environment = $1::puna_environment
            AND (c.quarantined_until IS NULL OR c.quarantined_until <= now())
            AND (c.room_id IS NULL
                 OR NOT EXISTS (SELECT 1 FROM rooms x
                                 WHERE x.id = c.room_id
                                   AND x.state IN ({LIVE_STATES})))"
    ))
    .bind::<Text, _>(environment.as_str())
    .load(conn)
    .await?;

    Ok(rows.into_iter().next().map(|r| r.n).unwrap_or(0) > 0)
}

/// Unbind whatever pair a room holds, leaving the reservation itself intact.
///
/// `last_activity` is deliberately NOT reset. The row keeps its position in the LRU ordering, so
/// a recently-released port is handed out last -- which is what makes a room torn down and
/// restarted land back on its own port.
///
/// This is the "reclaiming a port must null the binding and touch nothing else" rule: the room's
/// row and its on-disk state are untouched, because losing a port must never mean losing a room.
pub async fn release(
    _orchestrator: &Orchestrator,
    conn: &mut AsyncPgConnection,
    room_id: RoomId,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query("UPDATE port_reservations SET room_id = NULL WHERE room_id = $1")
        .bind::<SqlUuid, _>(room_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// The pair a room currently holds, if any.
///
/// **Read-only, and therefore not gated on [`Orchestrator`]**: the web tier calls this, and it is
/// the reason it can. A patch download embeds the room's address, and reservations are sticky, so a
/// patch taken from a room that is torn down already carries the address it will come back on —
/// which the lobby cannot do, because it only knows an address while a room is up.
///
/// The one thing that invalidates the answer is an LRU reclaim under range pressure, which is why
/// the room page stays authoritative and the reclaim writes a `room_events` row against the victim.
pub async fn reserved_pair(
    conn: &mut AsyncPgConnection,
    room_id: RoomId,
) -> Result<Option<u16>, diesel::result::Error> {
    let rows: Vec<BasePort> =
        diesel::sql_query("SELECT base_port FROM port_reservations WHERE room_id = $1")
            .bind::<SqlUuid, _>(room_id)
            .load(conn)
            .await?;

    Ok(rows
        .into_iter()
        .next()
        .map(|row| u16::try_from(row.base_port).unwrap_or_default())
        .filter(|port| *port != 0))
}

/// Hold a pair out of circulation until `until`.
///
/// Used when a Service comes up on an address other than the expected shared VIP. That collision
/// is necessarily with something Puna did not create -- Puna's own uniqueness is enforced here --
/// so the pair is parked rather than immediately retried.
pub async fn quarantine(
    _orchestrator: &Orchestrator,
    conn: &mut AsyncPgConnection,
    environment: Environment,
    base_port: u16,
    until: chrono::DateTime<chrono::Utc>,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query(
        "UPDATE port_reservations
            SET quarantined_until = $3, last_activity = now(), room_id = NULL
          WHERE environment = $1::puna_environment AND base_port = $2",
    )
    .bind::<Text, _>(environment.as_str())
    .bind::<Integer, _>(base_port as i32)
    .bind::<Timestamptz, _>(until)
    .execute(conn)
    .await?;
    Ok(())
}

/// Refresh `last_activity` for every pair bound to a room that is currently live.
///
/// Called each reconcile tick. This is what keeps the LRU ordering meaningful before pahoa
/// reports real activity: it degrades to "least recently *running*", which is still the right
/// victim order, rather than to "least recently allocated", which is not.
pub async fn touch_live_rooms(
    _orchestrator: &Orchestrator,
    conn: &mut AsyncPgConnection,
    environment: Environment,
) -> Result<usize, diesel::result::Error> {
    diesel::sql_query(format!(
        "UPDATE port_reservations p
            SET last_activity = now()
           FROM rooms r
          WHERE p.room_id = r.id
            AND p.environment = $1::puna_environment
            AND r.state IN ({LIVE_STATES})"
    ))
    .bind::<Text, _>(environment.as_str())
    .execute(conn)
    .await
}

/// Counts for the `puna_ports_*` gauges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortStats {
    pub total: i64,
    pub bound: i64,
    pub quarantined: i64,
}

pub async fn stats(
    conn: &mut AsyncPgConnection,
    environment: Environment,
) -> Result<PortStats, diesel::result::Error> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        total: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        bound: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        quarantined: i64,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT count(*) AS total,
                count(room_id) AS bound,
                count(*) FILTER (WHERE quarantined_until > now()) AS quarantined
           FROM port_reservations WHERE environment = $1::puna_environment",
    )
    .bind::<Text, _>(environment.as_str())
    .load(conn)
    .await?;

    let row = rows
        .into_iter()
        .next()
        .expect("aggregate always returns one row");
    Ok(PortStats {
        total: row.total,
        bound: row.bound,
        quarantined: row.quarantined,
    })
}

/// Refuse to start against a database belonging to the other environment.
///
/// Dev and prod have separate clusters, so this should be impossible -- but the two share one
/// public address and therefore one port space, and a `DATABASE_URL` pointed at the wrong one
/// would allocate from the wrong half. Cilium reports that as nothing at all: it silently hands
/// out a second IP and the losing room answers on an address DNS never mentions.
///
/// This is the cheapest of the three guards on that failure, and the only one that catches it
/// before a single port is allocated. The others are the CHECK constraint on the table and the
/// post-create ingress-IP read-back.
pub async fn assert_environment_matches(
    conn: &mut AsyncPgConnection,
    environment: Environment,
) -> anyhow::Result<()> {
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        foreign_bound: i64,
    }

    let rows: Vec<Row> = diesel::sql_query(
        "SELECT count(*) AS foreign_bound FROM port_reservations
          WHERE environment <> $1::puna_environment AND room_id IS NOT NULL",
    )
    .bind::<Text, _>(environment.as_str())
    .load(conn)
    .await?;

    let foreign = rows
        .into_iter()
        .next()
        .map(|r| r.foreign_bound)
        .unwrap_or(0);
    anyhow::ensure!(
        foreign == 0,
        "configured environment is {} but this database has {} port reservation(s) bound in the \
         other environment. Refusing to start: allocating from the wrong half of a shared port \
         space produces rooms that are unreachable rather than an error.",
        environment.as_str(),
        foreign,
    );
    Ok(())
}

/// Forget the environment this database does not serve.
///
/// Every database is seeded by the initial migration with reservations for **both** environments,
/// and the migration that made the range configurable backfilled a `port_ranges` row for each. Only
/// one of those is ever real: a database belongs to exactly one environment, and nothing else in
/// this module touches the other one -- `ensure_range` writes only its own row, and `allocate`
/// filters on `environment`, so the foreign rows are inert.
///
/// Inert but **misleading**, which is the reason to remove them. `port_ranges` is the table someone
/// reads to answer "which ports does this environment own", and a stale foreign row answers a
/// question nobody asked with a number that is no longer true -- the dev database's `prod` row still
/// claimed a range that dev itself had since expanded into.
///
/// ## Ordering, which is the part that matters
///
/// **[`assert_environment_matches`] must run first**, and this function must never be called before
/// it. That guard refuses to start when it finds foreign reservations *bound to rooms*, which is how
/// a `DATABASE_URL` pointed at the wrong environment is caught — the one mistake the design calls
/// unrecoverable. Cleaning up first would delete exactly the evidence it reads, turning a loud
/// refusal into a silent adoption of somebody else's database.
///
/// The range row goes only **if no foreign reservation remains**. The deletion above should have
/// ensured that, so the condition is a second opinion rather than the mechanism: if anything raced,
/// or the delete failed partway, the row that documents the other environment stays rather than
/// leaving the database describing a partition it no longer records.
pub async fn forget_foreign_environment(
    _orchestrator: &Orchestrator,
    conn: &mut AsyncPgConnection,
    environment: Environment,
) -> anyhow::Result<()> {
    // Unbound by construction: the assertion above refuses to start otherwise. Bound rows are a
    // wrong-database misconfiguration and are never this function's to resolve.
    let reservations = diesel::sql_query(
        "DELETE FROM port_reservations
          WHERE environment <> $1::puna_environment
            AND room_id IS NULL",
    )
    .bind::<Text, _>(environment.as_str())
    .execute(conn)
    .await?;

    let ranges = diesel::sql_query(
        "DELETE FROM port_ranges
          WHERE environment <> $1::puna_environment
            AND NOT EXISTS (
                SELECT 1 FROM port_reservations r
                 WHERE r.environment = port_ranges.environment)",
    )
    .bind::<Text, _>(environment.as_str())
    .execute(conn)
    .await?;

    if reservations > 0 || ranges > 0 {
        tracing::info!(
            environment = environment.as_str(),
            reservations,
            ranges,
            "removed rows belonging to the other environment; this database serves one"
        );
    }

    Ok(())
}

/// Record the configured port range and make the reservation rows match it.
///
/// Called once at orchestrator startup, before anything can allocate. Idempotent: the ordinary case
/// is a range that has not moved, which writes nothing.
///
/// **A range that has SHRUNK is the interesting case, and it never blocks startup.** A range is
/// configuration; refusing to run because it moved would mean one edit could wedge the orchestrator
/// for the whole environment, which is worse than anything it was protecting against.
///
/// Every reservation outside the new range is released, because a row that cannot legitimately be
/// allocated should not exist. What happens to the room depends on whether it is serving:
///
///   * **Live** — a restart is queued. The room is genuinely on a port this deployment no longer
///     claims, so it cannot simply stay there; the recreate stops it and [`allocate`] gives it a
///     fresh port inside the range. That goes through the same `redeploy_requested_at` signal an
///     operator's restart uses, so it inherits the per-tick cap and rolls gradually rather than
///     restarting an environment at once.
///   * **Anything else** — nothing to do. It is not serving on that port, and it will allocate a
///     valid one whenever it next starts.
pub async fn ensure_range(
    _orchestrator: &Orchestrator,
    conn: &mut AsyncPgConnection,
    environment: Environment,
    (low, high): (u16, u16),
) -> anyhow::Result<()> {
    #[derive(diesel::QueryableByName)]
    struct Stranded {
        #[diesel(sql_type = Integer)]
        base_port: i32,
        #[diesel(sql_type = SqlUuid)]
        room_id: RoomId,
        #[diesel(sql_type = diesel::sql_types::Bool)]
        live: bool,
    }

    let (low, high) = (i32::from(low), i32::from(high));

    // Read before writing, because the delete below destroys the evidence of which rooms were
    // affected -- and those are exactly the rooms that need a restart queued.
    let stranded: Vec<Stranded> = diesel::sql_query(format!(
        "SELECT p.base_port, p.room_id, (r.state IN ({LIVE_STATES})) AS live
           FROM port_reservations p
           JOIN rooms r ON r.id = p.room_id
          WHERE p.environment = $1::puna_environment
            AND (p.base_port < $2 OR p.base_port > $3)
          ORDER BY p.base_port"
    ))
    .bind::<Text, _>(environment.as_str())
    .bind::<Integer, _>(low)
    .bind::<Integer, _>(high)
    .load(conn)
    .await?;

    // The range first, because the trigger reads it and the seed below has to pass.
    diesel::sql_query(
        "INSERT INTO port_ranges (environment, base_low, base_high)
              VALUES ($1::puna_environment, $2, $3)
         ON CONFLICT (environment) DO UPDATE
                SET base_low = EXCLUDED.base_low,
                    base_high = EXCLUDED.base_high,
                    updated_at = now()
              WHERE port_ranges.base_low IS DISTINCT FROM EXCLUDED.base_low
                 OR port_ranges.base_high IS DISTINCT FROM EXCLUDED.base_high",
    )
    .bind::<Text, _>(environment.as_str())
    .bind::<Integer, _>(low)
    .bind::<Integer, _>(high)
    .execute(conn)
    .await?;

    // Everything outside the range, bound or not. A row for a port this deployment does not own is
    // a row that must never be handed out, and leaving a bound one in place would let the room
    // return to the same invalid port on its next start -- [`allocate`]'s first step is "the room's
    // own previous pair".
    let dropped = diesel::sql_query(
        "DELETE FROM port_reservations
          WHERE environment = $1::puna_environment
            AND (base_port < $2 OR base_port > $3)",
    )
    .bind::<Text, _>(environment.as_str())
    .bind::<Integer, _>(low)
    .bind::<Integer, _>(high)
    .execute(conn)
    .await?;

    // Every pair in range that has no row yet. `ON CONFLICT DO NOTHING` makes a widened range and
    // an unchanged one the same statement, and keeps every existing row's `last_activity` -- which
    // is the LRU ordering, and would be destroyed by a delete-and-reseed.
    let added = diesel::sql_query(
        "INSERT INTO port_reservations (environment, base_port)
              SELECT $1::puna_environment, p FROM generate_series($2, $3, 2) AS p
         ON CONFLICT (environment, base_port) DO NOTHING",
    )
    .bind::<Text, _>(environment.as_str())
    .bind::<Integer, _>(low)
    .bind::<Integer, _>(high)
    .execute(conn)
    .await?;

    // A live room genuinely IS serving on a port outside the range, so it cannot be left alone.
    // Queued through the ordinary redeploy signal rather than stopped here: that path already
    // stops the room, allocates a fresh port on the way back up, and is capped per tick -- so a
    // range change rolls through the environment instead of taking it down at once.
    let restarting: Vec<RoomId> = stranded
        .iter()
        .filter(|row| row.live)
        .map(|row| row.room_id)
        .collect();

    for row in &stranded {
        if row.live {
            tracing::warn!(
                room = %row.room_id,
                port = row.base_port,
                low,
                high,
                "this room is serving on a port outside the configured range; queueing a restart \
                 onto a valid one"
            );
        } else {
            tracing::info!(
                room = %row.room_id,
                port = row.base_port,
                "released a reservation outside the configured range; this room is not running, so \
                 it will simply take a valid port when it next starts"
            );
        }
    }

    if !restarting.is_empty() {
        crate::model::fleet::request_redeploy(conn, &restarting).await?;
    }

    if added > 0 || dropped > 0 {
        tracing::info!(
            environment = environment.as_str(),
            low,
            high,
            added,
            dropped,
            restarting = restarting.len(),
            "reconciled the port range against configuration"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::room::RoomState;

    /// The SQL literal and [`RoomState::is_live`] have to agree, and nothing else makes them.
    ///
    /// They are two spellings of D4: the allocator will not take a pair from a live room. If a
    /// state is added to the enum as live and not here, the exclusion silently stops covering it
    /// — and the symptom is a room losing its port while players are connected, which Cilium
    /// reports as nothing at all.
    #[test]
    fn the_live_state_list_matches_the_enum() {
        let expected = RoomState::ALL
            .into_iter()
            .filter(|s| s.is_live())
            .map(|s| format!("'{}'", s.as_sql()))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(LIVE_STATES, expected);
    }
}
