//! Build a synthetic Archipelago generation zip, uploadable to Puna as-is.
//!
//! ```text
//! cargo run -p puna-tools --bin make-generation -- --slots 12 --locations 250 --out /tmp/seed.zip
//! ```

use anyhow::{Context, Result};
use puna_tools::args::Args;
use puna_tools::seed::{self, Spec};

const USAGE: &str = "\
make-generation: a synthetic multiworld seed, uploadable to Puna as a generation

  --slots N        player slots, each owning --locations checks   (default 4)
  --spectators N   slots that connect and play nothing            (default 0)
  --games N        distinct games, assigned round-robin           (default 2)
  --locations N    checks per slot. Also the size of each game's
                   item table: --locations minus one ordinary
                   items, plus the Goal                           (default 100)
  --seed N         reproduce an earlier run                       (default random)
  --out PATH       where to write the zip            (default ./AP_<seed name>.zip)
  --help

Every slot gets exactly one Goal item, shuffled into the multiworld like any other, so it may sit
in its own world or anybody else's. The seed embeds release_mode=auto, so a slot that goals releases
its remaining items; that is what lets a room played by room-load reach an end.
";

fn main() -> Result<()> {
    let args = Args::parse(
        &["slots", "spectators", "games", "locations", "seed", "out"],
        &["help"],
    )?;
    if args.is_set("help") {
        print!("{USAGE}");
        return Ok(());
    }

    let spec = Spec {
        players: args.get("slots", 4)?,
        spectators: args.get("spectators", 0)?,
        games: args.get("games", 2)?,
        locations: args.get("locations", 100)?,
        // Printed below, so a run that turns up something odd can be reproduced exactly even
        // though the default is random.
        seed: args.get("seed", rand::random())?,
    };

    let (zip, summary) = seed::build(&spec)?;
    let path = args
        .text("out")
        .map(String::from)
        .unwrap_or_else(|| format!("AP_{}.zip", summary.seed_name));
    std::fs::write(&path, &zip).with_context(|| format!("writing {path}"))?;

    println!("wrote {path} ({} KiB)", summary.bytes / 1024);
    println!("  seed name    {}", summary.seed_name);
    println!(
        "  --seed       {}   (to reproduce this exact seed)",
        summary.seed
    );
    println!(
        "  slots        {} playing, {} spectating",
        summary.players, summary.spectators
    );
    println!(
        "  checks       {} per slot, {} in the multiworld",
        summary.locations_per_slot, summary.total_checks
    );
    println!("  games        {}", summary.games.join(", "));
    Ok(())
}
