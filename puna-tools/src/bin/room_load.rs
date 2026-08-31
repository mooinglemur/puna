//! Play a synthetic generation against a running room.
//!
//! ```text
//! cargo run -p puna-tools --bin room-load -- \
//!     --generation /tmp/seed.zip --room mw.ionium.us:45000 --rate 0.5
//! ```

use anyhow::{Context, Result, bail};
use pahoa_multidata::{MultiData, SlotType};
use puna_tools::args::{self, Args};
use puna_tools::load::{self, Config, ITEMS_HANDLING_ALL, SlotPlan, Totals};
use puna_tools::words::GOAL_ITEM;
use std::collections::BTreeSet;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Barrier;

const USAGE: &str = "\
room-load: synthetic check traffic against a running Puna room

  --generation PATH  the zip that made this room (required)
  --room HOST:PORT   the room's advertised address (required)
  --password X       for a room-wide password. Per-slot rooms are not supported;
                     see the note at the bottom of this help
  --slots LIST       which slots to play: 1-8, 1,3,5, or all       (default all)
  --connect-rate N   connections opened per second. 0 opens them
                     all at once, which is a connect storm          (default 5)
  --clients-per-slot N  sockets per slot, 1-3: the game client, then
                     a text client, then a tracker. The extra two
                     only consume the firehose and answer
                     heartbeats -- which is what a real player's
                     three clients do, and what makes the room's
                     fan-out realistic                              (default 1)
  --rate N           checks per second PER SLOT. The room's offered
                     load is this times the number of active slots (default 0.5)
                     Rates below one check per ten seconds are fine:
                     0.01 is one check per slot every 100 seconds
  --jitter 0..1      how bursty. The rate holds on average over ten
                     seconds and swings hard inside it              (default 0.8)
  --batch N          locations per LocationChecks packet             (default 1)
  --items-handling N 7 receives everything, which is the point       (default 7)
  --say-rate N       chat lines per second per slot, for filters     (default 0)
  --linger DURATION  keep draining after the last slot goals         (default 10s)
  --timeout DURATION give up on a room that will not finish          (default 30m)
  --help

Every slot stops checking the moment it receives its own Goal item and holds the connection open.
The seed sets release_mode=auto, so a slot that goals releases the rest of its world -- which is
what keeps the other slots' Goals reachable and lets the run reach an end.

Connections are opened on a ramp rather than all at once, because a room fills over minutes and
because opening two hundred at once is enough to make a healthy room drop most of them.

Connects over wss:// and verifies the certificate against the host you name, so use the room's
advertised hostname rather than an address. There is deliberately no flag to turn that off.

Every connection offers permessage-deflate, through pahoa's own WebSocket client, so the room
compresses a broadcast once and shares it exactly as it does for real players. How many negotiated
it is reported at the end.
";

#[tokio::main]
async fn main() -> Result<()> {
    // Before anything opens a socket. See `load::install_crypto_provider` for what goes wrong
    // without it, which is every `wss://` slot panicking at its first handshake.
    load::install_crypto_provider();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "room_load=info".into()),
        )
        .init();

    let args = Args::parse(
        &[
            "generation",
            "room",
            "password",
            "slots",
            "connect-rate",
            "clients-per-slot",
            "rate",
            "jitter",
            "batch",
            "items-handling",
            "say-rate",
            "linger",
            "timeout",
        ],
        &["help"],
    )?;
    if args.is_set("help") {
        print!("{USAGE}");
        return Ok(());
    }

    let config = Arc::new(Config {
        room: args.require("room")?.to_string(),
        password: args.text("password").map(String::from),
        connect_rate: args.get("connect-rate", load::DEFAULT_CONNECT_RATE)?,
        clients_per_slot: args.get("clients-per-slot", 1usize)?.clamp(1, 3),
        rate: args.get("rate", 0.5)?,
        jitter: args.get("jitter", 0.8)?,
        batch: args.get("batch", 1usize)?.max(1),
        items_handling: args.get("items-handling", ITEMS_HANDLING_ALL)?,
        say_rate: args.get("say-rate", 0.0)?,
        linger: args::duration(args.text("linger").unwrap_or("10s"))?,
        timeout: args::duration(args.text("timeout").unwrap_or("30m"))?,
    });

    let (plans, locations) = read_generation(args.require("generation")?)?;
    let wanted = select(args.text("slots").unwrap_or("all"), &plans)?;
    let plans: Vec<SlotPlan> = plans
        .into_iter()
        .filter(|p| wanted.contains(&p.slot))
        .collect();
    if plans.is_empty() {
        bail!("--slots selected nothing");
    }

    // Spectators never goal, so the run's end is decided by the players alone. Counting a
    // spectator would leave a run waiting forever for somebody who cannot finish.
    let players = plans.iter().filter(|p| p.goal_item.is_some()).count();
    // What a full house looks like, so the progress line can say `connected 1455/2000` rather than
    // a bare number that only means something if you remember what was asked for.
    let sockets = plans.len() * config.clients_per_slot;
    println!(
        "playing {} slots ({players} can goal) against {} at {}/s each, jitter {}",
        plans.len(),
        config.room,
        config.rate,
        config.jitter
    );
    // Computed once, here, so every slot's ramp position and the gate they share come off one
    // clock. See `load::schedule` for why the connects are dealt out at all. **Sized to
    // CONNECTIONS, not slots**: three clients per slot is three dials, and ramping only the game
    // clients would let the other two thirds arrive as a storm.
    let schedules = load::schedule(
        Instant::now(),
        plans.len() * config.clients_per_slot,
        config.connect_rate,
    );
    let ramp = match (schedules.first(), schedules.last()) {
        (Some(first), Some(last)) => last.connect_at - first.connect_at,
        _ => Duration::ZERO,
    };
    if ramp > Duration::ZERO {
        println!(
            "  connecting over {} at {}/s; each slot starts checking as soon as it is up, so load \
             builds with the population",
            humanize(ramp),
            config.connect_rate
        );
    } else {
        println!("  connecting all at once; nobody checks until every slot is up");
    }

    // **When the first check will land, and how long the whole thing would take.** A slow rate is a
    // legitimate soak and is indistinguishable from a stuck tool until something arrives: at 0.01/s
    // that is a hundred seconds of `checks 0` on a run doing exactly what it was told.
    //
    // The first check comes one window after the FIRST slot connects, since a ramped run no longer
    // waits for the rest, where the last slot's world is not finished until a ramp later, which is
    // why only the full-run figure carries it.
    let first_check = Duration::from_secs_f64(1.0 / config.rate.max(f64::MIN_POSITIVE));
    if config.rate > 0.0 && locations > 0 {
        let full = ramp + Duration::from_secs_f64(locations as f64 / config.rate);
        println!(
            "  {locations} locations per slot at {}/s: first check ~{} in, ~{} for a fresh run",
            config.rate,
            humanize(first_check),
            humanize(full)
        );
        if full > config.timeout {
            println!(
                "  NOTE: --timeout is {}, so this run would be cut short unless the room is \
                 already part-played",
                humanize(config.timeout)
            );
        }
    }
    // **The per-slot rate is not the room's load, and the difference is two multiplications.**
    // Every check the room accepts is announced to every connection, so the work it does is the
    // product, and a rate that reads as gentle per player is not gentle at two hundred of them.
    let connections = plans.len() * config.clients_per_slot;
    println!(
        "  ~{:.1} checks/s offered in total; each is announced to every one of {connections} \
         connections, so ~{:.0} messages/s to deliver",
        config.rate * plans.len() as f64,
        config.rate * plans.len() as f64 * connections as f64
    );
    if config.clients_per_slot > 1 {
        println!(
            "  {} sockets per slot: {}",
            config.clients_per_slot,
            load::Role::EVERY
                .iter()
                .take(config.clients_per_slot)
                .map(|r| format!("{r:?}"))
                .collect::<Vec<_>>()
                .join(" + ")
        );
    }

    let totals = Arc::new(Totals::default());
    let finished = Arc::new(AtomicBool::new(false));
    // Sized to the playing slots alone: an observer never waits at the gate, having nothing to
    // start doing. A barrier expecting arrivals that never come would hold the run open.
    let start = Arc::new(Barrier::new(plans.len()));

    // **Interleaved per slot, not role by role.** A player opens their game client, their text
    // client and their tracker together, so this is both the realistic arrival order and the one
    // that grows the population evenly; three separate waves would put every tracker at the end.
    let mut tasks = Vec::new();
    for (i, plan) in plans.into_iter().enumerate() {
        let slot = plan.slot;
        for (r, role) in load::Role::EVERY
            .iter()
            .take(config.clients_per_slot)
            .enumerate()
        {
            let schedule = schedules[i * config.clients_per_slot + r];
            tasks.push(match role {
                load::Role::Player => tokio::spawn(load::play(
                    plan.clone(),
                    Arc::clone(&config),
                    Arc::clone(&totals),
                    Arc::clone(&finished),
                    Arc::clone(&start),
                    // A per-slot seed, so slots burst independently rather than in lockstep.
                    0xC0FFEE ^ u64::from(slot) ^ (i as u64) << 32,
                    schedule,
                )),
                other => tokio::spawn(load::observe(
                    plan.clone(),
                    *other,
                    Arc::clone(&config),
                    Arc::clone(&totals),
                    Arc::clone(&finished),
                    schedule,
                )),
            });
        }
    }

    let watcher = tokio::spawn(watch(
        Arc::clone(&totals),
        Arc::clone(&finished),
        Arc::clone(&config),
        players as u64,
        sockets as u64,
        first_check,
    ));

    for task in tasks {
        match task.await {
            Ok(Ok(())) => {}
            // One slot failing is worth saying and not worth stopping the run for: the interesting
            // case is usually the other forty-nine still playing.
            Ok(Err(e)) => tracing::warn!("a slot ended early: {e:#}"),
            Err(e) => tracing::warn!("a slot panicked: {e}"),
        }
    }
    finished.store(true, Ordering::Relaxed);
    let _ = watcher.await;

    let drops = totals.drops.load(Ordering::Relaxed);
    let reconnects = totals.reconnects.load(Ordering::Relaxed);
    let deflated = totals.deflated.load(Ordering::Relaxed);
    let opened = totals.opened.load(Ordering::Relaxed);
    // **Stated rather than assumed.** Whether the room could share compression with these clients
    // decides what every outbound number means, and it is settled by a handshake header nobody
    // sees. A room on an image without the extension answers 0 here and nothing else changes.
    //
    // **Against connections opened, not against the population.** Reconnection made the bare count
    // misleading: a run that lost ten connections and got them back opened twenty, and printing
    // "20 negotiated" under a line reading `connected 10/10` reads as a contradiction.
    println!(
        "  {deflated} of {opened} connections opened negotiated permessage-deflate{}",
        if deflated == 0 {
            " -- the room compressed nothing for us, so outbound bytes are a worst case"
        } else {
            ""
        }
    );
    println!(
        "done: {} checks sent, {} items received, {}/{players} goaled",
        totals.checks_sent.load(Ordering::Relaxed),
        totals.items_received.load(Ordering::Relaxed),
        totals.goaled.load(Ordering::Relaxed)
    );
    if drops > 0 {
        // Said plainly at the end, because a room that sheds is the interesting result and the
        // numbers above are what the population that was up at the time managed. The room's own
        // `pahoa_lag_disconnects_total` is where to confirm it was backpressure rather than
        // something else.
        //
        // **Two numbers, because reconnection made one insufficient.** How often the room dropped
        // somebody is a measure of the room; how many never came back is a measure of the run. A
        // run with 545 drops that ended full is a room that shed and recovered, which used to be
        // indistinguishable here from one that lost 545 slots for good.
        //
        // **From the two counters, never from `connected`**, which is zero by the time this runs:
        // every task has been joined and every connection closed cleanly on the way out. Reading it
        // here reported a full recovery as "10 of 10 were still down at the end".
        let down = drops.saturating_sub(reconnects);
        println!(
            "  {drops} connection drops, {reconnects} recovered; {}",
            if down == 0 {
                "every connection was up when the run ended".to_string()
            } else {
                format!("{down} of {sockets} never came back")
            }
        );
    }
    Ok(())
}

/// Print progress, and decide when the run is over.
///
/// **The linger is the interesting part of the run, not politeness.** When the last player goals,
/// auto-release empties every unfinished world at once — the biggest `to_slot` burst a room ever
/// produces. Closing at the moment of the last goal would measure everything except it.
async fn watch(
    totals: Arc<Totals>,
    finished: Arc<AtomicBool>,
    config: Arc<Config>,
    players: u64,
    // Connections the run means to hold -- slots times `--clients-per-slot`, not players.
    sockets: u64,
    first_check: Duration,
) {
    let began = std::time::Instant::now();
    let mut all_goaled_at: Option<std::time::Instant> = None;
    let mut ticker = tokio::time::interval(Duration::from_secs(2));

    loop {
        ticker.tick().await;
        if finished.load(Ordering::Relaxed) {
            return;
        }
        let goaled = totals.goaled.load(Ordering::Relaxed);
        // **The drop count is on every line, not only in the warnings above it.** A room that drops
        // most of the run still leaves the totals climbing, so without this the display reads as a
        // full house doing less work rather than as a smaller run doing its share.
        //
        // Since connections reconnect, the number is a count of *events* and the population is
        // `connected` beside it, which is why that is now shown against the total rather than
        // alone. `connected 1455/2000` with `drops 545` is a room shedding and a harness coming
        // back; `connected 2000/2000` with the same drops is a room that shed and recovered, and
        // those are different runs that used to print the same line.
        let drops = totals.drops.load(Ordering::Relaxed);
        let sent = totals.checks_sent.load(Ordering::Relaxed);
        // **While nothing has been sent yet, say when something will be.** This is the whole of the
        // "it just sits there" report: a long ramp and a slow rate mean minutes of `checks 0` on a
        // run doing exactly what it was told, and a number counting down is the difference between
        // waiting and wondering.
        let waiting = if sent == 0 && began.elapsed() < first_check {
            format!(
                "  (first check in ~{})",
                humanize(first_check - began.elapsed())
            )
        } else {
            String::new()
        };
        println!(
            "  {:>5}s  connected {}/{sockets}{}  checks {sent}  items {}  goaled \
             {goaled}/{players}{waiting}",
            began.elapsed().as_secs(),
            totals.connected.load(Ordering::Relaxed),
            if drops > 0 {
                format!(
                    " (drops {drops}, back {})",
                    totals.reconnects.load(Ordering::Relaxed)
                )
            } else {
                String::new()
            },
            totals.items_received.load(Ordering::Relaxed),
        );
        // **Flushed, because stdout to a file is block-buffered.** A run of any length is one you
        // want to `tee` or watch through a pipe, and without this the progress arrives in 4 KiB
        // lumps, or, for a run that is killed, not at all.
        let _ = std::io::Write::flush(&mut std::io::stdout());

        if goaled >= players && players > 0 {
            match all_goaled_at {
                None => {
                    println!(
                        "  every player has goaled; draining the release cascade for {:?}",
                        config.linger
                    );
                    all_goaled_at = Some(std::time::Instant::now());
                }
                Some(at) if at.elapsed() >= config.linger => {
                    finished.store(true, Ordering::Relaxed);
                    return;
                }
                Some(_) => {}
            }
        }

        if began.elapsed() >= config.timeout {
            tracing::warn!("timed out after {:?}; stopping", config.timeout);
            finished.store(true, Ordering::Relaxed);
            return;
        }
    }
}

/// Read slot names, games and Goal ids out of the generation.
///
/// **With `pahoa-multidata`**, the same parser Puna and pahoa use, so what this believes about a
/// room cannot drift from what the room believes. A second reader here would be a second thing to
/// keep in step with a format neither repository owns.
fn read_generation(path: &str) -> Result<(Vec<SlotPlan>, usize)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes))
        .with_context(|| format!("{path} is not a zip"))?;
    let name = (0..archive.len())
        .map(|i| archive.by_index(i).expect("member").name().to_string())
        .find(|n| n.to_ascii_lowercase().ends_with(".archipelago"))
        .ok_or_else(|| anyhow::anyhow!("{path} has no .archipelago member"))?;

    let mut raw = Vec::new();
    archive.by_name(&name)?.read_to_end(&mut raw)?;
    let data = MultiData::parse(&raw).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;

    // The Goal id is per GAME -- two slots playing one game share it, and the item's receiver is
    // what makes an arrival theirs.
    let goal_of = |game: &str| {
        data.embedded_datapackage
            .get(game)
            .and_then(|pkg| pkg.item_name_to_id.get(GOAL_ITEM).copied())
    };

    let mut plans: Vec<SlotPlan> = data
        .connectable_slots()
        .map(|(slot, info)| SlotPlan {
            slot: *slot,
            name: info.name.clone(),
            game: info.game.clone(),
            // A spectator has no Goal and never will; a player without one is a seed this tool
            // cannot finish, which is reported below rather than discovered as a hang.
            goal_item: (info.slot_type == SlotType::Player)
                .then(|| goal_of(&info.game))
                .flatten(),
        })
        .collect();
    plans.sort_by_key(|p| p.slot);

    if plans.is_empty() {
        bail!("{path} has no connectable slots");
    }
    if let Some(orphan) = plans
        .iter()
        .find(|p| p.goal_item.is_none() && data.slot_info[&p.slot].slot_type == SlotType::Player)
    {
        bail!(
            "slot {} plays {} which has no {GOAL_ITEM} item -- this seed was not built by \
             make-generation, and a run against it could never end",
            orphan.slot,
            orphan.game
        );
    }
    // The largest world in the seed, which is what a fresh run's length is bounded by. Read from
    // the multidata rather than counted after connecting, because the estimate is wanted at
    // startup, and a resumed run against a part-played room will finish sooner than it says.
    let locations = plans
        .iter()
        .map(|p| data.locations.count_for(p.slot))
        .max()
        .unwrap_or(0);

    Ok((plans, locations))
}

/// A duration a person reads at a glance rather than `Duration`'s own debug shape.
fn humanize(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 90.0 {
        format!("{secs:.0}s")
    } else if secs < 5400.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{:.1}h", secs / 3600.0)
    }
}

/// `all`, `1,3,5`, `1-8`, or any combination of the last two.
fn select(spec: &str, plans: &[SlotPlan]) -> Result<BTreeSet<u32>> {
    if spec.eq_ignore_ascii_case("all") {
        return Ok(plans.iter().map(|p| p.slot).collect());
    }
    let mut wanted = BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        match part.split_once('-') {
            Some((from, to)) => {
                let from: u32 = from.trim().parse().with_context(|| format!("{part:?}"))?;
                let to: u32 = to.trim().parse().with_context(|| format!("{part:?}"))?;
                if from > to {
                    bail!("{part:?} counts backwards");
                }
                wanted.extend(from..=to);
            }
            None => {
                wanted.insert(part.parse().with_context(|| format!("{part:?}"))?);
            }
        }
    }
    let known: BTreeSet<u32> = plans.iter().map(|p| p.slot).collect();
    // Naming a slot the seed does not have is a typo worth reporting, not silently playing the
    // rest -- a run that quietly played eight of the nine slots asked for would look like a room
    // that was slower than it is.
    if let Some(missing) = wanted.difference(&known).next() {
        bail!("this generation has no slot {missing}");
    }
    Ok(wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plans(slots: &[u32]) -> Vec<SlotPlan> {
        slots
            .iter()
            .map(|slot| SlotPlan {
                slot: *slot,
                name: format!("slot{slot}"),
                game: "Gloomhaven Drift".into(),
                goal_item: Some(1),
            })
            .collect()
    }

    #[test]
    fn slot_selection_reads_lists_and_ranges() {
        let all = plans(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(select("all", &all).unwrap().len(), 8);
        assert_eq!(select("1,3,5", &all).unwrap(), BTreeSet::from([1, 3, 5]));
        assert_eq!(select("2-4", &all).unwrap(), BTreeSet::from([2, 3, 4]));
        assert_eq!(
            select("1, 3-5 , 8", &all).unwrap(),
            BTreeSet::from([1, 3, 4, 5, 8])
        );
    }

    /// A slot the seed does not have is a typo, and playing the rest anyway would look like a room
    /// that is slower than it is.
    #[test]
    fn selecting_a_slot_that_does_not_exist_is_an_error() {
        let all = plans(&[1, 2, 3]);
        assert!(select("4", &all).is_err());
        assert!(select("1-5", &all).is_err());
        assert!(select("3-1", &all).is_err(), "a backwards range");
        assert!(select("one", &all).is_err());
    }
}
