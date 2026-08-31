//! Building a multiworld seed: the `PyObj` tree, and the zip Puna ingests.
//!
//! The shape here was transcribed from a real generation (`AP_51201905219311909307`) by unpickling
//! it, not inferred from Archipelago's source — so what this writes is what a generator writes,
//! and the two class instances it contains are the only two `pahoa_pickle`'s allowlist permits.
//!
//! ## The pool is dealt, not scattered
//!
//! Every slot contributes exactly as many items as it has locations: `n - 1` ordinary ones drawn
//! **with replacement**, plus one [`GOAL_ITEM`] addressed to itself. All contributions are pooled
//! across the multiworld, shuffled, and dealt one per location.
//!
//! That is the real algorithm rather than an imitation of it, and doing it properly is what makes
//! the Goal placement fall out instead of being special-cased: each slot's Goal lands wherever the
//! shuffle puts it — its own world or anybody else's — exactly one exists per slot, and no
//! placement can be left over or short. Drawing ordinary items with replacement is likewise the
//! normal shape of an item pool: Super Mario 64 ships 120 `Power Star`s.
//!
//! ## `release_mode: "auto"`, which is what lets a room finish
//!
//! A slot that goals stops checking, so its remaining locations would strand every item in them —
//! including, potentially, another slot's Goal. Auto-release empties that world on goal instead, so
//! goals cascade: nobody has goaled at the start so everybody is checking, the first Goal found
//! triggers a release, and inductively every Goal reaches its owner. Without it a load run can
//! deadlock with two slots each holding the other's Goal behind a location neither will check.

use anyhow::{Context, Result, bail};
use pahoa_pickle::{ClassId, PyObj};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use std::io::Write;

use crate::pickle;
use crate::words::{self, GOAL_ITEM};

/// Where synthetic ids start.
///
/// **Deliberately nowhere near a real Archipelago range.** Worlds are allocated blocks in the
/// low tens of billions (the corpus seed uses `16871244000`); nine trillion is somewhere no
/// generator hands out, so a synthetic id in a database, a log line or a metric label is
/// identifiable as synthetic on sight rather than by checking which seed it came from.
const ID_BASE: i64 = 9_100_000_000_000;

/// Ids per game: items from the block's start, locations from its middle.
const GAME_STRIDE: i64 = 1_000_000;
const LOCATION_OFFSET: i64 = 500_000;

/// Archipelago's `SlotType`. `player` is the only kind that owns locations; `group` is item-link
/// machinery this does not generate.
const SLOT_TYPE_SPECTATOR: i64 = 0b00;
const SLOT_TYPE_PLAYER: i64 = 0b01;

/// Progression, in Archipelago's item flags. Ordinary filler is 0.
const FLAG_PROGRESSION: i64 = 0b001;

/// Roughly what fraction of ordinary items are marked progression, so a tracker's progression
/// filter has something to filter.
const PROGRESSION_SHARE: f64 = 0.25;

/// The generator version claimed in the seed.
///
/// Must be at or past Archipelago's `LEGACY_GENERATOR_CUTOFF` (0.6.2), below which pahoa applies a
/// much older client floor — a synthetic seed claiming to be ancient would quietly accept clients
/// this tool would never send.
///
/// **0.6.7, because that is the newest version upstream has actually released.** A synthetic seed
/// is meant to be indistinguishable from a real one everywhere it can cheaply be, and a generator
/// stamp naming an unreleased version is the sort of detail that makes somebody doubt a whole
/// fixture. It moves to 0.6.8 when upstream ships it.
const GENERATOR_VERSION: (i64, i64, i64) = (0, 6, 7);

/// The oldest server that may host this seed. `validate` refuses a room older than it.
const MINIMUM_SERVER_VERSION: (i64, i64, i64) = (0, 5, 0);

/// What to build.
#[derive(Debug, Clone)]
pub struct Spec {
    /// Slots that play: own locations, contribute items, need a Goal.
    pub players: usize,
    /// Slots that connect and watch. They own no locations and contribute nothing, which is why
    /// their `locations` entry is the empty dict the contiguity rule requires.
    pub spectators: usize,
    /// How many distinct games the players are spread across, round-robin.
    pub games: usize,
    /// Checks per player slot — and, minus one, the size of each game's ordinary item table.
    pub locations: usize,
    pub seed: u64,
}

/// What was built, for the operator to read back.
#[derive(Debug, Clone)]
pub struct Summary {
    pub seed_name: String,
    pub players: usize,
    pub spectators: usize,
    pub games: Vec<String>,
    pub locations_per_slot: usize,
    pub total_checks: usize,
    pub bytes: usize,
    pub seed: u64,
}

/// One game's name tables, shared by every slot playing it.
struct Game {
    name: String,
    /// Ordinary item names to ids. Excludes the Goal.
    items: Vec<(String, i64)>,
    goal_id: i64,
    locations: Vec<(String, i64)>,
}

/// Build the seed and return the zip Puna ingests.
pub fn build(spec: &Spec) -> Result<(Vec<u8>, Summary)> {
    if spec.players == 0 {
        bail!("a seed needs at least one player slot");
    }
    if spec.locations == 0 {
        bail!("a slot with no locations has nowhere to put its Goal item");
    }
    if spec.games == 0 {
        bail!("a seed needs at least one game");
    }

    let mut rng = StdRng::seed_from_u64(spec.seed);
    let games = build_games(spec, &mut rng);

    // Round-robin, so a slot count that is not a multiple of the game count still spreads evenly.
    let game_of = |player: usize| player % games.len();

    let handles = words::handles(spec.players + spec.spectators, &mut rng);
    let seed_name = format!("{:020}", rng.r#gen::<u64>());

    let placements = deal(spec, &games, &game_of, &mut rng);
    let multidata = multidata(spec, &games, &game_of, &handles, &seed_name, &placements);

    check_invariants(spec, &games, &game_of, &placements)?;

    let zip = package(&seed_name, &multidata, spec, &games, &game_of, &handles)?;
    let summary = Summary {
        seed_name,
        players: spec.players,
        spectators: spec.spectators,
        games: games.iter().map(|g| g.name.clone()).collect(),
        locations_per_slot: spec.locations,
        total_checks: spec.players * spec.locations,
        bytes: zip.len(),
        seed: spec.seed,
    };
    Ok((zip, summary))
}

fn build_games(spec: &Spec, rng: &mut impl Rng) -> Vec<Game> {
    let mut names: Vec<String> = words::GAMES.iter().map(|s| (*s).to_string()).collect();
    names.shuffle(rng);
    for i in names.len()..spec.games {
        names.push(format!(
            "{} {}",
            words::GAMES[i % words::GAMES.len()],
            i / words::GAMES.len() + 1
        ));
    }
    names.truncate(spec.games);

    names
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let base = ID_BASE + index as i64 * GAME_STRIDE;
            // `locations - 1` ordinary names, because the Goal occupies the last slot in the item
            // table. Items and locations are the same count by construction, which is upstream's
            // own invariant for a well-behaved apworld: the pool exactly fills the world.
            let items = words::items(spec.locations - 1, rng)
                .into_iter()
                .enumerate()
                .map(|(i, name)| (name, base + i as i64))
                .collect::<Vec<_>>();
            let goal_id = base + (spec.locations - 1) as i64;
            let locations = words::locations(spec.locations, rng)
                .into_iter()
                .enumerate()
                .map(|(i, name)| (name, base + LOCATION_OFFSET + i as i64))
                .collect();
            Game {
                name,
                items,
                goal_id,
                locations,
            }
        })
        .collect()
}

/// One item, addressed to whoever contributed it.
#[derive(Clone, Copy)]
struct Placed {
    item: i64,
    receiver: u32,
    flags: i64,
}

/// Pool every slot's contribution, shuffle, and deal one item per location in the multiworld.
fn deal(
    spec: &Spec,
    games: &[Game],
    game_of: &impl Fn(usize) -> usize,
    rng: &mut impl Rng,
) -> Vec<Vec<Placed>> {
    let mut pool: Vec<Placed> = Vec::with_capacity(spec.players * spec.locations);
    for player in 0..spec.players {
        let slot = player as u32 + 1;
        let game = &games[game_of(player)];
        for _ in 0..(spec.locations - 1) {
            // With replacement: an item pool repeats, and a world that handed out each of its
            // items exactly once would be a shape no real generation produces.
            let (_, id) = game.items.choose(rng).expect("a game has ordinary items");
            pool.push(Placed {
                item: *id,
                receiver: slot,
                flags: if rng.gen_bool(PROGRESSION_SHARE) {
                    FLAG_PROGRESSION
                } else {
                    0
                },
            });
        }
        pool.push(Placed {
            item: game.goal_id,
            receiver: slot,
            flags: FLAG_PROGRESSION,
        });
    }

    pool.shuffle(rng);

    // Deal into each player's own location list, in order.
    let mut per_slot = Vec::with_capacity(spec.players);
    let mut it = pool.into_iter();
    for _ in 0..spec.players {
        per_slot.push((&mut it).take(spec.locations).collect::<Vec<_>>());
    }
    per_slot
}

fn multidata(
    spec: &Spec,
    games: &[Game],
    game_of: &impl Fn(usize) -> usize,
    handles: &[String],
    seed_name: &str,
    placements: &[Vec<Placed>],
) -> PyObj {
    let total_slots = spec.players + spec.spectators;

    let slot_info = (0..total_slots)
        .map(|i| {
            let slot = i as i64 + 1;
            let (game, kind) = if i < spec.players {
                (games[game_of(i)].name.clone(), SLOT_TYPE_PLAYER)
            } else {
                // What the datapackage already calls a slot that plays nothing, so nothing
                // downstream has to special-case an empty game.
                ("Archipelago".to_string(), SLOT_TYPE_SPECTATOR)
            };
            (
                PyObj::Int(slot),
                PyObj::Instance {
                    class: ClassId::new("NetUtils", "NetworkSlot"),
                    args: vec![
                        str_(&handles[i]),
                        str_(&game),
                        PyObj::Instance {
                            class: ClassId::new("NetUtils", "SlotType"),
                            args: vec![PyObj::Int(kind)],
                        },
                        PyObj::Tuple(vec![]),
                    ],
                },
            )
        })
        .collect::<Vec<_>>();

    let connect_names = (0..total_slots)
        .map(|i| {
            (
                str_(&handles[i]),
                // Team 0 and nothing else: nothing upstream can generate a second team, and pahoa
                // refuses a seed that names one.
                PyObj::Tuple(vec![PyObj::Int(0), PyObj::Int(i as i64 + 1)]),
            )
        })
        .collect::<Vec<_>>();

    // **Every slot 1..=N gets a key, spectators included.** pahoa counts DECLARED slot ids, so a
    // spectator with no entry is what "slot ids are not contiguous" means -- the case that nearly
    // broke every real seed when validation was switched on.
    let locations = (0..total_slots)
        .map(|i| {
            let entries = if i < spec.players {
                let game = &games[game_of(i)];
                game.locations
                    .iter()
                    .zip(&placements[i])
                    .map(|((_, loc_id), placed)| {
                        (
                            PyObj::Int(*loc_id),
                            PyObj::Tuple(vec![
                                PyObj::Int(placed.item),
                                PyObj::Int(placed.receiver as i64),
                                PyObj::Int(placed.flags),
                            ]),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            (PyObj::Int(i as i64 + 1), PyObj::Dict(entries))
        })
        .collect::<Vec<_>>();

    // **`Archipelago` gets a package even though nothing plays it**, because every real seed
    // carries one: it is the generic world a spectator's slot names, and its contents are
    // upstream's own reserved entries, copied verbatim from the corpus rather than invented.
    //
    // An EMPTY package would not do. `resolve_datapackage` counts a game with no names as
    // unresolved, so a room would log `no data package for Archipelago` at every start: a line an
    // operator would reasonably read as the seed being malformed. Tried that first; it warned
    // exactly the same.
    let generic_package = (
        str_("Archipelago"),
        game_package(&[("Nothing", -1)], &[("Cheat Console", -1), ("Server", -2)]),
    );

    let datapackage = games
        .iter()
        .map(|game| {
            let mut items: Vec<(&str, i64)> = game
                .items
                .iter()
                .map(|(name, id)| (name.as_str(), *id))
                .collect();
            items.push((GOAL_ITEM, game.goal_id));
            let locations: Vec<(&str, i64)> = game
                .locations
                .iter()
                .map(|(name, id)| (name.as_str(), *id))
                .collect();
            (str_(&game.name), game_package(&items, &locations))
        })
        .chain(std::iter::once(generic_package))
        .collect::<Vec<_>>();

    let per_slot_empty = |f: fn() -> PyObj| {
        (0..total_slots)
            .map(|i| (PyObj::Int(i as i64 + 1), f()))
            .collect::<Vec<_>>()
    };

    PyObj::Dict(vec![
        (str_("seed_name"), str_(seed_name)),
        (str_("version"), version(GENERATOR_VERSION)),
        (
            str_("minimum_versions"),
            PyObj::Dict(vec![
                (str_("server"), version(MINIMUM_SERVER_VERSION)),
                (
                    str_("clients"),
                    PyObj::Dict(
                        (0..total_slots)
                            .map(|i| (PyObj::Int(i as i64 + 1), version(MINIMUM_SERVER_VERSION)))
                            .collect(),
                    ),
                ),
            ]),
        ),
        (str_("slot_info"), PyObj::Dict(slot_info)),
        (str_("connect_names"), PyObj::Dict(connect_names)),
        (str_("locations"), PyObj::Dict(locations)),
        (str_("datapackage"), PyObj::Dict(datapackage)),
        (
            str_("precollected_items"),
            PyObj::Dict(per_slot_empty(|| PyObj::List(vec![]))),
        ),
        (
            str_("precollected_hints"),
            PyObj::Dict(per_slot_empty(|| PyObj::Set(vec![]))),
        ),
        (str_("er_hint_data"), PyObj::Dict(vec![])),
        (str_("spheres"), PyObj::List(vec![])),
        (str_("race_mode"), PyObj::Int(0)),
        (
            str_("slot_data"),
            PyObj::Dict(per_slot_empty(|| PyObj::Dict(vec![]))),
        ),
        (str_("tags"), PyObj::List(vec![str_("AP")])),
        (str_("server_options"), server_options()),
    ])
}

/// One game's datapackage entry: the four name tables, then the checksum over them.
///
/// **The checksum is not optional, and leaving it out cost a whole test room its names.** An
/// earlier version wrote none, on the grounds that `pahoa-multidata` reads it as an `Option` and
/// that inventing one would be a value that lies. Both true, and both beside the point: a real
/// client never asks for names it has not been told the checksum of.
///
/// The chain, from the reference implementation rather than from guessing:
///
/// - `MultiServer.py:934-935` builds `RoomInfo.datapackage_checksums` and **omits a game whose
///   package has no checksum**. pahoa mirrors it (`pahoa-multidata/src/datapackage.rs:262`).
/// - `CommonClient.py:652` — `if game not in remote_data_package_checksums: continue` — so the
///   client never sends `GetDataPackage` for that game, and every item and location it renders is
///   a bare id. Note the *next* line: a checksum present but empty **does** trigger a fetch, so
///   the failure needs the key to be missing entirely, which is exactly what we produced.
///
/// Computed as upstream computes it (`worlds/AutoWorld.py:697`): sha1 over `NetUtils.encode` of
/// the package with the four keys **in alphabetical order** and no checksum in it. Nothing
/// verifies the value at play time — a client compares it for equality and caches under it — so
/// what matters is that it is derived from the content: regenerate a seed with the same game name
/// and different tables and the checksum moves, where a fixed string would leave every client
/// serving names from its cache of the *old* seed. Matching upstream's algorithm exactly is what
/// also lets one of these zips survive `WebHostLib/upload.py:63`, which recomputes and refuses a
/// mismatch.
fn game_package(items: &[(&str, i64)], locations: &[(&str, i64)]) -> PyObj {
    let dict = |pairs: &[(&str, i64)]| {
        PyObj::Dict(
            pairs
                .iter()
                .map(|(name, id)| (str_(name), PyObj::Int(*id)))
                .collect(),
        )
    };
    // Alphabetical, which is upstream's own order (`get_data_package_data`) and what its upload
    // check asserts. It is also the order hashed below, so the two cannot disagree.
    PyObj::Dict(vec![
        (str_("item_name_groups"), PyObj::Dict(vec![])),
        (str_("item_name_to_id"), dict(items)),
        (str_("location_name_groups"), PyObj::Dict(vec![])),
        (str_("location_name_to_id"), dict(locations)),
        (
            str_("checksum"),
            str_(&data_package_checksum(items, locations)),
        ),
    ])
}

/// `sha1(NetUtils.encode(package))`, over the same tables [`game_package`] writes.
///
/// `NetUtils.encode` is `json.dumps` with `separators=(',', ':')` and `ensure_ascii=False`, so the
/// canonical form is compact JSON with no escaping beyond JSON's own — which is what
/// `serde_json::to_string` produces for a string. Built by hand rather than through a
/// `serde_json::Value`, because a `Value`'s map sorts its keys and these tables must hash in the
/// order they are written.
fn data_package_checksum(items: &[(&str, i64)], locations: &[(&str, i64)]) -> String {
    use sha1::{Digest, Sha1};

    let table = |pairs: &[(&str, i64)]| {
        let body: Vec<String> = pairs
            .iter()
            .map(|(name, id)| {
                let key = serde_json::to_string(name).expect("a string always serializes");
                format!("{key}:{id}")
            })
            .collect();
        format!("{{{}}}", body.join(","))
    };
    let json = format!(
        concat!(
            r#"{{"item_name_groups":{{}},"item_name_to_id":{},"#,
            r#""location_name_groups":{{}},"location_name_to_id":{}}}"#
        ),
        table(items),
        table(locations)
    );
    format!("{:x}", Sha1::digest(json.as_bytes()))
}

/// The options the room adopts, because Puna passes `--use-embedded-options`.
///
/// `release_mode: "auto"` is the one that matters — see the module docs — and pahoa only accepts
/// either value because it round-trips: `from_text` is a substring test that lands anything
/// unrecognized on `disabled`, so `serve.rs`'s `permission` trusts a word only when
/// `as_text(from_text(w)) == w`. `"auto"` and `"disabled"` both do.
///
/// **`collect_mode: "disabled"`**, where pahoa's own default is `auto`. Under auto, a slot that
/// goals is immediately handed every outstanding item addressed to it, wherever those items still
/// sit — so the goal cascade delivers twice over, once by the release that empties the goaled
/// slot's world and again by the collect that fills it. Off, an item reaches a slot only when
/// somebody actually checks or releases the location holding it, which is the traffic a load run
/// is meant to be measuring. **It does not affect termination**: a slot goals on receiving its Goal
/// item, and auto-release is what keeps every Goal reachable.
///
/// Both passwords are written as explicit `None`, which is how a seed spells "no password"
/// (`serve.rs`'s `text`). The environment outranks a seed's password anyway, so this is belt and
/// braces against a synthetic seed producing a room nobody can join.
fn server_options() -> PyObj {
    PyObj::Dict(vec![
        (str_("release_mode"), str_("auto")),
        (str_("collect_mode"), str_("disabled")),
        (str_("password"), PyObj::None),
        (str_("server_password"), PyObj::None),
    ])
}

fn version((major, minor, build): (i64, i64, i64)) -> PyObj {
    PyObj::Tuple(vec![
        PyObj::Int(major),
        PyObj::Int(minor),
        PyObj::Int(build),
    ])
}

fn str_(s: &str) -> PyObj {
    PyObj::Str(s.into())
}

/// The rules `pahoa_multidata::MultiData::validate` enforces, checked before anything is written.
///
/// Duplicating them here is not distrust of that function — the tests round-trip through it — it is
/// that a violation caught at the point of construction says *which* slot, where the same failure
/// found at upload time says only that the seed will not load.
fn check_invariants(
    spec: &Spec,
    games: &[Game],
    game_of: &impl Fn(usize) -> usize,
    placements: &[Vec<Placed>],
) -> Result<()> {
    let total_slots = spec.players + spec.spectators;

    for (i, placed) in placements.iter().enumerate() {
        if placed.len() != spec.locations {
            bail!(
                "slot {} was dealt {} items for {} locations",
                i + 1,
                placed.len(),
                spec.locations
            );
        }
    }

    // Exactly one Goal per player, addressed to that player. A missing one is a load run that
    // never ends; a doubled one is a slot that goals twice and a count that never adds up.
    let mut goals = vec![0usize; spec.players];
    for placed in placements {
        for item in placed {
            let owner = item.receiver as usize - 1;
            if item.item == games[game_of(owner)].goal_id {
                goals[owner] += 1;
            }
        }
    }
    for (i, count) in goals.iter().enumerate() {
        if *count != 1 {
            bail!(
                "slot {} has {} Goal items, expected exactly 1",
                i + 1,
                count
            );
        }
    }

    if total_slots > u32::MAX as usize {
        bail!("more slots than a slot number can hold");
    }
    Ok(())
}

/// Wrap the multidata as a `.archipelago` and put it in a zip, beside a spoiler.
fn package(
    seed_name: &str,
    multidata: &PyObj,
    spec: &Spec,
    games: &[Game],
    game_of: &impl Fn(usize) -> usize,
    handles: &[String],
) -> Result<Vec<u8>> {
    let pickled = pickle::dumps(multidata);
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(9));
    encoder
        .write_all(&pickled)
        .context("compressing the multidata")?;
    let compressed = encoder.finish().context("compressing the multidata")?;

    // Format byte then zlib, which is what `MultiData::parse` reads.
    let mut archipelago = Vec::with_capacity(compressed.len() + 1);
    archipelago.push(pahoa_multidata::MAX_FORMAT_VERSION);
    archipelago.extend_from_slice(&compressed);

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        // **A fixed timestamp, because the default is the wall clock and that makes `--seed` a
        // lie.** `SimpleFileOptions::default()` calls `DateTime::default_for_write()`, which under
        // the `time` feature (enabled in this graph) stamps *now* into every entry. DOS time has
        // two-second resolution, so two runs of one seed produce identical multidata inside a zip
        // whose bytes differ, at four offsets: the time field of each local header and of each
        // central-directory entry.
        //
        // That is not only a flaky test. **Puna content-addresses generations by the sha256 of this
        // file**, so `--seed 42` twice would ingest as two separate generations, which is exactly
        // the deduplication that reproducing a run is supposed to give. The seed is printed so a
        // reported failure can be rebuilt; rebuilding it has to produce the same artifact.
        //
        // 1980-01-01 is the zip epoch and is what the crate itself falls back to without the `time`
        // feature, so this is the deterministic half of its own default rather than a value chosen
        // here.
        .last_modified_time(zip::DateTime::default());

    zip.start_file(format!("AP_{seed_name}.archipelago"), options)?;
    zip.write_all(&archipelago)?;

    // A spoiler, so `spoiler_policy` has something to serve and the ingest's spoiler detection has
    // something to find. Deliberately a summary rather than a playthrough: it exists to be a file
    // of the right name and shape.
    zip.start_file(format!("AP_{seed_name}_Spoiler.txt"), options)?;
    let mut spoiler = String::new();
    spoiler.push_str("Archipelago Version 0.6.8 - Seed: ");
    spoiler.push_str(seed_name);
    spoiler.push_str("\n\nThis is a SYNTHETIC seed generated by puna-tools for testing.\n");
    spoiler.push_str("No real game, player or world is named anywhere in it.\n\n");
    for i in 0..spec.players {
        spoiler.push_str(&format!(
            "Player {}: {} playing {}\n",
            i + 1,
            handles[i],
            games[game_of(i)].name
        ));
    }
    for (i, handle) in handles.iter().enumerate().skip(spec.players) {
        spoiler.push_str(&format!("Player {}: {} spectating\n", i + 1, handle));
    }
    zip.write_all(spoiler.as_bytes())?;

    Ok(zip.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pahoa_multidata::{MultiData, SlotType};
    use std::collections::BTreeMap;

    fn spec(players: usize, spectators: usize, games: usize, locations: usize) -> Spec {
        Spec {
            players,
            spectators,
            games,
            locations,
            seed: 42,
        }
    }

    /// Read the seed back out of the zip with the parser Puna and pahoa use.
    fn parse(zip: &[u8]) -> MultiData {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).expect("a zip");
        let name = (0..archive.len())
            .map(|i| archive.by_index(i).expect("member").name().to_string())
            .find(|n| n.ends_with(".archipelago"))
            .expect("a multidata member");
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut archive.by_name(&name).expect("member"), &mut raw)
            .expect("read");
        MultiData::parse(&raw).expect("pahoa's parser must read what we wrote")
    }

    /// **The checksum matches what Archipelago itself computes**, byte for byte.
    ///
    /// The expected value is not invented here: it is
    /// `sha1(NetUtils.encode(package).encode()).hexdigest()` run against the reference
    /// implementation for this exact package, which also confirmed the canonical form —
    /// `{"item_name_groups":{},"item_name_to_id":{"Goal":9100000000001,...` — is compact JSON in
    /// the written order. A checksum that merely *existed* would fix the reported symptom; one
    /// that matches is what lets a zip from this tool through `WebHostLib/upload.py`, which
    /// recomputes it and refuses a mismatch.
    #[test]
    fn the_checksum_is_the_one_archipelago_would_compute() {
        let items = [
            ("Goal", 9_100_000_000_001),
            ("Blue Sword", 9_100_000_000_002),
        ];
        let locations = [("Overworld Blue Goomba", 9_100_000_000_003)];

        assert_eq!(
            data_package_checksum(&items, &locations),
            "9066a1bf0338067e35399baf566b20362bfe541d"
        );
    }

    /// **It has to move with the content**, which is the whole reason it is a hash rather than a
    /// constant: a client caches the names it fetched *under this key*, so a regenerated seed
    /// sharing a game name and a stale checksum would be rendered with the previous seed's names.
    #[test]
    fn the_checksum_follows_the_names_and_the_ids() {
        let items = [("Goal", 1)];
        let locations = [("A Door", 2)];
        let base = data_package_checksum(&items, &locations);

        assert_ne!(base, data_package_checksum(&[("Goal", 3)], &locations));
        assert_ne!(base, data_package_checksum(&items, &[("A Door", 4)]));
        assert_ne!(base, data_package_checksum(&items, &[("A Gate", 2)]));
        // And the same tables twice must hash the same, or a rebuild would invalidate every cache.
        assert_eq!(base, data_package_checksum(&items, &locations));
    }

    /// **Every game the seed names carries a checksum, `Archipelago` included.**
    ///
    /// This is the assertion the field failure needed. A client adds `Archipelago` to the relevant
    /// games unconditionally (`CommonClient.py:648`), so the generic package skipping this would
    /// leave the reserved ids — `Nothing`, `Cheat Console`, `Server` — rendering as numbers on a
    /// room whose own games resolved perfectly.
    #[test]
    fn every_game_in_the_datapackage_can_be_asked_for_by_checksum() {
        let (zip, _) = build(&spec(6, 1, 3, 40)).expect("build");
        let data = parse(&zip);

        assert!(
            data.embedded_datapackage.contains_key("Archipelago"),
            "the generic package must be there to have a checksum at all"
        );
        for (game, package) in &data.embedded_datapackage {
            let checksum = package
                .checksum
                .as_deref()
                .unwrap_or_else(|| panic!("{game} has no checksum, so no client will ask for it"));
            assert_eq!(checksum.len(), 40, "{game}: sha1 hex is 40 characters");
            assert!(
                checksum.chars().all(|c| c.is_ascii_hexdigit()),
                "{game}: {checksum}"
            );
        }
    }

    /// **The whole contract in one test**: what this writes, pahoa reads, and its load-time checks
    /// accept. `validate` is the gate a room applies before it binds its port, so a seed that fails
    /// it is a pod that exits at startup.
    #[test]
    fn a_generated_seed_parses_and_passes_pahoas_load_checks() {
        for s in [
            spec(1, 0, 1, 1),
            spec(4, 0, 2, 50),
            spec(12, 3, 4, 200),
            // One game, many slots: every slot shares one name table, which is the case where a
            // per-game rather than per-slot id allocation could collide.
            spec(9, 0, 1, 30),
        ] {
            let (zip, _) = build(&s).expect("build");
            let data = parse(&zip);
            data.validate(pahoa_multidata::Version::new(0, 6, 7))
                .unwrap_or_else(|e| panic!("{s:?} would not load: {e}"));

            assert_eq!(data.slot_info.len(), s.players + s.spectators, "{s:?}");
            assert_eq!(data.connect_names.len(), s.players + s.spectators, "{s:?}");
            assert_eq!(data.locations.len(), s.players * s.locations, "{s:?}");
        }
    }

    /// Spectators connect, own nothing, and **still declare a locations key**. Omitting it is what
    /// "slot ids are not contiguous" means, and it is the failure that nearly refused every real
    /// seed when pahoa switched validation on.
    #[test]
    fn a_spectator_declares_no_locations_but_is_still_a_slot() {
        let s = spec(3, 2, 2, 10);
        let (zip, _) = build(&s).expect("build");
        let data = parse(&zip);

        let spectators: Vec<_> = data
            .slot_info
            .iter()
            .filter(|(_, info)| info.slot_type == SlotType::Spectator)
            .collect();
        assert_eq!(spectators.len(), 2);
        for (slot, info) in spectators {
            assert_eq!(
                data.locations.count_for(*slot),
                0,
                "slot {slot} owns locations"
            );
            assert_eq!(info.game, "Archipelago");
        }
        // The contiguity rule is over declared ids, and `validate` is what enforces it.
        data.validate(pahoa_multidata::Version::new(0, 6, 7))
            .expect("a seed with spectators must load");
    }

    /// **Exactly one Goal per slot, addressed to that slot, sitting at a real location.** A missing
    /// Goal is a load run that never ends; a doubled one is a slot that goals twice.
    #[test]
    fn every_slot_has_exactly_one_goal_somewhere_in_the_multiworld() {
        let s = spec(8, 1, 3, 40);
        let (zip, _) = build(&s).expect("build");
        let data = parse(&zip);

        // The Goal id of each game, read back out of the datapackage rather than recomputed.
        // Played games only. `Archipelago` is the spectator pseudo-game and carries an empty
        // package on purpose -- a spectator has no Goal because it can never finish.
        let goal_ids: BTreeMap<&str, i64> = data
            .embedded_datapackage
            .iter()
            .filter(|(game, _)| game.as_str() != "Archipelago")
            .map(|(game, pkg)| {
                (
                    game.as_str(),
                    *pkg.item_name_to_id
                        .get(GOAL_ITEM)
                        .unwrap_or_else(|| panic!("{game} has no {GOAL_ITEM} item")),
                )
            })
            .collect();

        let mut found = vec![0usize; s.players];
        for entry in data.locations.all() {
            let owner_game = &data.slot_info[&entry.receiver].game;
            if goal_ids.get(owner_game.as_str()) == Some(&entry.item) {
                found[entry.receiver as usize - 1] += 1;
            }
        }
        assert_eq!(found, vec![1; s.players], "goal placements per slot");
    }

    /// Items repeat, which is the normal shape of a pool — 120 Power Stars — and the thing a naive
    /// "one of each" generator would get wrong.
    #[test]
    fn items_are_drawn_with_replacement() {
        let s = spec(2, 0, 1, 400);
        let (zip, _) = build(&s).expect("build");
        let data = parse(&zip);

        let placed = data.locations.all().len();
        let distinct: std::collections::HashSet<i64> =
            data.locations.all().iter().map(|e| e.item).collect();
        assert_eq!(placed, 800);
        assert!(
            distinct.len() < placed,
            "every placed item was unique, so nothing repeated"
        );
    }

    /// `--seed` reproduces a run exactly, and two seeds do not collide — which is what makes a
    /// small generation varied between runs and a reported failure reproducible after one.
    #[test]
    fn the_seed_reproduces_a_run_and_different_seeds_differ() {
        let mut a = spec(3, 0, 2, 25);
        a.seed = 1;
        let mut b = a.clone();
        b.seed = 2;

        let (first, s1) = build(&a).expect("build");
        let (again, _) = build(&a).expect("build");
        let (other, s2) = build(&b).expect("build");

        assert_eq!(first, again, "--seed did not reproduce the run");
        assert_ne!(first, other, "two seeds produced the same zip");
        assert_ne!(s1.seed_name, s2.seed_name);
    }

    /// **Nothing in the zip is stamped with the wall clock**, which the assertion above can only
    /// catch by luck.
    ///
    /// `SimpleFileOptions::default()` writes *now* into every entry — `default_for_write()`, under
    /// the `time` feature this graph enables — and DOS time has two-second resolution. So two builds
    /// of one seed differ only when they straddle a boundary, which is a coin weighted by how long
    /// a build takes: it flaked **once in fifty** nightly runs and passes every time anybody runs it
    /// by hand.
    ///
    /// The consequence is bigger than the flake. **Puna content-addresses generations by the sha256
    /// of this file**, so a clock-stamped zip makes `--seed 42` ingest as a different generation
    /// every time — defeating the deduplication that reproducing a run exists to give.
    ///
    /// Asserted on the stored timestamp rather than by building twice across a real boundary: that
    /// version reproduces the bug faithfully and costs two seconds of every test run, and this one
    /// names the actual property.
    #[test]
    fn no_zip_entry_carries_the_wall_clock() {
        let (zip, _) = build(&spec(2, 0, 1, 5)).expect("build");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).expect("a zip");

        assert!(
            archive.len() >= 2,
            "this seed should carry seed and spoiler"
        );
        for i in 0..archive.len() {
            let entry = archive.by_index(i).expect("member");
            let name = entry.name().to_string();
            assert_eq!(
                entry.last_modified(),
                Some(zip::DateTime::default()),
                "{name} is stamped with the clock, so this seed's bytes depend on when it was built"
            );
        }
    }

    /// The seed adopts release-on-goal, and carries no password. Puna passes
    /// `--use-embedded-options`, so both of these are things the ROOM will do.
    #[test]
    fn the_embedded_options_release_on_goal_and_set_no_password() {
        let (zip, _) = build(&spec(2, 0, 1, 5)).expect("build");
        let data = parse(&zip);
        let options = data.server_options.expect("server_options");
        let dict = options.as_dict().expect("a dict");

        let get = |key: &str| {
            dict.iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v)
        };
        assert_eq!(get("release_mode").and_then(|v| v.as_str()), Some("auto"));
        // Off, against pahoa's own default of `auto`: a goaled slot should not be handed its
        // outstanding items on top of the release cascade that is the point of the run.
        assert_eq!(
            get("collect_mode").and_then(|v| v.as_str()),
            Some("disabled")
        );
        for key in ["password", "server_password"] {
            assert!(
                matches!(get(key), Some(PyObj::None)),
                "{key} must be explicitly None"
            );
        }
    }

    /// **Every mode word this writes must survive pahoa's round-trip check**, which is what stops
    /// `from_text`'s substring matching turning a near-miss into silence — anything it does not
    /// recognize lands on `disabled`, so a seed with `"of"` for `"off"` would quietly turn releases
    /// off and the run would never end.
    #[test]
    fn the_mode_words_are_ones_pahoa_will_accept() {
        // pahoa's `serve.rs::permission`: trust the word only when it round-trips.
        let round_trips = |word: &str| {
            let mut bits = 0u8;
            let lower = word.to_ascii_lowercase();
            if lower.contains("auto") {
                bits |= 0b110;
            }
            if lower.contains("enabled") {
                bits |= 0b001;
            }
            if lower.contains("goal") {
                bits |= 0b010;
            }
            let text = match bits {
                0b000 => "disabled",
                0b001 => "enabled",
                0b010 => "goal",
                0b110 => "auto",
                0b111 => "auto-enabled",
                _ => "?",
            };
            text == word.replace('_', "-")
        };

        let options = server_options();
        let dict = options.as_dict().expect("a dict");
        for (key, value) in dict {
            let Some(word) = value.as_str() else { continue };
            if key.as_str().is_some_and(|k| k.ends_with("_mode")) {
                assert!(round_trips(word), "{word:?} would not be trusted by pahoa");
            }
        }
    }

    /// A shape that cannot work is refused where it is built, naming the reason.
    #[test]
    fn impossible_specs_are_refused_rather_than_written() {
        assert!(build(&spec(0, 2, 1, 10)).is_err(), "no players");
        assert!(
            build(&spec(2, 0, 1, 0)).is_err(),
            "no locations, so nowhere for a Goal"
        );
        assert!(build(&spec(2, 0, 0, 10)).is_err(), "no games");
    }
}
