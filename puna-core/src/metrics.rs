//! One registry, and every `puna_*` metric family named exactly once.
//!
//! Both binaries import from here rather than declaring their own, because two tiers exporting
//! subtly different names for the same thing is a dashboard that quietly measures nothing. The
//! families are declared before their producers exist so the names are settled in one review
//! rather than accreted.

use std::sync::LazyLock;

use prometheus::{
    Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
};

/// The process registry. `/metrics` renders this and nothing else.
pub static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

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

/// Register every family, and pre-instantiate the label sets that are finite and known.
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
pub fn init() {
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

    // Tolerate re-registration: `init` is called once per process in production, but several
    // times across a test binary sharing one registry.
    let _ = REGISTRY.register(Box::new(crate::db::QUERY_HISTOGRAM.clone()));
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
    /// **`_total` promises monotonicity**, and a gauge that only happens to increase does not keep
    /// that promise: anything wrapping it in `rate()` is reading a number that may legitimately go
    /// down. Two families were declared as gauges with counter names until this test existed.
    #[test]
    fn every_total_is_a_counter_and_no_counter_is_missing_the_suffix() {
        super::init();
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
        super::init();
        super::init();
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
}
