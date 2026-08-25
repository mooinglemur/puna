//! The vocabulary a synthetic seed is built from.
//!
//! Nothing here names a real game, a real player or a real Archipelago world. That is deliberate:
//! a synthetic seed can end up in a database, a metric label and a tracker page, and a fixture that
//! looked like a real room's data would be a fixture somebody eventually mistakes for one.
//!
//! ## Why three lists for locations rather than one
//!
//! A location name is `<regional> <physical> <noun>` — *"Overworld Blue Goomba"*. Three small lists
//! multiply into [`LOCATION_SPACE`] distinct names, which is what lets even a twelve-slot seed draw
//! a varied set rather than the same first N entries every time. One long list would give a small
//! generation the same names on every run, and the whole point of varying them is that a bug which
//! depends on a particular name has somewhere to hide otherwise.

use rand::Rng;
use rand::seq::SliceRandom;
use std::collections::HashSet;

/// Where a thing is. Broad strokes of a map.
pub const REGIONAL: &[&str] = &[
    "Overworld",
    "Underworld",
    "Eastern",
    "Western",
    "Northern",
    "Southern",
    "Downtown",
    "Uptown",
    "Coastal",
    "Inland",
    "Sunken",
    "Floating",
    "Frontier",
    "Outer",
    "Inner",
    "Upper",
    "Lower",
    "Central",
    "Forgotten",
    "Hidden",
    "Abandoned",
    "Royal",
    "Ancient",
    "Deep",
    "Highland",
    "Lowland",
    "Riverside",
    "Cliffside",
    "Skyward",
    "Subterranean",
    "Boreal",
    "Tropical",
    "Desert",
    "Glacial",
    "Volcanic",
    "Twilight",
    "Dawnlit",
    "Duskbound",
    "Windswept",
    "Moonlit",
    "Offshore",
    "Backwater",
    "Midtown",
    "Seaside",
    "Undercity",
    "Farside",
    "Nearside",
    "Outskirt",
];

/// What it looks, feels or sounds like.
pub const PHYSICAL: &[&str] = &[
    "Red",
    "Blue",
    "Green",
    "Golden",
    "Silver",
    "Iron",
    "Copper",
    "Brass",
    "Tall",
    "Squat",
    "Liquid",
    "Molten",
    "Frozen",
    "Creamy",
    "Half",
    "Musical",
    "Shiny",
    "Rusted",
    "Velvet",
    "Brittle",
    "Hollow",
    "Prickly",
    "Gilded",
    "Cracked",
    "Humming",
    "Sticky",
    "Feathered",
    "Marble",
    "Obsidian",
    "Wooden",
    "Glassy",
    "Damp",
    "Radiant",
    "Squishy",
    "Serrated",
    "Woven",
    "Chipped",
    "Fragrant",
    "Luminous",
    "Leaden",
    "Porcelain",
    "Bristled",
    "Waxen",
    "Crooked",
    "Salted",
    "Whispering",
    "Lopsided",
    "Threadbare",
    "Glittering",
    "Sunbleached",
    "Mossy",
    "Enameled",
    "Quilted",
    "Braided",
    "Frosted",
    "Speckled",
    "Translucent",
    "Knotted",
    "Buttered",
    "Echoing",
];

/// A thing you could stand next to (or attack) and check.
pub const NOUNS: &[&str] = &[
    "Gate",
    "Archway",
    "Shop",
    "Door",
    "Mage",
    "Goomba",
    "Boss",
    "Chest",
    "Piano",
    "Balcony",
    "Bookshelf",
    "Fountain",
    "Cellar",
    "Bridge",
    "Ladder",
    "Vault",
    "Shrine",
    "Kiosk",
    "Terrace",
    "Lantern",
    "Alcove",
    "Anvil",
    "Aviary",
    "Belfry",
    "Cauldron",
    "Chandelier",
    "Cistern",
    "Crypt",
    "Dumbwaiter",
    "Ferry",
    "Foyer",
    "Gargoyle",
    "Greenhouse",
    "Hedge",
    "Kennel",
    "Larder",
    "Lighthouse",
    "Mailbox",
    "Mural",
    "Observatory",
    "Obelisk",
    "Parapet",
    "Pergola",
    "Portcullis",
    "Quarry",
    "Rookery",
    "Sarcophagus",
    "Scaffold",
    "Sewer",
    "Signpost",
    "Silo",
    "Stable",
    "Statue",
    "Sundial",
    "Tapestry",
    "Trapdoor",
    "Turnstile",
    "Waterwheel",
    "Well",
    "Windmill",
    "Wardrobe",
    "Zeppelin",
    "Jukebox",
    "Nightstand",
    "Trellis",
    "Culvert",
    "Drawbridge",
    "Footlocker",
    "Gazebo",
    "Haystack",
    "Icebox",
    "Jetty",
    "Kiln",
    "Loom",
    "Millstone",
    "Nook",
    "Outhouse",
    "Pillory",
    "Quiver",
    "Reliquary",
    "Turret",
    "Vestibule",
    "Wharf",
    "Cupola",
    "Dovecote",
    "Grotto",
    "Hearth",
    "Inkwell",
    "Lectern",
    "Mezzanine",
];

/// Distinct `<regional> <physical> <noun>` names available before any suffixing.
pub const LOCATION_SPACE: usize = REGIONAL.len() * PHYSICAL.len() * NOUNS.len();

/// A hundred things a world might hand you
pub const ITEMS: &[&str] = &[
    "Rusty Key",
    "Silver Key",
    "Progressive Sword",
    "Bomb Bag",
    "Feather Charm",
    "Grappling Hook",
    "Lantern Oil",
    "Moon Pearl",
    "Iron Boots",
    "Winged Sandals",
    "Health Fragment",
    "Mana Vial",
    "Compass",
    "Dungeon Map",
    "Small Key",
    "Boss Key",
    "Power Star",
    "Banana Coin",
    "Red Coin",
    "Blue Orb",
    "Ancient Tablet",
    "Cracked Amulet",
    "Fire Rod",
    "Ice Rod",
    "Thunder Wand",
    "Spellbook",
    "Hookshot",
    "Longbow",
    "Quiver Upgrade",
    "Bomb Upgrade",
    "Wallet Upgrade",
    "Magic Cape",
    "Mirror Shield",
    "Hearty Loaf",
    "Golden Feather",
    "Silver Arrow",
    "Crystal Shard",
    "Star Fragment",
    "Sun Medallion",
    "Moon Medallion",
    "Rocket Boots",
    "Double Jump",
    "Wall Kick",
    "Air Dash",
    "Slide Kick",
    "Progressive Armor",
    "Chain Mail",
    "Leather Vest",
    "Copper Bracelet",
    "Ruby Ring",
    "Sapphire Brooch",
    "Emerald Pin",
    "Pearl Necklace",
    "Bag of Seeds",
    "Watering Can",
    "Fishing Rod",
    "Bug Net",
    "Shovel",
    "Pickaxe",
    "Hammer",
    "Chisel",
    "Blowtorch",
    "Wrench",
    "Screwdriver",
    "Duct Tape",
    "Battery",
    "Fuse",
    "Circuit Board",
    "Keycard",
    "Access Badge",
    "Elevator Key",
    "Parking Token",
    "Bus Pass",
    "Subway Token",
    "Library Card",
    "Coffee Voucher",
    "Cold Sandwich",
    "Pickle Jar",
    "Hot Sauce",
    "Birthday Cake",
    "Cheese Wheel",
    "Soup Ladle",
    "Rolling Pin",
    "Tea Kettle",
    "Sugar Cube",
    "Salt Shaker",
    "Fizzy Drink",
    "Rubber Duck",
    "Paper Crane",
    "Music Box",
    "Tuning Fork",
    "Kazoo",
    "Harmonica",
    "Sheet Music",
    "Concert Ticket",
    "Backstage Pass",
    "Lucky Coin",
    "Loaded Die",
    "Deck of Cards",
    "Chess Piece",
];

/// The item every slot needs exactly one of, and the only name with meaning to the load tool.
pub const GOAL_ITEM: &str = "Goal";

/// Games that do not exist, so a synthetic seed can never be mistaken for a real one.
pub const GAMES: &[&str] = &[
    "Gloomhaven Drift",
    "Petal Ascendant",
    "Nine Lives of Rusk",
    "Vaultbreaker 64",
    "Chrono Bazaar",
    "The Long Kelp",
    "Marrow and Meridian",
    "Sunken Arcade",
    "Ferrous Bloom",
    "Widdershins",
    "Pocket Leviathan",
    "Astral Custodian",
    "Brackwater Tales",
    "Kingdom of Cogs",
    "Papercut Saga",
    "Velvet Automaton",
    "Twelve Bell Hollow",
    "The Understair",
    "Mirrorwright",
    "Salt and Signal",
    "Orbital Orchard",
    "Hexcrawl Deluxe",
    "Lanternfall",
    "Quiet Machines",
];

/// Handles nobody is using, for slot names.
pub const HANDLES: &[&str] = &[
    "cryptidwrangler",
    "PixelHermit",
    "vaultmoth",
    "SirLagsAlot",
    "quietstorm",
    "BrambleFox",
    "ноктюрн",
    "glasswing",
    "TwoLeftBoots",
    "marmaladesky",
    "Nullpointer",
    "hexemoji",
    "SlowClapper",
    "driftwoodie",
    "Kelpforest",
    "AmpersandJane",
    "bitrot",
    "TinCanTelephone",
    "moonlitmoss",
    "GrumpyKettle",
    "saltflats",
    "Perpetualiy",
    "cobblestep",
    "WanderingByte",
    "fernstatic",
    "OpalTangent",
    "brassmonkey",
    "SleepyTrilobite",
    "verdigris",
    "PaperLantern",
    "quillfeather",
    "MidnightOilCo",
    "sundialer",
    "Fathomless",
    "cinderblock",
    "TangentialTim",
    "wispwillow",
    "OctaveBelow",
    "riverstone",
    "GildedPigeon",
    "thimbleful",
    "NorthOfHere",
    "clatterbox",
    "SoftReboot",
    "emberglow",
    "PocketUniverse",
    "lichenlover",
    "TumbledownHall",
    "starlingsong",
    "BlueHourClub",
    "mossgatherer",
    "EchoChamberly",
    "windowseat",
    "FerrousWheel",
    "candlewick",
    "HalfRemembered",
    "opaltide",
    "SecondBreakfast",
    "rustbelt",
    "LighthouseKeep",
    "duskmoth",
    "PenultimateP",
    "flintspark",
    "AboveTheFold",
    "seaglassy",
    "CartographerX",
    "hollowbone",
    "SlightlyFeral",
    "amberwave",
    "TheQuietCar",
    "birchbark",
    "OffByOne",
    "stonefruit",
    "MarginNote",
    "coalsmoke",
    "UndertowJoe",
    "pinecone",
    "HeavyWeather",
    "loamlight",
    "SixOfCrows",
    "tidewrack",
    "AntiqueModem",
    "sparrowfall",
    "LowBattery",
    "gossamer",
    "TheLongWay",
    "peatmoss",
    "SundayDriver",
    "chalkline",
    "FirstLightt",
    "brinepool",
    "NocturnalOwl",
    "fogbank",
    "PatchTuesday",
    "reedwarbler",
    "SmallHours",
    "heathermoor",
    "TrailingComma",
    "silversmith",
    "DeadReckoner",
    "clovergrove",
    "WaxAndWane",
    "kindling",
    "TheSlowLane",
    "meltwater",
    "GraphitePaw",
    "sagebrush",
    "ExitPursued",
    "harborlight",
    "MinorChord",
    "driftglass",
    "HalfLifeHalf",
    "quarrystone",
    "PaleBlueDot",
    "thistledown",
    "RoundNumbers",
    "wickerwork",
    "LastKnownGood",
    "yarrowroot",
    "OpenLoop",
];

/// Distinct location names, drawn at random from the three lists.
///
/// **Sampling without replacement**, because a duplicate location name in one game is a datapackage
/// whose `location_name_to_id` silently holds fewer entries than there are locations — the ids
/// would still be unique, so the seed would load and the tracker would show two different checks
/// under one name.
///
/// Rejection sampling while the request is small against [`LOCATION_SPACE`], which is the ordinary
/// case by a wide margin; past an eighth of the space the expected retries stop being negligible
/// and it materializes and shuffles instead.
pub fn locations(n: usize, rng: &mut impl Rng) -> Vec<String> {
    let compose = |i: usize| {
        let noun = i % NOUNS.len();
        let rest = i / NOUNS.len();
        let physical = rest % PHYSICAL.len();
        let regional = rest / PHYSICAL.len();
        format!(
            "{} {} {}",
            REGIONAL[regional], PHYSICAL[physical], NOUNS[noun]
        )
    };

    if n > LOCATION_SPACE {
        // More locations than distinct names. Suffix the overflow rather than refusing: a seed with
        // 300,000 checks in one game is not a shape worth supporting properly, but it is a shape
        // worth being able to generate for a cardinality test.
        let mut names = locations(LOCATION_SPACE, rng);
        for i in 0..(n - LOCATION_SPACE) {
            names.push(format!(
                "{} {}",
                compose(i % LOCATION_SPACE),
                i / LOCATION_SPACE + 2
            ));
        }
        return names;
    }

    if n > LOCATION_SPACE / 8 {
        let mut all: Vec<usize> = (0..LOCATION_SPACE).collect();
        all.shuffle(rng);
        return all.into_iter().take(n).map(compose).collect();
    }

    let mut seen = HashSet::with_capacity(n);
    let mut names = Vec::with_capacity(n);
    while names.len() < n {
        let i = rng.gen_range(0..LOCATION_SPACE);
        if seen.insert(i) {
            names.push(compose(i));
        }
    }
    names
}

/// Distinct ordinary item names. [`GOAL_ITEM`] is added by the caller and is not among these.
///
/// The hundred base names come first, shuffled; past that they are qualified with a physical
/// adjective (*"Frosted Hookshot"*), and past **that** with a number. Distinct **names**, note —
/// nothing stops the same name being placed many times, which is the normal shape of an item pool
/// and the reason Super Mario 64 has 120 Power Stars.
pub fn items(n: usize, rng: &mut impl Rng) -> Vec<String> {
    let mut base: Vec<String> = ITEMS.iter().map(|s| (*s).to_string()).collect();
    base.shuffle(rng);
    if n <= base.len() {
        base.truncate(n);
        return base;
    }

    let mut qualified: Vec<String> = PHYSICAL
        .iter()
        .flat_map(|adj| ITEMS.iter().map(move |item| format!("{adj} {item}")))
        .collect();
    qualified.shuffle(rng);

    let mut names = base;
    names.extend(qualified);
    if n <= names.len() {
        names.truncate(n);
        return names;
    }

    let unique = names.len();
    for i in 0..(n - unique) {
        names.push(format!("{} {}", names[i % unique], i / unique + 2));
    }
    names
}

/// Distinct slot names, which are also the connect names a client authenticates with.
///
/// Numbered past the end of the list rather than refused: a 500-slot load test is a real thing to
/// want, and `HANDLES` is a vocabulary rather than a limit.
pub fn handles(n: usize, rng: &mut impl Rng) -> Vec<String> {
    let mut names: Vec<String> = HANDLES.iter().map(|s| (*s).to_string()).collect();
    names.shuffle(rng);
    let unique = names.len();
    for i in unique..n {
        names.push(format!("{}{}", names[i % unique], i / unique + 1));
    }
    names.truncate(n);
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn rng(seed: u64) -> StdRng {
        StdRng::seed_from_u64(seed)
    }

    fn all_distinct(names: &[String]) -> bool {
        names.iter().collect::<HashSet<_>>().len() == names.len()
    }

    /// Every list is a set. A repeat in any of them is a datapackage that quietly holds fewer
    /// entries than the seed has things, which loads fine and shows two checks under one name.
    #[test]
    fn the_source_lists_have_no_duplicates() {
        for (what, list) in [
            ("REGIONAL", REGIONAL),
            ("PHYSICAL", PHYSICAL),
            ("NOUNS", NOUNS),
            ("ITEMS", ITEMS),
            ("GAMES", GAMES),
            ("HANDLES", HANDLES),
        ] {
            assert_eq!(
                list.iter().collect::<HashSet<_>>().len(),
                list.len(),
                "{what} repeats a word"
            );
        }
        assert_eq!(ITEMS.len(), 100, "the item list is specified as a hundred");
        assert!(
            !ITEMS.contains(&GOAL_ITEM),
            "Goal must not also be an ordinary item, or a slot could be handed its goal as filler"
        );
    }

    /// Distinct at every size that changes the strategy: rejection sampling, the shuffle path, and
    /// past the space entirely.
    #[test]
    fn locations_are_distinct_at_every_scale() {
        for n in [
            1,
            100,
            5_000,
            LOCATION_SPACE / 4,
            LOCATION_SPACE,
            LOCATION_SPACE + 50,
        ] {
            let names = locations(n, &mut rng(7));
            assert_eq!(names.len(), n, "n={n}");
            assert!(all_distinct(&names), "n={n} produced a duplicate");
        }
    }

    /// The same, across the two qualification steps for items and the numbering step for handles.
    #[test]
    fn items_and_handles_are_distinct_at_every_scale() {
        for n in [1, 100, 101, 6_100, 6_200] {
            let names = items(n, &mut rng(11));
            assert_eq!(names.len(), n, "items n={n}");
            assert!(all_distinct(&names), "items n={n} produced a duplicate");
        }
        for n in [1, HANDLES.len(), HANDLES.len() + 1, 500] {
            let names = handles(n, &mut rng(13));
            assert_eq!(names.len(), n, "handles n={n}");
            assert!(all_distinct(&names), "handles n={n} produced a duplicate");
        }
    }

    /// **A small seed must not look like the last small seed.** This is the whole reason locations
    /// are composed from three lists rather than taken from one, so it is worth an assertion:
    /// twelve locations drawn twice should differ, and drawn twice from one seed should not.
    #[test]
    fn small_draws_vary_between_runs_and_repeat_within_one_seed() {
        let a = locations(12, &mut rng(1));
        let b = locations(12, &mut rng(2));
        assert_ne!(a, b, "two runs produced the same twelve locations");
        assert_eq!(
            a,
            locations(12, &mut rng(1)),
            "--seed did not reproduce a run"
        );
    }

    /// A name is three words from three lists, which is what makes the space large enough to draw
    /// from without repeating.
    #[test]
    fn a_location_reads_as_region_attribute_thing() {
        let name = &locations(1, &mut rng(3))[0];
        let words: Vec<&str> = name.split(' ').collect();
        assert_eq!(words.len(), 3, "{name}");
        assert!(REGIONAL.contains(&words[0]), "{name}");
        assert!(PHYSICAL.contains(&words[1]), "{name}");
        assert!(NOUNS.contains(&words[2]), "{name}");
        // A const block, because the value is known at compile time and clippy is right that a
        // runtime assertion over one is a test of the compiler. It still earns its place: it is
        // the floor that makes drawing without replacement cheap for any seed anyone would build.
        const { assert!(LOCATION_SPACE > 200_000) };
    }
}
