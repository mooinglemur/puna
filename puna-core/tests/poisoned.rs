//! A real malicious upload, and the bounds that refuse it.
//!
//! `fixtures/poisoned_multidata.archipelago` is not synthetic. It is a file that was uploaded to an
//! Archipelago host and crashed the MultiServer behind it, kept here because **the shape of an
//! attack is worth more than a description of one**: every number below was measured against this
//! file rather than argued from the format.
//!
//! ## What it is
//!
//! A hand-built minimal multidata, not a generated seed: one slot named `P` playing a game called
//! `G`, one location, no slot data, and a `precollected_items` entry for slot 1 holding **11,422,785
//! copies of the integer 0**. The payload is pure repetition, so it compresses to 38,559 bytes and
//! inflates to 22,871,873 of pickle, a ratio of 593:1 against the 2.9:1 to 4.6:1 that real seeds
//! manage.
//!
//! ## What it cost, before this
//!
//! `MultiData::parse` on it peaked at **732 MiB of resident memory over three seconds**, measured
//! on Puna's own pinned parser. `puna-web`'s container limit is 1Gi and its request is 256Mi, and
//! the same tier re-zips patches in memory. One upload is close; two are not a question. Upstream
//! reportedly paid about a gigabyte per ROOM for the same file, because every start-inventory code
//! becomes an object the room holds for its whole life.
//!
//! And the file is 38 KB against a 256 MiB upload limit, so nothing about it was near a ceiling.
//!
//! ## Safety of this test
//!
//! It parses the file **only through the bounded door**, which refuses it after inflating 771 KB.
//! Nothing here calls `MultiData::parse` directly, deliberately: a test that demonstrated the
//! unbounded path by running it would allocate three quarters of a gigabyte in CI to prove a point
//! already recorded above.

use puna_core::artifact::{self, IngestError};

const POISONED: &[u8] = include_bytes!("fixtures/poisoned_multidata.archipelago");

/// **The bomb is refused, and refused early.**
///
/// The ratio bound is what makes "early" true: 38,559 bytes may inflate to 771,180 before Puna
/// stops reading, which is 3% of the pickle and 0.1% of what parsing it used to cost. The absolute
/// cap would also refuse this file, at 32 MiB of inflate; both are checked, because they fail
/// differently and only one of them is cheap.
#[test]
fn the_poisoned_multidata_is_refused_before_it_can_be_parsed() {
    // Destructured rather than `expect_err`, which would print the value on failure: the value is
    // eleven million integers, so the failure this test exists to report used to arrive as 32 MB of
    // `Debug` output. A test that succeeds loudly and fails illegibly is half a test.
    let Err(err) = artifact::parse_multidata(POISONED) else {
        panic!("the poisoned seed parsed; the bound is gone and an upload can cost 732 MiB again");
    };

    match err {
        IngestError::MultidataTooLarge { limit, compressed } => {
            assert_eq!(compressed, POISONED.len());
            assert_eq!(
                limit,
                (POISONED.len() * artifact::MAX_MULTIDATA_RATIO) as u64,
                "the ratio bound is not what stopped it, so a 38 KB file was allowed to inflate to \
                 the absolute cap before anybody looked"
            );
            // The number that matters: what was actually allocated before the refusal.
            assert!(
                limit < 1024 * 1024,
                "the refusal came after inflating {limit} bytes, which is not early"
            );
        }
        other => panic!("refused for the wrong reason: {other}"),
    }
}

/// **Both bounds are load-bearing, and the ratio one is not sufficient by itself.**
///
/// An attacker willing to upload a megabyte can stay under 20:1 and still ask for far more than any
/// seed needs, which is what the absolute cap answers. Stated as arithmetic over the constants
/// rather than by building a second bomb: the property is that the two are combined with a `min`,
/// so neither alone decides.
#[test]
fn the_two_bounds_are_a_floor_of_each_other_rather_than_alternatives() {
    // A small file: the ratio binds.
    let small = 38_559usize;
    assert!(
        small * artifact::MAX_MULTIDATA_RATIO < artifact::MAX_MULTIDATA_BYTES,
        "the ratio no longer binds on a small upload, so the cheap bomb inflates to the cap"
    );

    // A large one: the absolute cap binds, or a 16 MiB upload could ask for 320 MiB.
    let large = 16 * 1024 * 1024usize;
    assert!(
        large * artifact::MAX_MULTIDATA_RATIO > artifact::MAX_MULTIDATA_BYTES,
        "the absolute cap no longer binds on a large upload, so the ratio is the only limit and it \
         scales with whatever somebody is willing to send"
    );
}

/// **The count bound, which is what the byte bounds cannot express.**
///
/// An integer costs two bytes of pickle, so a multidata comfortably inside 32 MiB can still declare
/// sixteen million pre-collected items. Bytes are the wrong unit for the thing that actually hurt
/// the upstream server: eleven million items became eleven million objects a room held forever.
///
/// Mutated from a real parsed seed, the idiom `tests/ingest.rs` uses for the same reason: everything
/// except the one broken fact is a working multiworld, so a refusal can only be the mutation. And it
/// goes through `load_refusal` rather than the private check, because `load_refusal` is what both
/// call sites run: the upload, and the re-check when a room is opened from a seed already stored.
#[test]
fn an_absurd_start_inventory_is_refused_even_inside_the_byte_bounds() {
    let Ok(path) = std::env::var("PUNA_TEST_GENERATION_ZIP") else {
        eprintln!("skipping: set PUNA_TEST_GENERATION_ZIP to a real generation zip");
        return;
    };
    let bytes = std::fs::read(&path).expect("the fixture zip");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes[..])).expect("a zip");
    let name = (0..archive.len())
        .map(|i| archive.by_index(i).expect("a member").name().to_string())
        .find(|n| n.to_ascii_lowercase().ends_with(".archipelago"))
        .expect("a multidata member");
    let raw = {
        use std::io::Read;
        let mut file = archive.by_name(&name).expect("the multidata");
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).expect("read");
        buf
    };
    let seed = artifact::parse_multidata(&raw).expect("a real seed parses");

    assert!(
        artifact::load_refusal(&seed).is_none(),
        "the unmutated seed is refused, so the assertion below would prove nothing"
    );

    let mut bomb = seed.clone();
    let slot = *bomb
        .precollected_items
        .keys()
        .next()
        .unwrap_or(&1)
        .min(&1_000_000);
    bomb.precollected_items
        .insert(slot, vec![0i64; artifact::MAX_PRECOLLECTED_ITEMS + 1]);

    let refusal = artifact::load_refusal(&bomb).expect("an absurd start inventory must be refused");
    assert!(
        refusal.contains("pre-collected"),
        "the refusal does not say what is wrong with the seed: {refusal}"
    );
}

// --- PUNA REFUSES NO LATER THAN THE ROOM DOES ----------------------------------------------------
//
// Two sides now carry limits on the same untrusted file, which is the arrangement that was asked
// for and the one that rots without saying anything. The rule is one-directional: Puna is the edge
// and may be as strict as it likes, but a seed Puna ACCEPTS must be one the room can load, or the
// refusal moves from a sentence on an upload form to a pod that exits at startup with the reason in
// a container log. That is the failure this whole path exists to prevent.
//
// **Compile-time, not a `#[test]`**, which is the form pahoa used for the same class of invariant on
// their side: these compare constants, so a pin bump that lowers one of theirs should fail the
// BUILD rather than a test run somebody might not have got to yet. They are also read at the pinned
// rev rather than transcribed, which is what makes them worth having public.
const _: () = assert!(
    artifact::MAX_MULTIDATA_BYTES as u64 <= pahoa_multidata::MAX_PICKLE_BYTES,
    "Puna would inflate a multidata that pahoa refuses to parse, so a seed could pass the upload \
     form and fail at room start"
);
const _: () = assert!(
    artifact::MAX_PRECOLLECTED_ITEMS <= pahoa_multidata::MAX_PRECOLLECTED_ITEMS,
    "Puna would accept a start inventory a room refuses"
);
// **The object budget has no Puna-side equivalent and cannot have one**: it counts what the parser
// builds, which is knowable only inside the parser. Held here anyway, because it is the tightest of
// the three by a wide margin and the only one a legitimate seed can reach.
//
// The floor is **Puna's own measurement, not pahoa's**: `make-generation --slots 3000 --locations
// 250` produces a seed of 3,857,136 opcodes, which clears the 4,000,000 budget by 1.04x. A
// 2000-slot sync, which this fleet has already load-tested, sits at 2,572,136 and 1.56x. So the
// budget binds somewhere between three and four thousand slots, and Puna's own tooling can build
// the seed that meets it. That is reported back rather than worked around; see the handoff.
const _: () = assert!(
    pahoa_pickle::MAX_OBJECTS >= 3_857_136,
    "pahoa's object budget no longer clears a 3000-slot seed this repository's own generator can \
     produce, so a legitimate multiworld would be refused at upload"
);

/// **A real seed still passes, which is the half that a bound gets wrong quietly.**
///
/// Skips without the fixture, like every other seed-gated test here: a bound that refused real
/// seeds would be found by somebody's upload failing, so it is worth asserting from the same corpus
/// the caps were measured against rather than only from the attack.
#[test]
fn a_real_seed_is_still_read() {
    let Ok(path) = std::env::var("PUNA_TEST_GENERATION_ZIP") else {
        eprintln!("skipping: set PUNA_TEST_GENERATION_ZIP to a real generation zip");
        return;
    };
    let bytes = std::fs::read(&path).expect("the fixture zip");
    let meta = artifact::inspect(&bytes, 256 * 1024 * 1024).expect("a real seed must still ingest");
    assert!(!meta.slots.is_empty(), "a real seed parsed to no slots");
}
