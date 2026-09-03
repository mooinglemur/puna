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
//! `puna_ports_bound` and the rest as permanent zeros: values they have no way to compute and
//! no business asserting. Nothing was visibly broken while the orchestrator was the only scraped
//! tier, and adding scrapes to the other two turned it into seven series per family where one is
//! meaningful.
//!
//! **The damage is to alerting, and it is the quiet kind.** `sum(puna_orchestrator_leader) != 1`
//! survives extra zeros, so it looked fine. `puna_ports_bound / puna_ports_capacity > 0.8` only
//! survived because the web tier reported `0/0`, which is NaN, which fails the comparison and is
//! dropped: correct by accident, one plausible refactor away from `+Inf` and a page at 3am.
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
//! actually prevents it: `puna-core` has no `kube` and no reconcile loop, so there is no code in
//! the web binary that could reach `K8S_REQUESTS` for a reason.

pub mod proxy;

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
/// they will diverge (ingest and upload counters belong to one, proxy and cache counters to the
/// other) and modelling that now makes the divergence a table edit rather than a refactor.
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

// --- HTTP, as the two web roles serve it --------------------------------------------------------
//
// **`kind` is a closed vocabulary and `room` is a resolved id, and both of those are cardinality
// decisions rather than style.** A label taken verbatim from a request line lets anybody who can
// reach the port mint series until the scrape falls over, which is a public listener's version of a
// memory leak. So `kind` comes from a `match` over the first path segments whose arms ARE the
// vocabulary below, and `room` is either an id a handler resolved or one the response proved real
// by answering it: see `puna-web`'s `http_metrics`.
//
// Three families and no `status`, no `method` and no per-route label. pahoa's `pahoa_http_*` carries
// a templated route because a room serves one small closed surface; Puna serves dozens of routes and
// the question these answer is "which part of Puna, for which room", which is the granularity a
// capacity or abuse question is actually asked at.

/// What part of Puna a request was for. The `kind` label's whole domain.
///
/// `static` and `health` are here rather than folded into `other` because both are high-volume and
/// neither is application traffic: the kubelet polls readiness every few seconds forever, and asset
/// requests outnumber page requests several to one. Left in `other`, they would be nearly all of it,
/// and "everything else" would answer nothing.
pub const HTTP_KINDS: &[&str] = &[
    "generations",
    "room",
    "journal",
    "tracker",
    "static",
    "health",
    "other",
];

/// The kinds `PUNA_ROLE=web` can serve, which is every kind except the tracker's.
///
/// Seeded per role rather than seeding [`HTTP_KINDS`] everywhere, for the reason the module docs
/// give: a zero published by a process that cannot serve the thing looks like an answer. The
/// tracker routes are mounted only under the tracker role, so a `kind="tracker"` zero on the web
/// tier would say that nobody is using the tracker.
pub const WEB_HTTP_KINDS: &[&str] = &[
    "generations",
    "room",
    "journal",
    "static",
    "health",
    "other",
];

/// The kinds `PUNA_ROLE=tracker` can serve. It mounts the tracker routes, the shared assets and the
/// two probes, and nothing else.
pub const TRACKER_HTTP_KINDS: &[&str] = &["tracker", "static", "health", "other"];

/// Requests served, by part of Puna and by room.
pub static HTTP_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_http_requests_total",
                "HTTP requests served, by part of Puna and room"
            ),
            &["kind", "room"],
        )
        .unwrap()
    )
});

/// Request body bytes, from `Content-Length`.
///
/// **What the client said it was sending, not what was read.** A request whose body is refused
/// (over the upload limit) still counts what it announced, which is the honest answer for a
/// capacity question and the wrong one for a "what did we accept" question. A chunked request
/// announces nothing and counts zero; nothing Puna serves is uploaded that way.
pub static HTTP_REQUEST_BYTES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_http_request_bytes_total",
                "HTTP request body bytes received, by part of Puna and room"
            ),
            &["kind", "room"],
        )
        .unwrap()
    )
});

/// Response body bytes, counted as they leave.
///
/// A sized body is counted from its own length; a streamed one is counted through a wrapper, so the
/// two responses that matter most here (a patch and a gzipped journal, both streamed rather than
/// buffered) are not the two this cannot see. **The journal's WebSocket frames are counted here
/// too**, under `kind="journal"`: they are the feed's actual traffic, and a number that covered the
/// page but not the socket would be wrong by orders of magnitude on exactly the room somebody is
/// asking about.
pub static HTTP_RESPONSE_BYTES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_http_response_bytes_total",
                "HTTP response body bytes sent, by part of Puna and room"
            ),
            &["kind", "room"],
        )
        .unwrap()
    )
});

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

/// Allocations Cilium refused outright, because the port was already held on the shared address.
///
/// **The rate is the whole signal, and it is deliberately left to an alert to interpret.** One
/// external Service holding one port produces a single increment, because the room quarantines that
/// pair and succeeds on the next one. A range misconfiguration (two environments drawing from one
/// span) produces a sustained rate instead, as rooms walk pair after pair. Puna does not try to
/// tell those apart: it keeps looking for a working port either way and states what happened, and
/// where the line between "bad luck" and "somebody mis-set `PUNA_PORT_RANGE`" falls is a threshold
/// an operator can tune without a release.
///
/// **Port collisions only.** A Service can be refused an address for reasons that have nothing to do
/// with the port (no pool holds the configured IP, the `lb-pool` label is missing, the sharing key
/// disagrees) and those are properties of the Service template, identical for every room in the
/// environment. They are counted as `puna_room_starts_total{result="address_unsatisfiable"}` and
/// **do not appear here**, which is what keeps this family, and the quarantine gauge beside it,
/// meaning what their names say.
///
/// `conflict` separates the two cases worth acting on differently. `external` is somebody else's
/// Service and is operations; `internal` means the port is held by a Service Puna itself manages,
/// which is a Puna bug (a leaked object the sweep should have collected) and deserves its own
/// alert rather than being averaged into the same number.
///
/// Read alongside [`PORTS_QUARANTINED`], which is what shows the *fleet* cost: a sustained refusal
/// rate drives that gauge up, and a range overlap is visible there as quarantine climbing without
/// rooms coming up.
pub static PORT_REFUSALS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_port_refusals_total",
                "Port pairs Cilium refused to allocate, by whose Service holds the port"
            ),
            &["conflict"],
        )
        .unwrap()
    )
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
/// alongside the PVC alert: generations are content-addressed and shared, so this grows with
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

/// Ticks by kind, which is how the loop's cadence becomes observable.
///
/// **A counter rather than a "current mode" gauge**, and the reason is sampling: convergence runs in
/// bursts a few seconds long, so a gauge read every scrape interval would miss almost all of them
/// and report whatever happened to be true at the instant of the scrape. A counter loses nothing:
/// `rate(...{kind="converge"}[5m])` is exactly "how much convergence are we doing", and
/// `rate(...{kind="reconcile"}[5m])` should sit at `1/PUNA_RECONCILE_INTERVAL` whenever the loop is
/// healthy, which makes the starvation failure visible as a number rather than as an absence.
pub static RECONCILE_TICKS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new("puna_reconcile_ticks_total", "Reconcile passes by kind"),
            &["kind"],
        )
        .unwrap()
    )
});

/// Rooms the loop is waiting on, which is what decides the cadence.
///
/// The instant-state gauge worth having, in place of a tick-mode one: it is meaningful at any
/// sample point, and it shows a case `puna_rooms{state}` structurally cannot: **a room sitting in
/// `idle` while its previous Deployment drains**. That room's state says `idle`, which reads as
/// resting, when it is in the middle of a restart.
pub static ROOMS_CONVERGING: LazyLock<IntGauge> = LazyLock::new(|| {
    register!(
        IntGauge::new(
            "puna_rooms_converging",
            "Rooms mid-transition, which is what keeps the loop on its short cadence",
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
// load: a monitoring change cannot add work to a game in progress. The rejected alternative, a
// ServiceMonitor per room, is argued in `puna-orchestrator/src/probing.rs`.
//
// Every one is labeled by room, which makes them the only families here with an **unbounded** label
// space. That is what `retain_rooms` exists for.

macro_rules! room_gauge {
    ($name:literal, $help:literal) => {
        LazyLock::new(|| register!(IntGaugeVec::new(Opts::new($name, $help), &["room"]).unwrap()))
    };
}

/// **Sockets, not players.** One player commonly holds three (game client, text client, tracker)
/// so a dashboard that labels this "players online" is wrong, and an idle reaper built on it would
/// never reap a room full of abandoned tracker tabs.
///
/// **The reaper reads neither this nor `puna_room_idle_seconds`**, and that correction is the whole
/// point of pahoa's P23: it reaps on `rooms.last_check_at`, the time a slot last registered a new
/// location check, because both of these stay fresh in a room where everybody is talking and nobody
/// is playing. See `Config::idle_timeout`.
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

/// What turns §7's `slots * 3 * 96KiB` memory request from a heuristic into a measurement, but
/// only after a week of it, not from one reading.
pub static ROOM_RESIDENT_BYTES: LazyLock<IntGaugeVec> =
    room_gauge!("puna_room_resident_bytes", "A room's resident set size");

/// Seconds since any client last sent anything: chat, `Sync`, `Get`, a status update. `null` until
/// one has, which is absent here rather than zero.
///
/// **Not the reaper's number**, despite what this comment said until 2026-08-21. It answers whether
/// a room's sockets are alive, which is worth a gauge on its own; whether the room is being *played*
/// is `last_check_at`, and conflating the two is exactly what P23 existed to end. Left under its own
/// honest name rather than repointed, because the two questions are both real.
pub static ROOM_IDLE_SECONDS: LazyLock<IntGaugeVec> = room_gauge!(
    "puna_room_idle_seconds",
    "Seconds since a room last heard from any client"
);

/// Clients dropped for falling too far behind.
///
/// **A counter fed by deltas, and that shape is forced.** pahoa reports a cumulative total, but
/// `prometheus`'s `IntCounter` exposes only `inc`/`inc_by`, and there is no `set`. Storing the total in
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

/// Slots this room is filtering, as the room counts them.
///
/// **pahoa's `filtered` is the EFFECTIVE state**, so a room-wide filter makes this the whole roster
/// rather than only the slots with rules of their own. That is the honest reading of "how many
/// slots have traffic being dropped", and it is deliberately not the same question the roster's
/// divergence chips answer.
pub static ROOM_SLOTS_FILTERED: LazyLock<IntGaugeVec> = room_gauge!(
    "puna_room_slots_filtered",
    "Slots in this room whose traffic a filter applies to"
);

/// Messages dropped because a filter matched what a slot **sent**.
pub static ROOM_FILTERED_FROM_SLOTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_room_filtered_from_slots_total",
                "Messages dropped by a filter on what a slot sent, per room"
            ),
            &["room"],
        )
        .unwrap()
    )
});

/// Messages dropped because a filter matched what a slot would **receive**.
///
/// **Counted per RECIPIENT, not per broadcast**: one chat line filtered for forty slots is forty,
/// which pahoa states explicitly and which makes this the number worth watching. Their words:
/// *a filter quietly discarding far more than an operator intended is the failure mode this feature
/// introduces.* An alert belongs on its **rate**, not its value: the total only ever climbs, and a
/// room that has been filtering correctly for a week has a large one.
pub static ROOM_FILTERED_TO_SLOTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_room_filtered_to_slots_total",
                "Messages dropped by a filter on their way to a slot, per room. Per recipient: one \
                 broadcast filtered for forty slots is forty"
            ),
            &["room"],
        )
        .unwrap()
    )
});

/// When the room's current Deployment was created: **how long this SPEC has been in force**.
///
/// A unix instant, not an age, which is deliberate: a gauge counting upward has to be re-set on
/// every scrape to stay true, where an instant is written once and `time() - x` is the age at
/// whatever moment somebody asks. It is also what makes a restart legible as a *step* rather than
/// as a sawtooth nobody was watching at the right second.
///
/// **`_timestamp_seconds`, not `_seconds`**, because `puna_room_idle_seconds` next door is a
/// duration and two `_seconds` families meaning different things is exactly the ambiguity this
/// codebase keeps paying for. pahoa spells `pahoa_last_save_timestamp_seconds` the same way.
pub static ROOM_DEPLOYMENT_CREATED: LazyLock<IntGaugeVec> = room_gauge!(
    "puna_room_deployment_created_timestamp_seconds",
    "Unix time the room's current Deployment was created: how long its spec has been in force"
);

/// When the room's **process** started: how long *this pahoa* has been serving.
///
/// **The pair is the point, and they are different questions.** The deployment age is how long the
/// current spec has been in force; this is how long the thing answering right now has been up. They
/// diverge when Kubernetes moved the pod (eviction, drain, preemption) or when the container
/// restarted in place, and either way **the room reloaded its save and every client reconnected**,
/// which an organizer notices and Puna could not otherwise explain.
///
/// It is also the honest explanation for a discontinuity in the re-exported room counters: pahoa's
/// totals reset to zero with the process, so a step here is the cliff on a cumulative graph.
///
/// Read as `time() - x` for an age, and `changes(x[range])` for how many times a room restarted
/// while somebody was looking.
pub static ROOM_PROCESS_STARTED: LazyLock<IntGaugeVec> = room_gauge!(
    "puna_room_process_started_timestamp_seconds",
    "Unix time the room's pahoa process started: how long this process has been serving"
);

/// A room's name, as an **info metric**: always `1`, carrying the label.
///
/// Every other series here is keyed by the room's uuid, which is correct: it is the identity, it
/// never changes, and a rename must not fork a counter into a new time series. It is also unusable
/// on a dashboard, where the reader wants "Thursday Sync" and not `9f3c…`.
///
/// This is the standard way out: one series per room joined at query time,
/// `… * on(room) group_left(name) puna_room_info`, so the name reaches a legend or a variable
/// without being carried on the ~28,000 series the proxy publishes, where a rename would fork every
/// one of them.
///
/// **No new disclosure.** `/room/<id>` renders the room name to anybody holding the link, so this
/// is already public where the slot names on the proxied series were a deliberate widening.
pub static ROOM_INFO: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register!(
        IntGaugeVec::new(
            Opts::new(
                "puna_room_info",
                "Always 1. Carries a room's name for joining onto its uuid-keyed series"
            ),
            &["room", "name"],
        )
        .unwrap()
    )
});

/// The name last published for each room, so a rename can retract the old series.
///
/// **`remove_label_values` needs the FULL label set**, so removing `(room, name)` requires knowing
/// the name that was published, which the caller no longer has once the room has been renamed.
/// Without this, renaming a room leaves its old name asserting `1` forever and the dropdown grows
/// an entry for a room that no longer goes by it. Same trap `retain_rooms` exists for, one label
/// deeper.
static ROOM_NAMES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Publish when a room's Deployment was created, from the cluster snapshot.
///
/// Separate entry point from [`publish_room`] because it has a **different observer**: this is what
/// Kubernetes says, where the process start is what the room says. A pair that agrees is a room
/// running the pod it was given; a process younger than its Deployment is a pod that moved or a
/// container that restarted in place, and that difference is only visible because the two numbers
/// arrive by different routes.
///
/// `None` removes the series, the same rule the gauges follow: a room with no Deployment is not a
/// room whose Deployment was created at the epoch.
pub fn publish_room_deployment(room: &str, created_at: Option<chrono::DateTime<chrono::Utc>>) {
    match created_at {
        Some(at) => ROOM_DEPLOYMENT_CREATED
            .with_label_values(&[room])
            .set(at.timestamp()),
        None => {
            let _ = ROOM_DEPLOYMENT_CREATED.remove_label_values(&[room]);
        }
    }
}

/// Publish (or re-publish) a room's name.
pub fn publish_room_info(room: &str, name: &str) {
    let Ok(mut names) = ROOM_NAMES.lock() else {
        return;
    };
    match names.get(room) {
        // Unchanged: the gauge already says this, and re-setting it every tick is noise.
        Some(previous) if previous == name => return,
        Some(previous) => {
            let _ = ROOM_INFO.remove_label_values(&[room, previous]);
        }
        None => {}
    }
    names.insert(room.to_string(), name.to_string());
    ROOM_INFO.with_label_values(&[room, name]).set(1);
}

/// How many series this room's own exposition currently contributes, per room.
///
/// The cardinality of the proxy, per room, which is the number worth having when it turns out to
/// be too large: `slots × message types` is the product pahoa's handoff costed at ~28,000 for a
/// 2000-slot sync, and this says which room is producing it rather than that the total is high.
pub static ROOM_METRICS_SERIES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register!(
        IntGaugeVec::new(
            Opts::new(
                "puna_room_metrics_series",
                "Series re-exported from a room's own /admin/v1/metrics"
            ),
            &["room"],
        )
        .unwrap()
    )
});

/// Samples a room offered that were refused. See [`proxy`] for each reason.
///
/// Sitting at zero is the answer this is for: the proxy passes through names Puna does not choose,
/// so "nothing was dropped" is what says the pass-through is total rather than quietly partial.
pub static ROOM_METRICS_DROPPED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register!(
        IntCounterVec::new(
            Opts::new(
                "puna_room_metrics_dropped_total",
                "Samples from a room's own metrics that were not re-exported"
            ),
            &["reason"],
        )
        .unwrap()
    )
});

/// The cumulative totals last polled from a room, one per counter re-exported from it.
///
/// A struct rather than a bare `i64` because there are three of these now, and they share a
/// lifetime: a room leaves all of them at the same moment.
#[derive(Debug, Clone, Copy, Default)]
struct Cumulative {
    lag_disconnects: i64,
    filtered_from_slots: i64,
    filtered_to_slots: i64,
}

/// Rooms with series published, and the last cumulative counter values seen for each.
///
/// Two jobs in one map because they have the same lifetime: a room leaves both at the same moment.
static ROOM_SERIES: LazyLock<Mutex<HashMap<String, Cumulative>>> =
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
    set(&ROOM_SLOTS_FILTERED, room, status.filters.slots_filtered);
    // The room's own answer for when its process started. Its partner,
    // `ROOM_DEPLOYMENT_CREATED`, is written from the cluster snapshot instead. The two come from
    // different observers on purpose, which is what makes a disagreement between them mean
    // something. See `publish_room_deployment`.
    set(
        &ROOM_PROCESS_STARTED,
        room,
        status.started_at.map(|at| at.timestamp()),
    );

    /// Advance a re-exported counter by the difference since the last poll.
    ///
    /// **`total < previous` means the ROOM restarted** and began its counters again at zero, so the
    /// delta is the new total rather than a negative. On the first sighting the whole total is
    /// added: the room may have been running before this orchestrator was, and the process's own
    /// counter started at zero, so this is what makes the series mean what its name says.
    fn advance(vec: &IntCounterVec, room: &str, total: Option<i64>, previous: Option<i64>) {
        let Some(total) = total else {
            return;
        };
        let delta = match previous {
            Some(previous) if total >= previous => total - previous,
            _ => total,
        };
        if delta > 0 {
            vec.with_label_values(&[room]).inc_by(delta as u64);
        }
    }

    let Ok(mut series) = ROOM_SERIES.lock() else {
        return;
    };
    let previous = series.get(room).copied();
    series.insert(
        room.to_string(),
        Cumulative {
            lag_disconnects: status.net.lag_disconnects.unwrap_or(0),
            filtered_from_slots: status.filters.dropped_from_slots.unwrap_or(0),
            filtered_to_slots: status.filters.dropped_to_slots.unwrap_or(0),
        },
    );

    advance(
        &ROOM_LAG_DISCONNECTS,
        room,
        status.net.lag_disconnects,
        previous.map(|p| p.lag_disconnects),
    );
    advance(
        &ROOM_FILTERED_FROM_SLOTS,
        room,
        status.filters.dropped_from_slots,
        previous.map(|p| p.filtered_from_slots),
    );
    advance(
        &ROOM_FILTERED_TO_SLOTS,
        room,
        status.filters.dropped_to_slots,
        previous.map(|p| p.filtered_to_slots),
    );
}

/// Drop every series for a room that is no longer live.
///
/// **The trap this exists for**: a `GaugeVec` keyed by room id keeps a series forever unless it is
/// removed, so without this every room that has ever run would leave behind a series asserting its
/// last-known client count. A stale gauge reads as a live room, which is worse than no metric.
///
/// Level-triggered on purpose: it reconciles the published set against the live set rather than
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
            &*ROOM_SLOTS_FILTERED,
            &*ROOM_DEPLOYMENT_CREATED,
            &*ROOM_PROCESS_STARTED,
        ] {
            let _ = vec.remove_label_values(&[room]);
        }
        for vec in [
            &*ROOM_LAG_DISCONNECTS,
            &*ROOM_FILTERED_FROM_SLOTS,
            &*ROOM_FILTERED_TO_SLOTS,
        ] {
            let _ = vec.remove_label_values(&[room]);
        }
        false
    });

    // The name gauge keys on `(room, name)`, so it needs the name that was published rather than
    // the one the room currently has: see `ROOM_NAMES`.
    if let Ok(mut names) = ROOM_NAMES.lock() {
        names.retain(|room, name| {
            if live.contains(room) {
                return true;
            }
            let _ = ROOM_INFO.remove_label_values(&[room, name]);
            false
        });
    }

    // **Reconciled against `live` in its own right, not swept alongside the map above.** The
    // proxied families are keyed by `(room, slot, cmd, ...)`, so a room left behind does not
    // strand one stale reading but every series it ever had, and driving that off `ROOM_SERIES`
    // would make it depend on the status publisher having seen the same room, which is true today
    // and is not a property either side states. Found by a test that published metrics for a room
    // and no status: the room was dropped from every gauge and kept re-exporting.
    proxy::retain(live);
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
/// `pg_enum` and compares: adding a state to the migration without adding it here fails there.
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
pub const START_RESULTS: &[&str] = &[
    "ok",
    "failed",
    "port_exhausted",
    "ip_mismatch",
    "address_refused",
    "address_unsatisfiable",
];

/// Who holds the port a refusal collided with, for [`PORT_REFUSALS`].
///
/// **Seeded to zero so `internal` renders as a real zero.** That series should never move (it means
/// Puna is holding a port against itself) and an alert on it is only trustworthy if the absence of
/// the series and a genuine zero are distinguishable.
pub const PORT_CONFLICTS: &[&str] = &["external", "internal"];

/// The two cadences the reconcile loop runs at, for [`RECONCILE_TICKS`].
///
/// **Seeded to zero, so "no convergence is happening" is a reading rather than an absence**, which
/// matters more here than for most families, because a converge series that is simply missing looks
/// identical to a loop that has stopped converging when it should be.
///
/// The orchestrator's `plan::TickKind::as_str` must produce exactly these, and a test over there
/// asserts it. A publisher writing a label this list does not seed is the mistake
/// [`PROBE_CAPABILITY`] was built to stop repeating.
pub const TICK_KINDS: &[&str] = &["reconcile", "converge"];

/// Capabilities a room probe may or may not have, depending on the pahoa build it is talking to.
/// Publish what the room probe can do.
///
/// **One writer, one vocabulary.** This used to be a `const` list declared here, before any probe
/// existed, guessing at `activity` and `client_count`; M11 then built the real capabilities under
/// different names. Both wrote to the same gauge, so it carried the union and asserted `0` for two
/// capabilities that no longer name anything, next to the very series that disproved them.
///
/// Now the names come from [`crate::probe::ProbeCapabilities`] itself, and this is the only thing that writes
/// them.
pub fn publish_probe_capabilities(capabilities: &crate::probe::ProbeCapabilities) {
    for (name, present) in capabilities.as_pairs() {
        PROBE_CAPABILITY
            .with_label_values(&[name])
            .set(i64::from(present));
    }
}

/// Families every component exports, because every component does the thing they measure.
///
/// Only database timing so far, and it belongs here for a concrete reason rather than by default:
/// all three tiers hold a diesel pool, so a query-latency series scoped to one of them would
/// answer "is Postgres slow" for a third of the traffic.
pub const SHARED_FAMILIES: &[&str] = &["diesel_query_seconds"];

/// Families **both roles of the web binary** export, and the orchestrator does not.
///
/// Its own table rather than a repeat in the two below, because the family tables have to
/// *partition* the registry: a name in two of them makes `families()` describe an ownership that
/// is not real, and the disjointness test says so. These three have one real owner and it is "the
/// tier that serves HTTP", which is two components.
///
/// Not [`SHARED_FAMILIES`] either: the orchestrator's only listener is its health and metrics
/// server, so a request count from it would answer a different question in the same name.
pub const HTTP_FAMILIES: &[&str] = &[
    "puna_http_requests_total",
    "puna_http_request_bytes_total",
    "puna_http_response_bytes_total",
];

/// Families only `puna-web` exports. Empty today: what it serves that the tracker does not is
/// measured by [`HTTP_FAMILIES`]'s `kind` label rather than by families of its own.
pub const WEB_FAMILIES: &[&str] = &[];

/// Families only `puna-tracker` exports. Empty today.
///
/// Upstream fetch counts and cache hit rates belong here (the numbers that say whether the three
/// cache layers are doing their job) when there is something to attach them to. Its HTTP traffic is
/// in [`HTTP_FAMILIES`], which the web role exports too.
pub const TRACKER_FAMILIES: &[&str] = &[];

/// Families only `puna-orchestrator` exports.
///
/// Everything about rooms, ports, reconciliation and the cluster, which is nearly the whole
/// registry: these are all computed by the reconcile loop or by the sweep, and no other process
/// has the inputs to compute any of them. `puna_commands_total`, `puna_command_seconds` and
/// `puna_probe_capability` have no producer yet (M11 and M12) and are listed here because that is
/// where their producer will be: the dispatcher and the probe are orchestrator-side.
pub const ORCHESTRATOR_FAMILIES: &[&str] = &[
    "puna_rooms",
    "puna_room_starts_total",
    "puna_room_start_seconds",
    "puna_ports_capacity",
    "puna_ports_bound",
    "puna_ports_quarantined",
    "puna_port_reclaims_total",
    "puna_port_refusals_total",
    "puna_port_ip_mismatch_total",
    "puna_generations",
    "puna_generation_bytes",
    "puna_slots_unclaimed",
    "puna_orchestrator_leader",
    "puna_integrity_faults",
    "puna_orphan_directories",
    "puna_reconcile_seconds",
    "puna_reconcile_ticks_total",
    "puna_rooms_converging",
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
    "puna_room_slots_filtered",
    "puna_room_filtered_from_slots_total",
    "puna_room_filtered_to_slots_total",
    // The proxy's own bookkeeping. The families it PROXIES are deliberately not listed here and
    // cannot be: their names come from pahoa, not from Puna. See `proxied_families_are_not
    // _declared_here` in `tests/metrics_proxy.rs` for why that is the design rather than a gap.
    "puna_room_metrics_series",
    "puna_room_metrics_dropped_total",
    "puna_room_info",
    "puna_room_deployment_created_timestamp_seconds",
    "puna_room_process_started_timestamp_seconds",
];

/// Families that are REGISTERED but do not appear until something writes a series.
///
/// An orthogonal axis to the per-component tables, and a real one: a labeled family renders no
/// `# TYPE` line at all while it has no children, so "registered" and "visible in `/metrics`" are
/// different sets. Every name here is a `*Vec` whose label space is combinatorial and mostly
/// uninteresting: pre-seeding them would trade one confusion for a wall of permanent zeros, so
/// [`init`] deliberately leaves them empty.
///
/// `diesel_query_seconds` is the one that is not a choice: it is labeled by query, so it cannot be
/// seeded without inventing a query name. It shows up as soon as the process talks to Postgres,
/// which for the web tiers is the readiness probe.
///
/// The distinction is worth encoding because it decides what a dashboard sees on a cold process,
/// and because moving a family across it is a decision rather than an accident: the scope tests
/// fail either way round.
pub const DEFERRED_FAMILIES: &[&str] = &[
    "diesel_query_seconds",
    "puna_commands_total",
    "puna_k8s_requests_total",
    "puna_reconcile_errors_total",
    // Labeled by room, so they appear only once a room has been probed, and disappear again when
    // it stops, which is what `retain_rooms` is for.
    "puna_room_clients_connected",
    "puna_room_mailbox_depth",
    "puna_room_outbound_queued_bytes",
    "puna_room_resident_bytes",
    "puna_room_idle_seconds",
    "puna_room_lag_disconnects_total",
    "puna_room_slots_filtered",
    "puna_room_filtered_from_slots_total",
    "puna_room_filtered_to_slots_total",
    "puna_room_metrics_series",
    "puna_room_info",
    "puna_room_deployment_created_timestamp_seconds",
    "puna_room_process_started_timestamp_seconds",
];

/// The families `component` registers, shared ones included.
///
/// Used by the scope tests, and by anyone writing an alert who needs to know which job a series
/// can legitimately come from.
pub fn families(component: Component) -> Vec<&'static str> {
    let (http, own) = match component {
        Component::Web => (HTTP_FAMILIES, WEB_FAMILIES),
        Component::Tracker => (HTTP_FAMILIES, TRACKER_FAMILIES),
        // Not a tier that serves HTTP: its listener is health and metrics, nothing else.
        Component::Orchestrator => (&[][..], ORCHESTRATOR_FAMILIES),
    };
    SHARED_FAMILIES
        .iter()
        .chain(http)
        .chain(own)
        .copied()
        .collect()
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
/// series at all, and a panel reading "no data" is ambiguous in exactly the case where 0 is the
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
        Component::Web => init_http(WEB_HTTP_KINDS),
        Component::Tracker => init_http(TRACKER_HTTP_KINDS),
        Component::Orchestrator => init_orchestrator(),
    }
}

/// The two web roles' request accounting, seeded for the kinds this role can actually serve.
///
/// Seeded with an empty `room`, which is a real label value here rather than a placeholder: it is
/// what every request that is not about one room carries, and on the roomless kinds it is the only
/// value they will ever have.
fn init_http(kinds: &[&str]) {
    for kind in kinds {
        HTTP_REQUESTS.with_label_values(&[kind, ""]).reset();
        HTTP_REQUEST_BYTES.with_label_values(&[kind, ""]).reset();
        HTTP_RESPONSE_BYTES.with_label_values(&[kind, ""]).reset();
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
    LazyLock::force(&ROOM_SLOTS_FILTERED);
    LazyLock::force(&ROOM_FILTERED_FROM_SLOTS);
    LazyLock::force(&ROOM_FILTERED_TO_SLOTS);
    LazyLock::force(&ROOM_METRICS_SERIES);
    LazyLock::force(&ROOM_METRICS_DROPPED);
    LazyLock::force(&ROOM_INFO);
    LazyLock::force(&ROOM_DEPLOYMENT_CREATED);
    LazyLock::force(&ROOM_PROCESS_STARTED);
    LazyLock::force(&ROOM_START_SECONDS);
    LazyLock::force(&PORTS_CAPACITY);
    LazyLock::force(&PORTS_BOUND);
    LazyLock::force(&PORTS_QUARANTINED);
    LazyLock::force(&PORT_RECLAIMS);
    LazyLock::force(&PORT_REFUSALS);
    LazyLock::force(&PORT_IP_MISMATCH);
    LazyLock::force(&ORCHESTRATOR_LEADER);
    LazyLock::force(&INTEGRITY_FAULTS);
    LazyLock::force(&ORPHAN_DIRECTORIES);
    LazyLock::force(&GENERATIONS);
    LazyLock::force(&GENERATION_BYTES);
    LazyLock::force(&SLOTS_UNCLAIMED);
    LazyLock::force(&RECONCILE_SECONDS);
    LazyLock::force(&RECONCILE_TICKS);
    LazyLock::force(&ROOMS_CONVERGING);
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
    for kind in TICK_KINDS {
        RECONCILE_TICKS.with_label_values(&[kind]).reset();
    }
    for conflict in PORT_CONFLICTS {
        PORT_REFUSALS.with_label_values(&[conflict]).reset();
    }
    // Seeded, so "the proxy is passing everything through" is a row of zeros rather than a family
    // that has not appeared yet: the same reason `puna_integrity_faults` is seeded.
    for reason in proxy::DROP_REASONS {
        ROOM_METRICS_DROPPED.with_label_values(&[reason]).reset();
    }
    // Registered last, and only here: it is the one collector in this process with no descriptors,
    // and a second would silently fail to register. See `metrics::proxy`.
    proxy::register(&REGISTRY);
    // Seeded to zero so a cold orchestrator renders every capability rather than none, and from
    // the SAME vocabulary the publisher uses: the two diverging is exactly what went wrong.
    for capability in crate::probe::ProbeCapabilities::NAMES {
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
        // **Guarded, because `init` WRITES.** It reseeds every `PROBE_CAPABILITY` child to zero,
        // and unguarded this ran between the capability test's publish and its assertion,
        // failing it with "was seeded but never published", which reads as the publisher being
        // broken rather than as this test having zeroed the gauge underneath it. Intermittent:
        // reproduced twice in sixty runs of the full lib, and never when the metrics tests were
        // run alone, because it needs enough other tests in flight to lose the race.
        //
        // The rule these statics impose: anything touching the shared registry takes `exclusive`,
        // reads included: a read here is a read of state another test is mid-way through writing.
        let _guard = exclusive();

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
        // Shares the registry with the capability test, which publishes to the same gauge.
        let _guard = exclusive();

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
        for capability in crate::probe::ProbeCapabilities::NAMES {
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

    /// The room series are PROCESS-GLOBAL, and `retain_rooms` is a whole-fleet reconcile, so a
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
    /// child if it is missing and so can never answer "is there a series": the exact question
    /// both traps turn on.
    fn gauge(room: &str) -> Option<i64> {
        gauge_of(&ROOM_CLIENTS, room)
    }

    /// The same question of any room-labeled gauge. See [`gauge`].
    fn gauge_of(vec: &IntGaugeVec, room: &str) -> Option<i64> {
        prometheus::core::Collector::collect(vec)
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
    /// count, indefinitely, and indistinguishably from a room that really has three people in it.
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

    /// A probe that cannot tell removes the series rather than publishing a zero: the same
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
    /// `IntCounter` has no `set`, so this advances by the difference, and a room restart, where
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

    /// **The three re-exported counters keep SEPARATE baselines.**
    ///
    /// They used to share one `i64` per room, because there was only one of them. Widening that to
    /// a struct is the kind of change that compiles either way: with a shared baseline, a room
    /// filtering steadily while dropping nobody for lag would credit the lag counter with the
    /// filter's total: a counter climbing for a reason that has nothing to do with its name, on
    /// the one metric an operator reaches for when clients are being dropped.
    #[test]
    fn each_re_exported_counter_advances_on_its_own_baseline() {
        let _guard = exclusive();
        let room = "55555555-5555-5555-5555-555555555555";
        let count = |vec: &IntCounterVec| {
            vec.get_metric_with_label_values(&[room])
                .expect("the child")
                .get()
        };

        let filtering = |from: i64, to: i64, lag: i64| crate::probe::RoomStatus {
            net: NetStatus {
                clients_connected: Some(1),
                lag_disconnects: Some(lag),
                ..Default::default()
            },
            filters: crate::probe::FilterStatus {
                slots_filtered: Some(4),
                dropped_from_slots: Some(from),
                dropped_to_slots: Some(to),
            },
            ..Default::default()
        };

        // **Every total moves, and by wildly different amounts**, which is what makes a crossed
        // baseline visible. An earlier version of this test held lag at zero, which was true under the bug
        // as well as under the fix, so it proved nothing.
        publish_room(room, &filtering(10, 400, 5));
        assert_eq!(count(&ROOM_FILTERED_FROM_SLOTS), 10);
        assert_eq!(count(&ROOM_FILTERED_TO_SLOTS), 400);
        assert_eq!(count(&ROOM_LAG_DISCONNECTS), 5);

        // The filters do most of the work; one more client lags out. Each counter advances against
        // its own last reading, so lag goes up by one and not by the filter's five hundred.
        publish_room(room, &filtering(12, 900, 6));
        assert_eq!(count(&ROOM_FILTERED_FROM_SLOTS), 12);
        assert_eq!(count(&ROOM_FILTERED_TO_SLOTS), 900);
        assert_eq!(
            count(&ROOM_LAG_DISCONNECTS),
            6,
            "a crossed baseline credits lag with traffic the filter dropped"
        );

        // `slots_filtered` is a gauge and follows the same null-is-not-zero rule as the rest.
        assert!(
            ROOM_SLOTS_FILTERED
                .get_metric_with_label_values(&[room])
                .is_ok()
        );
        publish_room(
            room,
            &crate::probe::RoomStatus {
                net: NetStatus {
                    clients_connected: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let published = prometheus::core::Collector::collect(&*ROOM_SLOTS_FILTERED)
            .iter()
            .flat_map(prometheus::proto::MetricFamily::get_metric)
            .any(|m| {
                m.get_label()
                    .iter()
                    .any(|l| l.name() == "room" && l.value() == room)
            });
        assert!(
            !published,
            "a room that cannot report its filters is not a room filtering nothing"
        );

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

    /// **The regression.** Two writers had two vocabularies, so the gauge carried their union and
    /// asserted `puna_probe_capability{capability="client_count"} 0` beside a populated
    /// `puna_room_clients_connected`: a flat contradiction, live, for as long as both existed.
    ///
    /// Asserted on the RENDERED label set rather than on the constant, because comparing the
    /// constant to itself is what the old code would also have passed.
    #[test]
    fn the_capability_gauge_carries_exactly_the_capabilities_that_exist() {
        use crate::probe::{ProbeCapabilities, RoomProbe};

        let _guard = exclusive();

        // **The real seeding path, not a hand-rolled copy of it.** Seeding from `NAMES` here would
        // compare the constant to itself and pass against the very divergence this exists to catch,
        // which it did, until a mutation test said so.
        init(Component::Orchestrator);
        publish_probe_capabilities(&crate::probe::HttpsProbe.capabilities());

        let published: std::collections::BTreeSet<String> =
            prometheus::core::Collector::collect(&*PROBE_CAPABILITY)
                .iter()
                .flat_map(prometheus::proto::MetricFamily::get_metric)
                .flat_map(|m| m.get_label().to_vec())
                .filter(|l| l.name() == "capability")
                .map(|l| l.value().to_string())
                .collect();

        let expected: std::collections::BTreeSet<String> = ProbeCapabilities::NAMES
            .iter()
            .map(|n| (*n).to_string())
            .collect();

        assert_eq!(
            published, expected,
            "the gauge names capabilities that do not exist, or omits ones that do"
        );

        // And the values are the probe's own answers, not a seeded zero left behind.
        for (name, present) in crate::probe::HttpsProbe.capabilities().as_pairs() {
            assert_eq!(
                PROBE_CAPABILITY
                    .get_metric_with_label_values(&[name])
                    .expect("the child")
                    .get(),
                i64::from(present),
                "{name} was seeded but never published"
            );
        }

        // Left as the seeding leaves it, because a sibling test asserts a cold process's view.
        for capability in ProbeCapabilities::NAMES {
            PROBE_CAPABILITY.with_label_values(&[capability]).set(0);
        }
    }

    /// **The two ages come from two observers, and both are instants rather than durations.**
    ///
    /// A gauge counting upward would have to be re-set on every scrape to stay true; an instant is
    /// written once and `time() - x` is the age whenever somebody asks. The pair is what makes a pod
    /// that moved distinguishable from a room that has simply been up a while, so the test asserts
    /// they are independent, since a refactor that fed both from one reading would produce two
    /// series that agree by construction and answer only one question.
    #[test]
    fn the_deployment_and_process_instants_are_written_independently() {
        let _guard = exclusive();
        let room = "two-observers";
        let at = |secs: i64| chrono::DateTime::from_timestamp(secs, 0).expect("an instant");

        publish_room_deployment(room, Some(at(1_000)));
        let mut status = crate::probe::RoomStatus {
            started_at: Some(at(1_500)),
            ..Default::default()
        };
        publish_room(room, &status);

        assert_eq!(gauge_of(&ROOM_DEPLOYMENT_CREATED, room), Some(1_000));
        assert_eq!(gauge_of(&ROOM_PROCESS_STARTED, room), Some(1_500));

        // The room restarted in place: its process moves, the Deployment does not. That gap is the
        // whole signal, and it is what an organizer saw as everybody reconnecting.
        status.started_at = Some(at(9_000));
        publish_room(room, &status);
        assert_eq!(gauge_of(&ROOM_DEPLOYMENT_CREATED, room), Some(1_000));
        assert_eq!(gauge_of(&ROOM_PROCESS_STARTED, room), Some(9_000));

        // A room that cannot say when it started publishes nothing, rather than the epoch: the
        // same null-is-not-zero rule the rest of these follow. `time() - 0` would render as
        // fifty-six years of uptime.
        status.started_at = None;
        publish_room(room, &status);
        assert_eq!(gauge_of(&ROOM_PROCESS_STARTED, room), None);

        retain_rooms(&std::collections::HashSet::new());
        assert_eq!(gauge_of(&ROOM_DEPLOYMENT_CREATED, room), None);
    }

    /// **A rename must retract the old series**, and this is the whole reason `ROOM_NAMES` exists.
    ///
    /// `remove_label_values` needs the full label set, so retracting `(room, name)` needs the name
    /// that was published, which the caller no longer has once the room has been renamed. Without
    /// the map, a renamed room asserts `1` under both names forever: the join then multiplies every
    /// series it touches, and a dropdown built from this grows an entry for a name nothing goes by.
    #[test]
    fn renaming_a_room_retracts_the_name_it_was_published_under() {
        let _guard = exclusive();
        let room = "renamed-room";

        publish_room_info(room, "Thursday Sync");
        assert_eq!(
            ROOM_INFO
                .get_metric_with_label_values(&[room, "Thursday Sync"])
                .expect("the child")
                .get(),
            1
        );

        publish_room_info(room, "Friday Sync");
        assert_eq!(published_names(room), vec!["Friday Sync".to_string()]);

        // And it goes entirely when the room does.
        retain_rooms(&std::collections::HashSet::new());
        assert!(published_names(room).is_empty());
    }

    /// Names currently published for a room, read out of the rendered exposition.
    ///
    /// Through `gather` rather than through the gauge, because a retracted child is *absent* rather
    /// than zero, and asking the vec for a child it does not have would create one.
    fn published_names(room: &str) -> Vec<String> {
        gather()
            .lines()
            .filter(|line| line.starts_with("puna_room_info{"))
            .filter(|line| line.contains(&format!("room=\"{room}\"")))
            .filter_map(|line| line.split_once("name=\""))
            .filter_map(|(_, rest)| rest.split_once('"'))
            .map(|(name, _)| name.to_string())
            .collect()
    }

    /// The pairs and the names are one list, so a capability added to the struct cannot be
    /// published under a name the seeding does not know.
    #[test]
    fn every_capability_has_a_name_and_every_name_a_capability() {
        use crate::probe::ProbeCapabilities;

        let all = ProbeCapabilities {
            status: true,
            commands: true,
            graceful_shutdown: true,
            metrics: true,
        };
        let named: Vec<&str> = all.as_pairs().iter().map(|(n, _)| *n).collect();

        assert_eq!(named, ProbeCapabilities::NAMES);
        assert!(
            all.as_pairs().iter().all(|(_, present)| *present),
            "a field was dropped from as_pairs, so it would never be published"
        );
    }
}
