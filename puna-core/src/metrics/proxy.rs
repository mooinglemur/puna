//! Re-exporting a room's own Prometheus exposition under this process's registry.
//!
//! pahoa serves `/admin/v1/metrics` with per-slot, per-message-type counters. Puna scrapes it on
//! the probe pass, adds `room="<uuid>"`, and republishes — so **a Prometheus scrape reads a cache
//! and never reaches a live multiworld**, which is the property the whole poll-cache-re-export
//! design exists for. See `puna-orchestrator/src/probing.rs` for why the alternative, a
//! ServiceMonitor per room, was rejected.
//!
//! ## Why a `Collector` rather than mirroring each family
//!
//! **Nothing here knows what pahoa exports**, and that is the point: a label or a metric added on
//! their side needs no release on ours. Mirroring would mean declaring each family, which brings
//! back everything the exposition format already solves — and two specific walls in the
//! `prometheus` crate. `IntCounter` has no `set`, so a cumulative total has to be tracked and
//! advanced by difference; and `remove_label_values` needs the *full* label set, which is
//! unknowable for a label space Puna does not define. A collector sidesteps both by holding
//! already-built protos and handing them over on demand.
//!
//! [`Registry::gather`](prometheus::Registry::gather) also **merges families by name across
//! collectors and sorts their metrics**, so two hundred rooms exporting `pahoa_packets_in_total`
//! render as one family — which the text format requires, since a second `# HELP` line for one
//! name is a parse error at the far end.
//!
//! ## The landmine: one desc-less collector per registry
//!
//! [`Collector::desc`] returns nothing here, because the descriptors are not knowable ahead of the
//! scrape. `Registry::register` sums desc ids into a collector id, so **every desc-less collector
//! hashes to the same id and the second one registers as `AlreadyReg`** — a silent
//! nothing-happens, since registration failures are conventionally ignored. There is exactly one
//! in this process and `tests/metrics_proxy.rs` pins that.
//!
//! ## What is refused, and why refusing beats passing through
//!
//! The orchestrator's own `/metrics` must not be breakable by a room, so three things are dropped
//! rather than re-exported, each counted under `puna_room_metrics_dropped_total`:
//!
//! - **A name that collides with a family Puna owns.** `gather` merges by name, so a room
//!   exporting `puna_rooms` would have its metrics folded into Puna's own family — one series
//!   silently claiming to be a reading this process made.
//! - **Histograms and summaries**, including their `_sum` and `_count` companions. Not a
//!   limitation worth hiding: `prometheus-parse` folds bucket lines into one sample and leaves the
//!   companions as separate untyped ones, so passing through what survives would publish half a
//!   histogram — a `_count` with no buckets reads as a working metric. pahoa exports none today;
//!   when it does, this is the place, and the counter is what says so.
//! - **An incoming `room` label**, which is replaced rather than duplicated. Two label pairs with
//!   one name is an invalid metric, and the value Puna adds is the authoritative one — it knows
//!   which room it just scraped.
//!
//! A room whose exposition cannot be parsed at all contributes nothing and leaves Puna's own
//! families untouched.

use std::collections::{BTreeMap, HashSet};
use std::sync::{LazyLock, Mutex};

use prometheus::core::Collector;
use prometheus::proto;

use super::{Component, ROOM_METRICS_DROPPED, ROOM_METRICS_SERIES, families};

/// Reasons a series is refused, and the label values of `puna_room_metrics_dropped_total`.
///
/// Seeded to zero by `init`, so "nothing was dropped" is a reading rather than an absence.
pub const DROP_REASONS: &[&str] = &["name_collision", "unsupported_type", "duplicate_room_label"];

/// What one room's exposition contributed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    /// Series now published for this room.
    pub series: usize,
    /// Samples refused. Every one is counted under `puna_room_metrics_dropped_total` too.
    pub dropped: usize,
}

/// Per-room converted families, ready to hand to `gather`.
///
/// Converted at poll time rather than at scrape time on purpose: a Prometheus scrape is a hot path
/// that several replicas may hit at once, and re-parsing a few megabytes of exposition per scrape
/// would put the load back exactly where the cache exists to remove it.
static PROXIED: LazyLock<Mutex<BTreeMap<String, Vec<proto::MetricFamily>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Parse a room's exposition, add `room`, and hold it until the next poll replaces it.
///
/// Replaces wholesale rather than merging: a family a room has stopped exporting must stop being
/// exported here, and the room's own document is the whole truth about what it currently has.
pub fn publish(room: &str, exposition: &str) -> Published {
    let scrape =
        match prometheus_parse::Scrape::parse(exposition.lines().map(|l| Ok(l.to_string()))) {
            Ok(scrape) => scrape,
            Err(e) => {
                tracing::warn!(
                    %room,
                    error = %e,
                    "a room's metrics could not be parsed; publishing none of them"
                );
                forget(room);
                return Published::default();
            }
        };

    let (converted, dropped) = convert(room, &scrape);
    let series: usize = converted.iter().map(|f| f.get_metric().len()).sum();

    for (reason, count) in dropped {
        if count > 0 {
            ROOM_METRICS_DROPPED
                .with_label_values(&[reason])
                .inc_by(count as u64);
        }
    }

    let total_dropped: usize = dropped.iter().map(|(_, n)| n).sum();

    if let Ok(mut proxied) = PROXIED.lock() {
        proxied.insert(room.to_string(), converted);
    }
    ROOM_METRICS_SERIES
        .with_label_values(&[room])
        .set(series as i64);

    Published {
        series,
        dropped: total_dropped,
    }
}

/// Drop everything proxied for a room.
///
/// Called both when a room stops being live and when a poll fails: a room that did not answer must
/// not keep asserting its last-known counters, which is the same null-is-not-a-reading rule the
/// gauges follow.
pub fn forget(room: &str) {
    if let Ok(mut proxied) = PROXIED.lock() {
        proxied.remove(room);
    }
    let _ = ROOM_METRICS_SERIES.remove_label_values(&[room]);
}

/// Drop every room that is no longer live, level-triggered.
///
/// Called by [`super::retain_rooms`], and reconciling against the live set rather than reacting to
/// each departure for the same reason it does: there are four ways a room stops being live, and a
/// hook per path is one somebody forgets.
pub fn retain(live: &std::collections::HashSet<String>) {
    let gone: Vec<String> = match PROXIED.lock() {
        Ok(proxied) => proxied
            .keys()
            .filter(|room| !live.contains(*room))
            .cloned()
            .collect(),
        Err(_) => return,
    };
    for room in gone {
        forget(&room);
    }
}

/// Turn one scrape into families carrying `room`, with a tally of what was refused.
fn convert(
    room: &str,
    scrape: &prometheus_parse::Scrape,
) -> (Vec<proto::MetricFamily>, [(&'static str, usize); 3]) {
    // Names this process owns. A proxied family sharing one would be merged into it by `gather`.
    let owned: HashSet<&str> = families(Component::Orchestrator).into_iter().collect();

    // A histogram's or summary's companions are ordinary samples named `x_sum` and `x_count`, so
    // refusing the family means refusing those too: otherwise a `_count` with no buckets behind
    // it survives, and reads as a metric that works.
    let structured: HashSet<&str> = scrape
        .samples
        .iter()
        .filter(|s| {
            matches!(
                s.value,
                prometheus_parse::Value::Histogram(_) | prometheus_parse::Value::Summary(_)
            )
        })
        .map(|s| s.metric.as_str())
        .collect();

    let mut collisions = 0usize;
    let mut unsupported = 0usize;
    let mut duplicate_room = 0usize;
    let mut by_name: BTreeMap<&str, proto::MetricFamily> = BTreeMap::new();

    for sample in &scrape.samples {
        let name = sample.metric.as_str();

        if owned.contains(name) {
            collisions += 1;
            continue;
        }
        if structured.contains(name) || companion_of_structured(name, &structured) {
            unsupported += 1;
            continue;
        }

        let (kind, value) = match sample.value {
            prometheus_parse::Value::Counter(v) => (proto::MetricType::COUNTER, v),
            prometheus_parse::Value::Gauge(v) => (proto::MetricType::GAUGE, v),
            prometheus_parse::Value::Untyped(v) => (proto::MetricType::UNTYPED, v),
            prometheus_parse::Value::Histogram(_) | prometheus_parse::Value::Summary(_) => {
                unsupported += 1;
                continue;
            }
        };

        let mut metric = proto::Metric::default();
        let mut labels: Vec<proto::LabelPair> = Vec::new();
        for (key, value) in sample.labels.iter() {
            if key == ROOM_LABEL {
                duplicate_room += 1;
                continue;
            }
            labels.push(pair(key, value));
        }
        labels.push(pair(ROOM_LABEL, room));
        // The encoder does not sort, and a stable order keeps a diff of two scrapes readable.
        labels.sort_by(|a, b| a.name().cmp(b.name()));
        metric.set_label(labels);

        match kind {
            proto::MetricType::COUNTER => {
                let mut counter = proto::Counter::default();
                counter.set_value(value);
                metric.set_counter(counter);
            }
            proto::MetricType::GAUGE => {
                let mut gauge = proto::Gauge::default();
                gauge.set_value(value);
                metric.set_gauge(gauge);
            }
            // A sample whose family carried no `# TYPE` line. pahoa always writes one, so this is
            // the degenerate case; but a proxy that dropped data because the far side omitted a
            // header would be discarding a real reading over a formatting detail.
            //
            // `prometheus` deprecates the untyped setters as protobuf-specific rather than as
            // wrong; `expect` rather than `allow` so their removal fails the build with something
            // to read instead of a warning nobody sees.
            #[expect(
                deprecated,
                reason = "the only way to express an untyped sample; deprecated as protobuf-specific, not as incorrect"
            )]
            _ => {
                let mut untyped = proto::Untyped::default();
                untyped.set_value(value);
                metric.set_untyped(untyped);
            }
        }

        by_name
            .entry(name)
            .or_insert_with(|| {
                let mut family = proto::MetricFamily::default();
                family.set_name(name.to_string());
                // pahoa's own help text, verbatim, which is half of what "expose it and we proxy
                // it" buys. The fallback is never empty: an encoder writing `# HELP name` with
                // nothing after it is a line a parser has to guess about.
                family.set_help(
                    scrape
                        .docs
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| format!("re-exported from a room ({name})")),
                );
                family.set_field_type(kind);
                family
            })
            .mut_metric()
            .push(metric);
    }

    (
        by_name.into_values().collect(),
        [
            ("name_collision", collisions),
            ("unsupported_type", unsupported),
            ("duplicate_room_label", duplicate_room),
        ],
    )
}

const ROOM_LABEL: &str = "room";

/// Is this the `_sum` or `_count` of a histogram or summary that was refused?
fn companion_of_structured(name: &str, structured: &HashSet<&str>) -> bool {
    ["_sum", "_count", "_bucket"]
        .iter()
        .filter_map(|suffix| name.strip_suffix(suffix))
        .any(|stem| structured.contains(stem))
}

fn pair(name: &str, value: &str) -> proto::LabelPair {
    let mut pair = proto::LabelPair::default();
    pair.set_name(name.to_string());
    pair.set_value(value.to_string());
    pair
}

/// Hands the cached families to `Registry::gather`, which merges them by name across rooms.
struct RoomMetrics;

impl Collector for RoomMetrics {
    /// **Deliberately empty**, and the reason is in the module docs: descriptors are not knowable
    /// before a room is scraped. The cost is that only one desc-less collector may be registered
    /// per registry.
    fn desc(&self) -> Vec<&prometheus::core::Desc> {
        Vec::new()
    }

    fn collect(&self) -> Vec<proto::MetricFamily> {
        let Ok(proxied) = PROXIED.lock() else {
            return Vec::new();
        };
        proxied.values().flatten().cloned().collect()
    }
}

/// Register the proxy. Called from `init` for the orchestrator alone.
///
/// A failure here is logged rather than ignored: the only way to fail is a second desc-less
/// collector, and the symptom of ignoring it is a `/metrics` that quietly carries no room series
/// while every other part of the pipeline looks healthy.
pub(super) fn register(registry: &prometheus::Registry) {
    if let Err(e) = registry.register(Box::new(RoomMetrics)) {
        tracing::warn!(
            error = %e,
            "the room metrics proxy did not register; no room's own metrics will be re-exported"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A room's exposition, in the shape pahoa's handoff quotes.
    fn exposition() -> &'static str {
        concat!(
            "# HELP pahoa_packets_in_total Packets received from a slot\n",
            "# TYPE pahoa_packets_in_total counter\n",
            "pahoa_packets_in_total{team=\"0\",slot=\"1\",player=\"MooingLemurSMS\",\
             game=\"Super Mario Sunshine\",cmd=\"Bounce\"} 40\n",
            "# HELP pahoa_packets_preauth_total Packets that arrived before a slot was known\n",
            "# TYPE pahoa_packets_preauth_total counter\n",
            "pahoa_packets_preauth_total{cmd=\"Connect\"} 1\n",
            "# HELP pahoa_clients_connected Sockets currently open\n",
            "# TYPE pahoa_clients_connected gauge\n",
            "pahoa_clients_connected 3\n",
        )
    }

    fn parse(text: &str) -> prometheus_parse::Scrape {
        prometheus_parse::Scrape::parse(text.lines().map(|l| Ok(l.to_string()))).expect("parses")
    }

    fn label(metric: &proto::Metric, name: &str) -> Option<String> {
        metric
            .get_label()
            .iter()
            .find(|l| l.name() == name)
            .map(|l| l.value().to_string())
    }

    #[test]
    fn a_rooms_series_gain_its_room_and_keep_everything_else() {
        let (families, dropped) = convert("room-a", &parse(exposition()));
        assert_eq!(dropped.iter().map(|(_, n)| n).sum::<usize>(), 0);
        assert_eq!(families.len(), 3, "one family per name");

        let packets = families
            .iter()
            .find(|f| f.name() == "pahoa_packets_in_total")
            .expect("the counter is re-exported");
        assert_eq!(packets.get_field_type(), proto::MetricType::COUNTER);
        assert_eq!(
            packets.help(),
            "Packets received from a slot",
            "pahoa's help text carries through verbatim"
        );

        let metric = &packets.get_metric()[0];
        assert_eq!(label(metric, "room").as_deref(), Some("room-a"));
        // Every label the room sent, untouched, including `team`, which pahoa added ahead of the
        // first scrape precisely so a dashboard would not have to be rebuilt for it.
        assert_eq!(label(metric, "team").as_deref(), Some("0"));
        assert_eq!(label(metric, "slot").as_deref(), Some("1"));
        assert_eq!(
            label(metric, "game").as_deref(),
            Some("Super Mario Sunshine")
        );
        assert_eq!(metric.get_counter().get_value(), 40.0);

        let gauge = families
            .iter()
            .find(|f| f.name() == "pahoa_clients_connected")
            .expect("the gauge is re-exported");
        assert_eq!(gauge.get_field_type(), proto::MetricType::GAUGE);
        assert_eq!(gauge.get_metric()[0].get_gauge().get_value(), 3.0);
        // A sample with no labels of its own still gets one.
        assert_eq!(
            label(&gauge.get_metric()[0], "room").as_deref(),
            Some("room-a")
        );
    }

    /// A room cannot claim to be this process.
    ///
    /// `gather` merges by name, so without this a room exporting `puna_rooms` would have its
    /// metrics folded into the family the orchestrator computes — a series indistinguishable from
    /// one Puna made, asserting whatever the room felt like.
    #[test]
    fn a_room_cannot_publish_under_a_name_puna_owns() {
        let text = concat!(
            "# TYPE puna_rooms gauge\n",
            "puna_rooms{state=\"running\"} 9999\n",
            "# TYPE pahoa_packets_in_total counter\n",
            "pahoa_packets_in_total{cmd=\"Sync\"} 2\n",
        );
        let (families, dropped) = convert("room-a", &parse(text));

        assert!(
            families.iter().all(|f| f.name() != "puna_rooms"),
            "a room's `puna_rooms` must not be re-exported"
        );
        assert_eq!(
            dropped
                .iter()
                .find(|(r, _)| *r == "name_collision")
                .map(|(_, n)| *n),
            Some(1)
        );
        // And the rest of the document still lands: one bad family does not cost the room its
        // real ones.
        assert_eq!(families.len(), 1);
    }

    /// Half a histogram reads as a working metric, so none of it is published.
    #[test]
    fn a_histogram_is_refused_whole_rather_than_published_in_pieces() {
        let text = concat!(
            "# TYPE pahoa_save_seconds histogram\n",
            "pahoa_save_seconds_bucket{le=\"0.1\"} 1\n",
            "pahoa_save_seconds_bucket{le=\"+Inf\"} 4\n",
            "pahoa_save_seconds_sum 0.42\n",
            "pahoa_save_seconds_count 4\n",
            "# TYPE pahoa_packets_in_total counter\n",
            "pahoa_packets_in_total{cmd=\"Sync\"} 2\n",
        );
        let (families, dropped) = convert("room-a", &parse(text));

        let names: Vec<&str> = families.iter().map(|f| f.name()).collect();
        assert_eq!(
            names,
            vec!["pahoa_packets_in_total"],
            "neither the histogram nor its _sum/_count companions may be published"
        );
        assert!(
            dropped
                .iter()
                .any(|(r, n)| *r == "unsupported_type" && *n > 0)
        );
    }

    /// Two label pairs with one name is an invalid metric, so ours replaces theirs.
    #[test]
    fn a_room_label_from_the_room_is_replaced_not_duplicated() {
        let text = concat!(
            "# TYPE pahoa_packets_in_total counter\n",
            "pahoa_packets_in_total{room=\"somebody-else\",cmd=\"Sync\"} 2\n",
        );
        let (families, dropped) = convert("room-a", &parse(text));

        let metric = &families[0].get_metric()[0];
        assert_eq!(
            metric
                .get_label()
                .iter()
                .filter(|l| l.name() == "room")
                .count(),
            1,
            "exactly one room label"
        );
        assert_eq!(label(metric, "room").as_deref(), Some("room-a"));
        assert_eq!(
            dropped
                .iter()
                .find(|(r, _)| *r == "duplicate_room_label")
                .map(|(_, n)| *n),
            Some(1)
        );
    }

    /// The reason vocabulary is written down once. A reason `convert` can produce and `init` does
    /// not seed renders as an absence until the first drop, which is the ambiguity the seeding
    /// exists to remove.
    #[test]
    fn every_drop_reason_convert_can_report_is_declared() {
        let text = concat!(
            "# TYPE puna_rooms gauge\n",
            "puna_rooms 1\n",
            "# TYPE pahoa_x histogram\n",
            "pahoa_x_bucket{le=\"+Inf\"} 1\n",
            "# TYPE pahoa_y counter\n",
            "pahoa_y{room=\"other\"} 1\n",
        );
        let (_, dropped) = convert("room-a", &parse(text));
        for (reason, _) in dropped {
            assert!(
                DROP_REASONS.contains(&reason),
                "{reason} is reported but not declared"
            );
        }
        assert_eq!(dropped.len(), DROP_REASONS.len());
    }
}
