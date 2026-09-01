//! The room metrics proxy, from a room's exposition through to the orchestrator's `/metrics`.
//!
//! Its own binary because it publishes into the process-global registry, which is exactly what
//! `metrics_scope_orchestrator.rs` asserts the *absence* of: that file checks a cold process
//! renders precisely the families Puna owns, and a proxied series arriving in the same binary would
//! break it for the right reason at the wrong time.
//!
//! The unit tests in `metrics::proxy` cover the conversion. What only this level can reach is the
//! part that has to be true for a scrape to be *ingestible*: one `# HELP` line per family name
//! however many rooms contributed to it.

mod common;

use std::sync::{Mutex, MutexGuard};

use puna_core::metrics::{self, Component};

/// These tests share one registry and one proxy map, so they are serialized. Same rule as the
/// metrics tests in the library: a read is a read of state another test is part-way through
/// writing, and the failure is a flake that reads as the publisher being broken.
static EXCLUSIVE: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    match EXCLUSIVE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Does the scrape carry this room's **proxied** series, rather than merely mentioning the room?
///
/// The distinction is the whole reason this helper exists. `puna_room_metrics_series{room="…"}` is
/// Puna's own gauge and renders whatever the proxy does, so a bare search for `room="…"` passes
/// with the collector unregistered and nothing re-exported at all, which is what mutating
/// `proxy::register` out proved before this was written.
fn carries_proxied_series(rendered: &str, room: &str) -> bool {
    rendered.lines().any(|line| {
        line.starts_with("pahoa_packets_in_total") && line.contains(&format!(r#"room="{room}""#))
    })
}

fn exposition(cmd: &str, value: u32) -> String {
    format!(
        "# HELP pahoa_packets_in_total Packets received from a slot\n\
         # TYPE pahoa_packets_in_total counter\n\
         pahoa_packets_in_total{{team=\"0\",slot=\"1\",player=\"Troy\",game=\"Yacht Dice\",\
         cmd=\"{cmd}\"}} {value}\n"
    )
}

/// The whole feature in one assertion: a room's own counter, under its own name and help, carrying
/// the room it came from.
#[test]
fn a_rooms_metrics_reach_the_orchestrators_scrape() {
    let _guard = exclusive();
    metrics::init(Component::Orchestrator);
    metrics::proxy::forget("room-a");

    let published = metrics::proxy::publish("room-a", &exposition("Bounce", 40));
    assert_eq!(published.series, 1);
    assert_eq!(published.dropped, 0);

    let rendered = metrics::gather();
    assert!(
        rendered.contains("# HELP pahoa_packets_in_total Packets received from a slot"),
        "pahoa's own help text should reach the scrape:\n{rendered}"
    );
    assert!(
        rendered.contains("# TYPE pahoa_packets_in_total counter"),
        "and its type"
    );
    assert!(
        carries_proxied_series(&rendered, "room-a"),
        "the re-exported series must name the room it came from:\n{rendered}"
    );
    assert!(
        rendered.contains(r#"cmd="Bounce""#) && rendered.contains(r#"team="0""#),
        "every label the room sent is kept"
    );

    metrics::proxy::forget("room-a");
}

/// **One `# HELP` per name, whatever the fleet size**, and this is the assertion that matters
/// most, because getting it wrong does not lose a series, it makes the whole scrape unparseable at
/// the far end. A second `# HELP` line for one metric name is an error to Prometheus, so two
/// hundred rooms exporting the same counter have to render as one family.
///
/// It holds because `Registry::gather` merges families by name across everything a collector
/// returns. That is behavior this code depends on and does not implement, which is the reason to
/// pin it here rather than trust it.
#[test]
fn many_rooms_render_as_one_family() {
    let _guard = exclusive();
    metrics::init(Component::Orchestrator);

    for room in ["room-a", "room-b", "room-c"] {
        metrics::proxy::publish(room, &exposition("Sync", 7));
    }

    let rendered = metrics::gather();
    assert_eq!(
        rendered.matches("# HELP pahoa_packets_in_total").count(),
        1,
        "a second HELP line for one name makes the scrape unparseable:\n{rendered}"
    );
    assert_eq!(rendered.matches("# TYPE pahoa_packets_in_total").count(), 1);
    for room in ["room-a", "room-b", "room-c"] {
        assert!(
            carries_proxied_series(&rendered, room),
            "{room} is missing from the merged family"
        );
    }

    for room in ["room-a", "room-b", "room-c"] {
        metrics::proxy::forget(room);
    }
}

/// A room that stops being live takes **every** series it had, not one stale reading.
///
/// The gauges lose one series per room; these are keyed by `(room, slot, cmd, …)`, so a room left
/// behind by `retain_rooms` strands its whole label space, and every one of those series would go
/// on asserting a counter that stopped moving, which reads as a room gone quiet rather than a room
/// that is gone.
#[test]
fn a_room_that_is_no_longer_live_leaves_nothing_behind() {
    let _guard = exclusive();
    metrics::init(Component::Orchestrator);

    metrics::proxy::publish("room-gone", &exposition("Sync", 3));
    metrics::proxy::publish("room-here", &exposition("Sync", 4));
    assert!(
        carries_proxied_series(&metrics::gather(), "room-gone"),
        "the precondition: it has to be published before its removal means anything"
    );

    let live: std::collections::HashSet<String> = ["room-here".to_string()].into_iter().collect();
    metrics::retain_rooms(&live);

    let rendered = metrics::gather();
    assert!(
        !rendered.contains(r#"room="room-gone""#),
        "a room that is no longer live must not keep re-exporting:\n{rendered}"
    );
    assert!(
        carries_proxied_series(&rendered, "room-here"),
        "and a live one must be untouched"
    );
    assert!(
        !rendered.contains("puna_room_metrics_series{room=\"room-gone\"}"),
        "its cardinality gauge goes with it"
    );

    metrics::proxy::forget("room-here");
}

/// **The families being proxied are deliberately not in Puna's tables, and cannot be.**
///
/// `ORCHESTRATOR_FAMILIES` is an exact list that `metrics_scope_orchestrator.rs` holds the registry
/// to, which works precisely because every name in it is one Puna chose. A proxied name is
/// pahoa's, arrives at runtime, and changes when they add a metric; listing it would mean a Puna
/// release for every metric they ship, which is the coupling the exposition-format handoff exists
/// to remove.
///
/// So the tables carry the proxy's own bookkeeping and nothing it carries. What stands in for the
/// scope test here is the collision guard: a room cannot publish under a name Puna owns.
#[test]
fn the_proxied_names_are_pahoas_and_the_tables_stay_punas() {
    let _guard = exclusive();
    metrics::init(Component::Orchestrator);

    let declared = metrics::families(Component::Orchestrator);
    assert!(
        declared.contains(&"puna_room_metrics_series")
            && declared.contains(&"puna_room_metrics_dropped_total"),
        "the proxy's own bookkeeping is Puna's and belongs in the table"
    );
    assert!(
        declared
            .iter()
            .all(|name| name.starts_with("puna_") || metrics::SHARED_FAMILIES.contains(name)),
        "a family table should only ever hold names this process computes"
    );

    // A room claiming a Puna name is refused rather than merged into the real family, beside a
    // legitimate family from the same document, which is the positive control: without it this
    // assertion also passes when nothing is being re-exported at all.
    let hostile = format!(
        "# TYPE puna_rooms gauge\npuna_rooms{{state=\"running\"}} 9999\n{}",
        exposition("Sync", 5)
    );
    metrics::proxy::publish("room-hostile", &hostile);
    let rendered = metrics::gather();
    assert!(
        carries_proxied_series(&rendered, "room-hostile"),
        "the rest of the document must still land, or this proves nothing:\n{rendered}"
    );
    assert!(
        !rendered.contains("9999"),
        "a room must not be able to publish under a name Puna owns:\n{rendered}"
    );

    metrics::proxy::forget("room-hostile");
}

/// A room whose document cannot be read costs that room its series and nothing else.
///
/// The property is that the orchestrator's own `/metrics` stays well formed: it is the singleton,
/// its scrape is what an incident is diagnosed from, and a room is the one input to it that Puna
/// does not write.
#[test]
fn a_malformed_document_cannot_reach_the_scrape() {
    let _guard = exclusive();
    metrics::init(Component::Orchestrator);

    metrics::proxy::publish("room-a", "this is not an exposition at all\n\0\0}{");
    let rendered = metrics::gather();

    assert!(!rendered.contains("this is not an exposition"));
    assert!(
        rendered.contains("# TYPE puna_rooms gauge"),
        "Puna's own families must be untouched:\n{rendered}"
    );
    // And the rendered document still parses as families, which is the shape a scrape needs.
    assert!(
        common::rendered_families(&rendered).contains(&"puna_rooms".to_string()),
        "the exposition should still be readable"
    );

    metrics::proxy::forget("room-a");
}
