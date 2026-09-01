//! `puna-tracker` exports its own families and nobody else's.
//!
//! Its own binary, for the reason in `metrics_scope_web.rs`: the registry is a process-global, so
//! a negative assertion is only meaningful in a process nothing else has touched.
//!
//! Separate from the web tier's even though the two register the same (empty) set today. They are
//! the same binary under different `PUNA_ROLE` values but they are not the same component, and the
//! moment one of them gains a family this test is what catches it being added to the wrong list.

mod common;

use puna_core::metrics::{self, Component};

#[test]
fn the_tracker_tier_exports_shared_families_and_nothing_it_cannot_compute() {
    metrics::init(Component::Tracker);
    let rendered = common::rendered_families(&metrics::gather());

    // `seeded_`, not `families`: a registered *Vec with no series renders no `# TYPE` line, so
    // the visible set is the registered set minus DEFERRED_FAMILIES. For this tier that leaves
    // nothing at all on a cold process: `diesel_query_seconds` appears once readiness runs its
    // first query.
    let mut expected: Vec<String> = metrics::seeded_families(Component::Tracker)
        .into_iter()
        .map(String::from)
        .collect();
    expected.sort();

    assert_eq!(
        rendered, expected,
        "puna-tracker's registry does not match families(Component::Tracker) -- a family was \
         forced without being listed, or listed without being forced"
    );

    for family in metrics::ORCHESTRATOR_FAMILIES {
        assert!(
            !rendered.iter().any(|r| r == family),
            "puna-tracker exports {family}, which only the orchestrator can compute"
        );
    }
}
