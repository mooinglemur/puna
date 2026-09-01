//! `puna-orchestrator` exports every family it is supposed to, and no more.
//!
//! Its own binary, for the reason in `metrics_scope_web.rs`.
//!
//! This is the direction that catches the opposite mistake from the other two files. Theirs fail
//! when a family leaks into a tier that cannot compute it; this one fails when a family is
//! declared and listed but never actually forced, which does not break a scrape, it just means
//! the series is absent until something touches it. That is the ambiguity `init` exists to remove:
//! `puna_integrity_faults` reading 0 is reassuring, and reading "no data" is not.

mod common;

use puna_core::metrics::{self, Component};

#[test]
fn the_orchestrator_exports_exactly_the_families_it_owns() {
    metrics::init(Component::Orchestrator);
    let rendered = common::rendered_families(&metrics::gather());

    // `seeded_`, not `families`: a registered *Vec with no series renders no `# TYPE` line, so the
    // combinatorial counters in DEFERRED_FAMILIES are legitimately absent until something writes
    // one. Comparing against the full registered set is what this test did first, and it failed,
    // which is how DEFERRED_FAMILIES came to be written down rather than left in a doc comment.
    let mut expected: Vec<String> = metrics::seeded_families(Component::Orchestrator)
        .into_iter()
        .map(String::from)
        .collect();
    expected.sort();

    assert_eq!(
        rendered, expected,
        "the orchestrator's registry does not match families(Component::Orchestrator) -- a \
         family was forced without being listed, or listed without being forced"
    );
}

/// The tables have to partition the registry, not merely cover it.
///
/// A name in two lists would make `families()` report a component exporting something it does
/// not, which is the same class of wrongness this whole change is fixing, just in the
/// documentation rather than in the process.
#[test]
fn the_family_tables_are_disjoint_and_free_of_duplicates() {
    let all: Vec<&str> = metrics::SHARED_FAMILIES
        .iter()
        .chain(metrics::WEB_FAMILIES)
        .chain(metrics::TRACKER_FAMILIES)
        .chain(metrics::ORCHESTRATOR_FAMILIES)
        .copied()
        .collect();

    let mut seen = std::collections::BTreeSet::new();
    for name in &all {
        assert!(
            seen.insert(*name),
            "{name} appears in more than one family table, or twice in one"
        );
    }
}
