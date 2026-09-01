//! `puna-web` exports its own families and nobody else's.
//!
//! ## Why this is an integration test rather than a unit test
//!
//! The registry is a process-global, and every `#[test]` in a lib's unit-test module shares one
//! binary, so a unit test asserting "the web tier does not export `puna_ports_bound`" would pass
//! or fail depending on whether some other test had already forced that family. It would be a test
//! of execution order.
//!
//! Each file under `tests/` compiles to its OWN binary with its own statics, so this one observes
//! a registry that nothing else has touched. That is what makes the negative assertion mean
//! something. There is one file per component for exactly that reason; they cannot be merged.
//!
//! ## What it is defending against
//!
//! Registering every family in every process is invisible while only one tier is scraped, and
//! `puna-orchestrator` was the only scraped tier until the web and tracker metrics listeners
//! landed. Then `puna_ports_bound / puna_ports_capacity` became seven series where one is
//! meaningful, and the alert survived only because the web tier's `0/0` renders as NaN.
//!
//! An alert should not depend on which tiers happen to publish a zero.

mod common;

use puna_core::metrics::{self, Component};

#[test]
fn the_web_tier_exports_shared_families_and_nothing_it_cannot_compute() {
    metrics::init(Component::Web);
    let rendered = common::rendered_families(&metrics::gather());

    // `seeded_`, not `families`: a registered *Vec with no series renders no `# TYPE` line, so
    // the visible set is the registered set minus DEFERRED_FAMILIES. For this tier that leaves
    // nothing at all on a cold process: `diesel_query_seconds` appears once readiness runs its
    // first query.
    let mut expected: Vec<String> = metrics::seeded_families(Component::Web)
        .into_iter()
        .map(String::from)
        .collect();
    expected.sort();

    assert_eq!(
        rendered, expected,
        "puna-web's registry does not match families(Component::Web): a family was forced \
         without being listed, or listed without being forced"
    );

    // Stated separately from the equality above, because this is the property that actually
    // broke and the failure message should say so rather than dumping two sorted lists.
    for family in metrics::ORCHESTRATOR_FAMILIES {
        assert!(
            !rendered.iter().any(|r| r == family),
            "puna-web exports {family}, which only the orchestrator can compute. It would \
             publish a permanent zero, and any alert summing or dividing that family would have \
             to know to exclude this tier."
        );
    }
}
