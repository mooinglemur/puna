//! One registry, and every `puna_*` metric family named exactly once.
//!
//! Both binaries import from here rather than declaring their own, because two tiers exporting
//! subtly different names for the same thing is a dashboard that quietly measures nothing. The
//! families are declared before their producers exist so the names are settled in one review
//! rather than accreted.
//!
//! ## Declared in one place, REGISTERED per component
//!
//! Declaring a family here does not mean every process exports it. [`init`] takes a [`Component`]
//! and registers only the families that component actually produces, because a family is
//! registered when its `LazyLock` is first forced and not before.
//!
//! That distinction is the whole reason this is not one `init()`. Registering everything
//! everywhere made `puna-web` and `puna-tracker` export `puna_orchestrator_leader`,
//! `puna_ports_bound` and the rest as permanent zeros -- values they have no way to compute and
//! no business asserting. Nothing was visibly broken while the orchestrator was the only scraped
//! tier, and adding scrapes to the other two turned it into seven series per family where one is
//! meaningful.
//!
//! **The damage is to alerting, and it is the quiet kind.** `sum(puna_orchestrator_leader) != 1`
//! survives extra zeros, so it looked fine. `puna_ports_bound / puna_ports_capacity > 0.8` only
//! survived because the web tier reported `0/0`, which is NaN, which fails the comparison and is
//! dropped -- correct by accident, one plausible refactor away from `+Inf` and a page at 3am.
//! Alert expressions should not have to know which tiers happen to export a zero.
//!
//! ## Adding a family
//!
//! Declare it below, then add its name to exactly one of [`SHARED_FAMILIES`],
//! [`WEB_FAMILIES`], [`TRACKER_FAMILIES`] or [`ORCHESTRATOR_FAMILIES`], and force it in that
//! component's arm of [`init`]. `tests/metrics_scope_*.rs` fail if the table and the registry
//! disagree.
//!
//! The residual risk this does not close: a tier that *touches* another component's family
//! registers it on the spot, since that is what `LazyLock` does. The compile-time split is what
//! actually prevents it -- `puna-core` has no `kube` and no reconcile loop, so there is no code in
//! the web binary that could reach `K8S_REQUESTS` for a reason.

use std::sync::LazyLock;

use std::collections::HashMap;
use std::sync::Mutex;

use prometheus::{
    Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
};

/// The process registry. `/metrics` renders this and nothing else.
pub static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// Which process this is, for the purpose of deciding what it exports.
///
/// `Web` and `Tracker` are the same binary under different `PUNA_ROLE` values, and today they
/// register the same (empty) set beyond the shared families. They are still separate variants:
/// they will diverge -- ingest and upload counters belong to one, proxy and cache counters to the
/// other -- and modelling that now makes the divergence a table edit rather than a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// `puna-web` under `PUNA_ROLE=web`.
    Web,
    /// `puna-web` under `PUNA_ROLE=tracker`.
    Tracker,
    /// `puna-orchestrator`.
    Orchestrator,
}

macro_rules! register {
    ($metric:expr) => {{
        let m = $metric;
        REGISTRY
            .register(Box::new(m.clone()))
            .expect("duplicate metric registration");
        m
    }};
}

/// Rooms by observed state. The shape of the fleet at a glance.
pub static ROOMS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register!(
        IntGaugeVec::new(
            Opts::new("puna_rooms", "Rooms by observed state"),
            &["state"]
        )
        .unwrap()
    )
});

/// Start attempts by outcome. `port_exhausted` and `ip_mismatch` are first-class outcomes, not
/// errors: the first is a capacity signal, the second is the silent-Cilium failure being caught.
pub static ROOM_STARTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new("puna_room_starts_total", "Room start attempts by outcome"),
            &["result"],
        )
        .unwrap()
    )
});

/// Desired-to-running latency. The cold-start number a player actually experiences.
pub static ROOM_START_SECONDS: LazyLock<Histogram> = LazyLock::new(|| {
    register!(
        Histogram::with_opts(
            HistogramOpts::new(
                "puna_room_start_seconds",
                "Seconds from desired=running to a ready replica",
            )
            .buckets(vec![1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0]),
        )
        .unwrap()
    )
});

/// The size of this environment's range.
///
/// **`puna_ports_capacity`, not `puna_ports_total`**, which is what the design sketched: `_total` is
/// reserved for counters, and this is a capacity that moves only when the range is resized. The
/// suffix would have promised `rate()` a number that never accumulates.
pub static PORTS_CAPACITY: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(
        IntGauge::new(
            "puna_ports_capacity",
            "Port pairs in this environment's range"
        )
        .unwrap()
    )
});

pub static PORTS_BOUND: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(IntGauge::new("puna_ports_bound", "Port pairs reserved to a room").unwrap())
});

pub static PORTS_QUARANTINED: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(
        IntGauge::new(
            "puna_ports_quarantined",
            "Port pairs held out after an IP mismatch"
        )
        .unwrap()
    )
});

/// LRU reclaims. Each one invalidates the address embedded in an already-downloaded patch, so a
/// rising rate is a capacity problem players can feel.
///
/// A **counter**, not a gauge, and the `_total` suffix is the reason: it promises monotonicity to
/// anything that wraps this in `rate()`, and a gauge that only happens to increase is a promise the
/// type does not keep.
pub static PORT_RECLAIMS: LazyLock<IntCounter> = LazyLock::new(|| {
    register!(IntCounter::new("puna_port_reclaims_total", "Port pairs reclaimed via LRU").unwrap())
});

/// Services that came up on an address other than the expected shared VIP. Should stay zero;
/// non-zero means something outside Puna took a port on the sharing key.
pub static PORT_IP_MISMATCH: LazyLock<IntCounter> = LazyLock::new(|| {
    register!(
        IntCounter::new(
            "puna_port_ip_mismatch_total",
            "Room Services observed on an unexpected address",
        )
        .unwrap()
    )
});

/// Indexed generations, and the bytes they occupy.
///
/// Both from the slow lane rather than every tick: they are aggregates over a table that changes
/// when somebody uploads, which is not thirty-second news. The bytes are the number to watch
/// alongside the PVC alert — generations are content-addressed and shared, so this grows with
/// distinct seeds rather than with rooms.
pub static GENERATIONS: LazyLock<IntGauge> =
    LazyLock::new(|| register!(IntGauge::new("puna_generations", "Indexed generations").unwrap()));

pub static GENERATION_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(
        IntGauge::new(
            "puna_generation_bytes",
            "Total size of indexed generation archives"
        )
        .unwrap()
    )
});

/// Slots nobody has claimed, across every room that is not deleted.
///
/// A standing number rather than an alert: it is how much of the fleet is waiting on people to
/// follow their claim links, which is an organizing problem rather than a fault.
pub static SLOTS_UNCLAIMED: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(IntGauge::new("puna_slots_unclaimed", "Room slots with no owner").unwrap())
});

/// **Alert on `!= 1` for 5m.** Zero means nothing is reconciling; more than one means the leader
/// lock is not doing its job.
pub static ORCHESTRATOR_LEADER: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(
        IntGauge::new(
            "puna_orchestrator_leader",
            "1 if this process holds the leader advisory lock",
        )
        .unwrap()
    )
});

/// **Alert on `> 0` immediately.** A room whose row claims on-disk state that is not there. Never
/// auto-repaired, because silently recreating it would present an empty room as a real one.
pub static INTEGRITY_FAULTS: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(
        IntGauge::new(
            "puna_integrity_faults",
            "Rooms whose state directory is missing"
        )
        .unwrap()
    )
});

/// Directories with no row. Reported, never deleted automatically.
pub static ORPHAN_DIRECTORIES: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(
        IntGauge::new(
            "puna_orphan_directories",
            "State directories with no room row"
        )
        .unwrap()
    )
});

pub static RECONCILE_SECONDS: LazyLock<Histogram> = LazyLock::new(|| {
    register!(
        Histogram::with_opts(
            HistogramOpts::new("puna_reconcile_seconds", "Duration of one reconcile tick")
                .buckets(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 15.0, 30.0]),
        )
        .unwrap()
    )
});

pub static RECONCILE_ERRORS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new("puna_reconcile_errors_total", "Reconcile failures by stage"),
            &["stage"],
        )
        .unwrap()
    )
});

pub static K8S_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new("puna_k8s_requests_total", "Kubernetes API calls"),
            &["verb", "resource", "result"],
        )
        .unwrap()
    )
});

pub static COMMANDS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_commands_total",
                "Console commands by kind and outcome"
            ),
            &["command", "result"],
        )
        .unwrap()
    )
});

pub static COMMAND_SECONDS: LazyLock<Histogram> = LazyLock::new(|| {
    register!(
        Histogram::with_opts(
            HistogramOpts::new("puna_command_seconds", "Console command round-trip")
                .buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 15.0]),
        )
        .unwrap()
    )
});

// --- per-room, re-exported from the probe ---------------------------------------------------------
//
// **Polled by the orchestrator and cached here, never scraped from a room.** A Prometheus scrape
// reads these values and never touches a live multiworld, which decouples scrape rate from room
// load: a monitoring change cannot add work to a game in progress. The rejected alternative — a
// ServiceMonitor per room — is argued in `puna-orchestrator/src/probing.rs`.
//
// Every one is labeled by room, which makes them the only families here with an **unbounded** label
// space. That is what `retain_rooms` exists for.

macro_rules! room_gauge {
    ($name:literal, $help:literal) => {
        LazyLock::new(|| register!(IntGaugeVec::new(Opts::new($name, $help), &["room"]).unwrap()))
    };
}

/// **Sockets, not players.** One player commonly holds three — game client, text client, tracker —
/// so a dashboard that labels this "players online" is wrong, and an idle reaper built on it would
/// never reap a room full of abandoned tracker tabs. `puna_room_idle_seconds` is the reaper's
/// number.
pub static ROOM_CLIENTS: LazyLock<IntGaugeVec> = room_gauge!(
    "puna_room_clients_connected",
    "Open client sockets per room, which is not a player count"
);

pub static ROOM_MAILBOX_DEPTH: LazyLock<IntGaugeVec> = room_gauge!(
    "puna_room_mailbox_depth",
    "Queued messages in a room's actor mailbox"
);

pub static ROOM_OUTBOUND_QUEUED_BYTES: LazyLock<IntGaugeVec> = room_gauge!(
    "puna_room_outbound_queued_bytes",
    "Bytes queued for delivery to a room's clients"
);

/// What turns §7's `slots * 3 * 96KiB` memory request from a heuristic into a measurement — but
/// only after a week of it, not from one reading.
pub static ROOM_RESIDENT_BYTES: LazyLock<IntGaugeVec> =
    room_gauge!("puna_room_resident_bytes", "A room's resident set size");

/// Seconds since any client last spoke. **The number an idle reaper reads**, and `null` until a
/// client has spoken at all — which is absent here rather than zero.
pub static ROOM_IDLE_SECONDS: LazyLock<IntGaugeVec> = room_gauge!(
    "puna_room_idle_seconds",
    "Seconds since a room last heard from any client"
);

/// Clients dropped for falling too far behind.
///
/// **A counter fed by deltas, and that shape is forced.** pahoa reports a cumulative total, but
/// `prometheus`'s `IntCounter` exposes only `inc`/`inc_by` — there is no `set`. Storing the total in
/// a gauge would fail M9's naming invariant both ways round: `..._total` on a gauge, or a counter's
/// semantics behind a gauge's name. So [`publish_room`] keeps the last polled value and advances by
/// the difference.
pub static ROOM_LAG_DISCONNECTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_room_lag_disconnects_total",
                "Clients disconnected for lagging, per room"
            ),
            &["room"],
        )
        .unwrap()
    )
});

/// Rooms with series published, and the last cumulative counter value seen for each.
///
/// Two jobs in one map because they have the same lifetime: a room leaves both at the same moment.
static ROOM_SERIES: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Publish one room's numbers.
///
/// **`None` removes the series rather than publishing a zero.** That is the same rule the database
/// columns follow: a probe that cannot tell must not be indistinguishable from a room with nobody
/// in it, and on a dashboard a zero is worse than a gap because it looks like a reading.
pub fn publish_room(room: &str, status: &crate::probe::RoomStatus) {
    fn set(vec: &IntGaugeVec, room: &str, value: Option<i64>) {
        match value {
            Some(v) => vec.with_label_values(&[room]).set(v),
            None => {
                let _ = vec.remove_label_values(&[room]);
            }
        }
    }

    set(&ROOM_CLIENTS, room, status.net.clients_connected);
    set(&ROOM_MAILBOX_DEPTH, room, status.net.mailbox_depth);
    set(
        &ROOM_OUTBOUND_QUEUED_BYTES,
        room,
        status.net.outbound_queued_bytes,
    );
    set(&ROOM_RESIDENT_BYTES, room, status.net.resident_bytes);
    set(&ROOM_IDLE_SECONDS, room, status.activity.idle_seconds);

    let Ok(mut series) = ROOM_SERIES.lock() else {
        return;
    };
    let previous = series.get(room).copied();
    series.insert(room.to_string(), status.net.lag_disconnects.unwrap_or(0));

    if let Some(total) = status.net.lag_disconnects {
        // **`total < previous` means the ROOM restarted** and began its counters again at zero, so
        // the delta is the new total rather than a negative. On the first sighting the whole total
        // is added: the room may have been running before this orchestrator was, and the process's
        // own counter started at zero, so this is what makes the series mean what its name says.
        let delta = match previous {
            Some(previous) if total >= previous => total - previous,
            _ => total,
        };
        if delta > 0 {
            ROOM_LAG_DISCONNECTS
                .with_label_values(&[room])
                .inc_by(delta as u64);
        }
    }
}

/// Drop every series for a room that is no longer live.
///
/// **The trap this exists for**: a `GaugeVec` keyed by room id keeps a series forever unless it is
/// removed, so without this every room that has ever run would leave behind a series asserting its
/// last-known client count. A stale gauge reads as a live room, which is worse than no metric.
///
/// Level-triggered on purpose — it reconciles the published set against the live set rather than
/// hooking each transition. There are several ways a room stops being live (stopped, deleted,
/// failed, vanished) and a hook per path is a hook somebody forgets.
pub fn retain_rooms(live: &std::collections::HashSet<String>) {
    let Ok(mut series) = ROOM_SERIES.lock() else {
        return;
    };

    series.retain(|room, _| {
        if live.contains(room) {
            return true;
        }
        for vec in [
            &*ROOM_CLIENTS,
            &*ROOM_MAILBOX_DEPTH,
            &*ROOM_OUTBOUND_QUEUED_BYTES,
            &*ROOM_RESIDENT_BYTES,
            &*ROOM_IDLE_SECONDS,
        ] {
            let _ = vec.remove_label_values(&[room]);
        }
        let _ = ROOM_LAG_DISCONNECTS.remove_label_values(&[room]);
        false
    });
}

/// Which capabilities the room probe currently has. Makes a room stuck on an old pahoa image
/// visible on a dashboard instead of surfacing as features that quietly do nothing.
pub static PROBE_CAPABILITY: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register!(
        IntGaugeVec::new(
            Opts::new(
                "puna_probe_capability",
                "1 if the room probe supports a capability"
            ),
            &["capability"],
        )
        .unwrap()
    )
});

/// Every value of the `room_state` enum in migration 0001.
///
/// Duplicated from SQL on purpose so the gauge can publish a zero for each state at startup.
/// Kept honest by `rooms_states_match_the_database` in the Postgres-backed suite, which reads
/// `pg_enum` and compares -- adding a state to the migration without adding it here fails there.
pub const ROOM_STATES: &[&str] = &[
    "provisioning",
    "idle",
    "starting",
    "running",
    "degraded",
    "stopping",
    "failed",
    "deleting",
    "integrity_fault",
];

/// Outcomes of a start attempt.
pub const START_RESULTS: &[&str] = &["ok", "failed", "port_exhausted", "ip_mismatch"];

/// Capabilities a room probe may or may not have, depending on the pahoa build it is talking to.
pub const PROBE_CAPABILITIES: &[&str] =
    &["activity", "client_count", "graceful_shutdown", "commands"];

/// Families every component exports, because every component does the thing they measure.
///
/// Only database timing so far, and it belongs here for a concrete reason rather than by default:
/// all three tiers hold a diesel pool, so a query-latency series scoped to one of them would
/// answer "is Postgres slow" for a third of the traffic.
pub const SHARED_FAMILIES: &[&str] = &["diesel_query_seconds"];

/// Families only `puna-web` exports. Empty today.
///
/// The HTTP request fairing (§11) lands here when it is built, and this is the list it goes in.
pub const WEB_FAMILIES: &[&str] = &[];

/// Families only `puna-tracker` exports. Empty today.
///
/// Upstream fetch counts and cache hit rates belong here -- the numbers that say whether the
/// three cache layers are doing their job -- when there is something to attach them to.
pub const TRACKER_FAMILIES: &[&str] = &[];

/// Families only `puna-orchestrator` exports.
///
/// Everything about rooms, ports, reconciliation and the cluster, which is nearly the whole
/// registry: these are all computed by the reconcile loop or by the sweep, and no other process
/// has the inputs to compute any of them. `puna_commands_total`, `puna_command_seconds` and
/// `puna_probe_capability` have no producer yet (M11 and M12) and are listed here because that is
/// where their producer will be -- the dispatcher and the probe are orchestrator-side.
pub const ORCHESTRATOR_FAMILIES: &[&str] = &[
    "puna_rooms",
    "puna_room_starts_total",
    "puna_room_start_seconds",
    "puna_ports_capacity",
    "puna_ports_bound",
    "puna_ports_quarantined",
    "puna_port_reclaims_total",
    "puna_port_ip_mismatch_total",
    "puna_generations",
    "puna_generation_bytes",
    "puna_slots_unclaimed",
    "puna_orchestrator_leader",
    "puna_integrity_faults",
    "puna_orphan_directories",
    "puna_reconcile_seconds",
    "puna_reconcile_errors_total",
    "puna_k8s_requests_total",
    "puna_commands_total",
    "puna_command_seconds",
    "puna_probe_capability",
    // Re-exported from the probe, labeled by room. See `publish_room`.
    "puna_room_clients_connected",
    "puna_room_mailbox_depth",
    "puna_room_outbound_queued_bytes",
    "puna_room_resident_bytes",
    "puna_room_idle_seconds",
    "puna_room_lag_disconnects_total",
];

/// Families that are REGISTERED but do not appear until something writes a series.
///
/// An orthogonal axis to the per-component tables, and a real one: a labeled family renders no
/// `# TYPE` line at all while it has no children, so "registered" and "visible in `/metrics`" are
/// different sets. Every name here is a `*Vec` whose label space is combinatorial and mostly
/// uninteresting -- pre-seeding them would trade one confusion for a wall of permanent zeros, so
/// [`init`] deliberately leaves them empty.
///
/// `diesel_query_seconds` is the one that is not a choice: it is labeled by query, so it cannot be
/// seeded without inventing a query name. It shows up as soon as the process talks to Postgres,
/// which for the web tiers is the readiness probe.
///
/// The distinction is worth encoding because it decides what a dashboard sees on a cold process,
/// and because moving a family across it is a decision rather than an accident -- the scope tests
/// fail either way round.
pub const DEFERRED_FAMILIES: &[&str] = &[
    "diesel_query_seconds",
    "puna_commands_total",
    "puna_k8s_requests_total",
    "puna_reconcile_errors_total",
    // Labeled by room, so they appear only once a room has been probed -- and disappear again when
    // it stops, which is what `retain_rooms` is for.
    "puna_room_clients_connected",
    "puna_room_mailbox_depth",
    "puna_room_outbound_queued_bytes",
    "puna_room_resident_bytes",
    "puna_room_idle_seconds",
    "puna_room_lag_disconnects_total",
];

/// The families `component` registers, shared ones included.
///
/// Used by the scope tests, and by anyone writing an alert who needs to know which job a series
/// can legitimately come from.
pub fn families(component: Component) -> Vec<&'static str> {
    let own = match component {
        Component::Web => WEB_FAMILIES,
        Component::Tracker => TRACKER_FAMILIES,
        Component::Orchestrator => ORCHESTRATOR_FAMILIES,
    };
    SHARED_FAMILIES.iter().chain(own).copied().collect()
}

/// The families `component` renders on a freshly started process, before anything has happened.
///
/// [`families`] minus [`DEFERRED_FAMILIES`]. This is what a scrape of a cold pod returns, and
/// therefore what a panel shows before the first room starts.
pub fn seeded_families(component: Component) -> Vec<&'static str> {
    families(component)
        .into_iter()
        .filter(|name| !DEFERRED_FAMILIES.contains(name))
        .collect()
}

/// Register the families `component` produces, and pre-instantiate the label sets that are finite
/// and known.
///
/// The pre-instantiation is the point. A labeled family emits NOTHING until some label
/// combination is touched, so a freshly started process would export no `puna_integrity_faults`
/// series at all -- and a panel reading "no data" is ambiguous in exactly the case where 0 is the
/// reassuring answer. Unlabeled gauges and histograms appear as soon as they are registered, so
/// only the `*Vec` families need this.
///
/// Combinatorial labels (`puna_k8s_requests_total`, `puna_reconcile_errors_total`,
/// `puna_commands_total`) are deliberately left to appear on first use: their label spaces are
/// large and mostly uninteresting, and pre-seeding them would trade one confusion for a wall of
/// permanent zeros.
///
/// **A zero published by the wrong process is worse than a missing series**, which is why this
/// takes a component rather than seeding everything. An absent series is visibly absent; a zero
/// from a tier that cannot compute the value looks like an answer. See the module docs.
pub fn init(component: Component) {
    // Every tier holds a diesel pool. Tolerates re-registration: `init` runs once per process in
    // production but several times across a test binary sharing one registry.
    let _ = REGISTRY.register(Box::new(crate::db::QUERY_HISTOGRAM.clone()));

    match component {
        // Nothing yet beyond the shared families. When the request fairing lands, force it here
        // and add it to WEB_FAMILIES -- the scope test fails if only one of those happens.
        Component::Web => {}
        Component::Tracker => {}
        Component::Orchestrator => init_orchestrator(),
    }
}

/// Everything the reconcile loop and the sweep produce.
fn init_orchestrator() {
    // Labeled by room: registered here, empty until a probe writes one. See `DEFERRED_FAMILIES`.
    LazyLock::force(&ROOM_CLIENTS);
    LazyLock::force(&ROOM_MAILBOX_DEPTH);
    LazyLock::force(&ROOM_OUTBOUND_QUEUED_BYTES);
    LazyLock::force(&ROOM_RESIDENT_BYTES);
    LazyLock::force(&ROOM_IDLE_SECONDS);
    LazyLock::force(&ROOM_LAG_DISCONNECTS);
    LazyLock::force(&ROOM_START_SECONDS);
    LazyLock::force(&PORTS_CAPACITY);
    LazyLock::force(&PORTS_BOUND);
    LazyLock::force(&PORTS_QUARANTINED);
    LazyLock::force(&PORT_RECLAIMS);
    LazyLock::force(&PORT_IP_MISMATCH);
    LazyLock::force(&ORCHESTRATOR_LEADER);
    LazyLock::force(&INTEGRITY_FAULTS);
    LazyLock::force(&ORPHAN_DIRECTORIES);
    LazyLock::force(&GENERATIONS);
    LazyLock::force(&GENERATION_BYTES);
    LazyLock::force(&SLOTS_UNCLAIMED);
    LazyLock::force(&RECONCILE_SECONDS);
    LazyLock::force(&RECONCILE_ERRORS);
    LazyLock::force(&K8S_REQUESTS);
    LazyLock::force(&COMMANDS);
    LazyLock::force(&COMMAND_SECONDS);

    for state in ROOM_STATES {
        ROOMS.with_label_values(&[state]).set(0);
    }
    for result in START_RESULTS {
        ROOM_STARTS.with_label_values(&[result]).reset();
    }
    for capability in PROBE_CAPABILITIES {
        PROBE_CAPABILITY.with_label_values(&[capability]).set(0);
    }
}

/// Render the registry in Prometheus text exposition format.
/// Publish whether this process holds the leader lock.
///
/// A setter rather than direct access to the gauge, so the "alert on `sum(...) != 1`" rule has one
/// place that can write it and a parked replica cannot forget to publish its zero.
pub fn set_leader(leading: bool) {
    ORCHESTRATOR_LEADER.set(i64::from(leading));
}

pub fn gather() -> String {
    use prometheus::Encoder;
    let mut buf = Vec::new();
    let encoder = prometheus::TextEncoder::new();
    encoder
        .encode(&REGISTRY.gather(), &mut buf)
        .expect("failed to encode metrics");
    String::from_utf8(buf).expect("metrics encoding produced invalid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`_total` promises monotonicity**, and a gauge that only happens to increase does not keep
    /// that promise: anything wrapping it in `rate()` is reading a number that may legitimately go
    /// down. Two families were declared as gauges with counter names until this test existed.
    #[test]
    fn every_total_is_a_counter_and_no_counter_is_missing_the_suffix() {
        // Every component, so the invariant covers the whole registry rather than one tier's
        // slice. The registry is cumulative within a test binary, so this is their union.
        for component in [
            super::Component::Web,
            super::Component::Tracker,
            super::Component::Orchestrator,
        ] {
            super::init(component);
        }
        let text = super::gather();

        // `# TYPE <name> <type>` lines, which is the exposition format's own answer.
        let types: Vec<(&str, &str)> = text
            .lines()
            .filter_map(|line| line.strip_prefix("# TYPE "))
            .filter_map(|rest| rest.split_once(' '))
            .filter(|(name, _)| name.starts_with("puna_"))
            .collect();
        assert!(!types.is_empty(), "no puna_* families were rendered");

        for (name, kind) in types {
            if name.ends_with("_total") {
                assert_eq!(
                    kind, "counter",
                    "{name} is named like a counter but is a {kind}"
                );
            }
            if kind == "counter" {
                assert!(
                    name.ends_with("_total"),
                    "{name} is a counter without the suffix"
                );
            }
        }
    }

    #[test]
    fn init_is_idempotent_and_publishes_the_known_series() {
        // Called twice on purpose: the registry rejects duplicate names, so this catches both a
        // collision between two families and an init that cannot be run more than once.
        super::init(super::Component::Orchestrator);
        super::init(super::Component::Orchestrator);
        let text = super::gather();

        // Unlabeled families appear on registration alone.
        for expected in [
            "puna_ports_capacity",
            "puna_orchestrator_leader",
            "puna_integrity_faults",
            "puna_room_start_seconds",
        ] {
            assert!(text.contains(expected), "{expected} missing from /metrics");
        }

        // Labeled families appear only once a series exists, which is what init pre-seeds. Assert
        // on a full series rather than the family name, since the name alone would also match a
        // bare HELP line and prove nothing.
        for state in super::ROOM_STATES {
            let series = format!("puna_rooms{{state=\"{state}\"}} 0");
            assert!(text.contains(&series), "missing series: {series}");
        }
        for capability in super::PROBE_CAPABILITIES {
            let series = format!("puna_probe_capability{{capability=\"{capability}\"}} 0");
            assert!(text.contains(&series), "missing series: {series}");
        }
        for result in super::START_RESULTS {
            let series = format!("puna_room_starts_total{{result=\"{result}\"}} 0");
            assert!(text.contains(&series), "missing series: {series}");
        }
    }

    // --- the two traps in the room re-export ------------------------------------------------------

    use crate::probe::{ActivityStatus, NetStatus, RoomStatus};

    /// The room series are PROCESS-GLOBAL, and `retain_rooms` is a whole-fleet reconcile -- so a
    /// test that ends by clearing the fleet deletes the series of any test running beside it.
    /// Cargo runs these on parallel threads, so they take turns.
    ///
    /// Found the honest way: the suite passed single-threaded and failed at random otherwise.
    static SERIALIZE: Mutex<()> = Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        // A panicking test poisons the lock; the state it guards is rebuilt by the next test
        // anyway, so recovering beats turning one failure into four.
        SERIALIZE.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn status(clients: Option<i64>, idle: Option<i64>, lag: Option<i64>) -> RoomStatus {
        RoomStatus {
            net: NetStatus {
                clients_connected: clients,
                lag_disconnects: lag,
                ..Default::default()
            },
            activity: ActivityStatus {
                idle_seconds: idle,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The published value for a room, or `None` if it has no series at all.
    ///
    /// Read from the COLLECTED family rather than `get_metric_with_label_values`, which creates the
    /// child if it is missing and so can never answer "is there a series" -- the exact question
    /// both traps turn on.
    fn gauge(room: &str) -> Option<i64> {
        prometheus::core::Collector::collect(&*ROOM_CLIENTS)
            .iter()
            .flat_map(prometheus::proto::MetricFamily::get_metric)
            .find(|m| {
                m.get_label()
                    .iter()
                    .any(|l| l.name() == "room" && l.value() == room)
            })
            .map(|m| m.get_gauge().get_value() as i64)
    }

    /// **Trap one: a stale series reads as a live room.** A `GaugeVec` keyed by room keeps its
    /// children forever, so a room that has stopped would otherwise keep asserting its last client
    /// count -- indefinitely, and indistinguishably from a room that really has three people in it.
    #[test]
    fn a_room_that_stops_being_live_loses_its_series() {
        let _guard = exclusive();
        let room = "11111111-1111-1111-1111-111111111111";
        publish_room(room, &status(Some(3), Some(60), Some(0)));
        assert_eq!(gauge(room), Some(3));

        // Still live: kept.
        retain_rooms(&std::collections::HashSet::from([room.to_string()]));
        assert_eq!(gauge(room), Some(3));

        // Gone: dropped, rather than left asserting three players forever.
        retain_rooms(&std::collections::HashSet::new());
        assert_eq!(gauge(room), None, "a stale gauge reads as a live room");
    }

    /// A probe that cannot tell removes the series rather than publishing a zero -- the same
    /// null-is-not-zero rule the database columns follow. On a dashboard a zero is worse than a
    /// gap, because a gap looks like missing data and a zero looks like a reading.
    #[test]
    fn a_value_the_probe_cannot_supply_is_absent_rather_than_zero() {
        let _guard = exclusive();
        let room = "22222222-2222-2222-2222-222222222222";
        publish_room(room, &status(Some(2), None, None));
        assert_eq!(gauge(room), Some(2));

        // The TCP fallback: reachable, nothing known.
        publish_room(room, &status(None, None, None));
        assert_eq!(gauge(room), None, "not 0");

        retain_rooms(&std::collections::HashSet::new());
    }

    /// **Trap two: a cumulative counter fed into `inc_by`.** pahoa reports a total and
    /// `IntCounter` has no `set`, so this advances by the difference -- and a room restart, where
    /// the total goes BACKWARDS, must add the new total rather than underflow or stall.
    #[test]
    fn a_cumulative_counter_advances_by_its_delta_and_survives_a_room_restart() {
        let _guard = exclusive();
        let room = "33333333-3333-3333-3333-333333333333";
        let total = || {
            ROOM_LAG_DISCONNECTS
                .get_metric_with_label_values(&[room])
                .expect("the child")
                .get()
        };

        // First sighting adds the whole total: the room may have been running before this process
        // was, and this process's own counter started at zero.
        publish_room(room, &status(Some(1), None, Some(7)));
        assert_eq!(total(), 7);

        // Then deltas.
        publish_room(room, &status(Some(1), None, Some(9)));
        assert_eq!(total(), 9);

        // Unchanged adds nothing.
        publish_room(room, &status(Some(1), None, Some(9)));
        assert_eq!(total(), 9);

        // **The room restarted**: its total began again at 2. The counter must move forward by 2,
        // never backwards and never by a negative that would panic or wrap.
        publish_room(room, &status(Some(1), None, Some(2)));
        assert_eq!(total(), 11, "a room restart must not stall or underflow");

        retain_rooms(&std::collections::HashSet::new());
    }

    /// The baseline is dropped with the series, so a room that comes back is not credited with the
    /// difference against a total it no longer has.
    #[test]
    fn a_removed_rooms_counter_baseline_goes_with_it() {
        let _guard = exclusive();
        let room = "44444444-4444-4444-4444-444444444444";
        publish_room(room, &status(Some(1), None, Some(100)));
        retain_rooms(&std::collections::HashSet::new());

        // Fresh series, fresh baseline: the room's current total, not a delta against 100.
        publish_room(room, &status(Some(1), None, Some(5)));
        assert_eq!(
            ROOM_LAG_DISCONNECTS
                .get_metric_with_label_values(&[room])
                .expect("the child")
                .get(),
            5
        );

        retain_rooms(&std::collections::HashSet::new());
    }
}
