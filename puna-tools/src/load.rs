//! Playing a synthetic seed against a running room.
//!
//! One connection per slot, each sending location checks at a bursty rate until it receives its own
//! `Goal` item, then holding the connection open and draining until everybody else is done too.
//!
//! ## The protocol structs are local, and that is not laziness
//!
//! `pahoa-proto` has all of these — and in the wrong directions. Its `ClientPacket` is
//! `Deserialize` (the server reads it) and its `ServerPacket` is `Serialize` (the server writes
//! it), which is exactly the opposite of what a client needs, so borrowing them would mean
//! fighting the derives to save forty lines.
//!
//! Reading is deliberately **lenient**: every server packet this does not care about is skipped by
//! `cmd`, and unknown fields on the ones it does care about are ignored. A load tool that fell over
//! because the room grew a field would be a load tool that stops working the week somebody adds
//! one.

use anyhow::{Context, Result, anyhow, bail};
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Barrier;

/// Archipelago's `ClientStatus::Goal`.
const STATUS_GOAL: u8 = 30;

/// Receive items, receive starting inventory, receive from own world — everything.
///
/// **The default is 7 on purpose.** Checks flowing *from* slots make other slots' items flow *to*
/// them, and that return direction is the firehose the filtered feed exists for and the one the
/// outbound counters measure. A load tool that only sent would exercise half the server.
pub const ITEMS_HANDLING_ALL: u8 = 0b111;

/// The version a client claims.
///
/// **0.6.7 because that is what upstream has actually released.** pahoa is moving its own
/// `SERVER_VERSION` to the same value until 0.6.8 ships, and the two are not independent: a room
/// running `compatibility = 0` — tournament mode — refuses any client whose version is not
/// **exactly** the server's (`pahoa-room/src/room.rs:505`, `IncompatibleVersion`). Under the
/// default `compatibility = 2` only the seed's floor applies, which is `MIN_CLIENT_VERSION` = 0.5.0
/// for any generator at or past 0.6.2.
///
/// So: matching pahoa exactly is what keeps this usable against a tournament-mode room, and the
/// floor is what keeps it usable everywhere else. The floor half is asserted against the pinned
/// crate rather than trusted; see `the_client_version_clears_pahoas_floor`. Once pahoa's move
/// lands and the pin catches up, this is a candidate for reading `pahoa_room::SERVER_VERSION`
/// directly — a fact from the same rev beats a number transcribed from another repository.
const CLIENT_VERSION: (u32, u32, u32) = (0, 6, 7);

/// How long a rate holds on average. Bursts happen inside it; the average comes out over it.
const WINDOW: Duration = Duration::from_secs(10);

/// Sub-intervals a window is dealt across.
const TICKS_PER_WINDOW: u32 = 10;

/// How long a connected slot waits for the others **when there is no ramp** — see [`schedule`],
/// which is where the gate does or does not exist.
///
/// The grace is there because one slot that cannot connect must not hold the rest: a bare barrier
/// turns a single refused connection into a hang, which reads as a broken tool rather than as the
/// one error it is.
const START_GRACE: Duration = Duration::from_secs(30);

/// Connections opened per second when nothing says otherwise. See [`schedule`].
pub const DEFAULT_CONNECT_RATE: f64 = 5.0;

/// The first wait after a connection is lost. See [`reconnect_delay`].
const RECONNECT_BASE: Duration = Duration::from_millis(500);

/// The longest wait between attempts, and also the length a session has to reach before it counts
/// as having been a stable connection. See [`Backoff::held`].
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// How long to wait before attempt `attempt + 1`, doubling and jittered.
///
/// **A real client reconnects, so a load tool that does not is measuring a room that is emptying.**
/// Every drop used to end that slot for the rest of the run, which quietly changed what was being
/// measured: 545 of 2000 gone means every later number is per-1455, and the room was serving a
/// population that only ever shrank. A room shedding load is *supposed* to see the shed clients come
/// back — that is the interesting half of backpressure, and none of it was being exercised.
///
/// **Jittered, and that is not decoration.** The event that drops connections drops many at once: a
/// goal cascade shed 545 in twelve seconds. Undelayed, all 545 redial in the same instant — the
/// connect storm [`schedule`] exists to avoid, aimed at a room that has just demonstrated it is
/// already at its limit. Equal jitter (half the window fixed, half uniform) spreads them without
/// letting the first retry land at zero.
pub fn reconnect_delay(attempt: u32, rng: &mut impl Rng) -> Duration {
    // Saturating rather than wrapping: at attempt 64 a shift would panic in debug and wrap to a
    // one-nanosecond backoff in release, which is a reconnect storm produced by an integer.
    let window = RECONNECT_BASE
        .saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX))
        .min(RECONNECT_MAX);
    let half = window / 2;
    half + half.mul_f64(rng.r#gen::<f64>())
}

/// Wait, unless the run ends first. `false` means it did.
///
/// Polled rather than notified because `finished` is a bare flag shared with the progress watcher,
/// and a bare `sleep` of the full backoff would hold the process open for up to
/// [`RECONNECT_MAX`] after the last goal — a tool that appears to hang at the end of a run it has
/// already finished.
async fn hold(delay: Duration, finished: &AtomicBool) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if finished.load(Ordering::Relaxed) {
            return false;
        }
        let slice = (deadline - Instant::now()).min(Duration::from_millis(250));
        tokio::time::sleep(slice).await;
    }
    !finished.load(Ordering::Relaxed)
}

/// Why one connection's session ended.
enum Ended {
    /// The run is over. Nothing to reconnect to.
    RunOver,
    /// The connection went away and the run has not finished.
    Lost,
}

/// What to do after one session ended.
enum Retry {
    /// Dial again — the wait has already been served.
    Again,
    /// The run is over, or it ended while waiting.
    Stop,
}

/// Decide what happens after a session, and serve the backoff when the answer is "again".
///
/// **Every ending except `RunOver` is retried, deliberately, including a refusal.** Troy's rule is
/// that a connection always comes back if the room dropped it before we were done, and the tool is
/// in no position to decide which refusals are permanent: a room mid-restart refuses for a few
/// seconds and then does not, and a room that refuses forever is answered by the backoff reaching
/// its 30-second ceiling — a slot knocking politely rather than hammering.
///
/// The logging is loud once and quiet after, because at two thousand connections a warning per
/// attempt is a wall of text that hides the first one. The first loss of a stable connection is the
/// interesting line; the ninth consecutive failure to redial is not.
///
/// **A free function taking the state rather than a `reconnecting(|| session())` combinator**,
/// which is what this was first. `AsyncFnMut` puts no `Send` bound on the future it returns, so a
/// closure borrowing the slot's own state — which is the entire point of the loop — produced
/// *"implementation of `Send` is not general enough"* at every `tokio::spawn`: an error in the
/// caller, about lifetimes, naming nothing that is actually wrong. The loop is six lines at each of
/// the two call sites and they cannot drift, because everything that decides anything is here.
async fn retry(
    what: &str,
    outcome: Result<Ended>,
    lasted: Duration,
    backoff: &mut Backoff,
    rng: &mut StdRng,
    finished: &AtomicBool,
) -> Retry {
    backoff.held(lasted);

    match outcome {
        Ok(Ended::RunOver) => return Retry::Stop,
        Ok(Ended::Lost) if finished.load(Ordering::Relaxed) => return Retry::Stop,
        Ok(Ended::Lost) => {
            if backoff.attempt == 0 {
                tracing::warn!("{what} lost its connection; reconnecting");
            } else {
                tracing::debug!("{what} lost its connection again");
            }
        }
        Err(e) => {
            if backoff.attempt == 0 {
                tracing::warn!("{what}: {e:#}; reconnecting");
            } else {
                tracing::debug!("{what}: {e:#}");
            }
        }
    }

    if hold(backoff.wait(rng), finished).await {
        Retry::Again
    } else {
        Retry::Stop
    }
}

/// The backoff's own rng: `Send`, and separate from a slot's seeded traffic rng.
///
/// Separate so a reconnect does not consume draws the traffic shape depends on — `--seed`
/// reproducibility is already lost the moment a drop happens, and the failure path should not take
/// more of it than the failure did. `ThreadRng` would do neither: it is not `Send`, and this is held
/// across an await inside a spawned task.
fn backoff_rng() -> StdRng {
    StdRng::from_entropy()
}

/// One connection's place in the backoff.
struct Backoff {
    attempt: u32,
}

impl Backoff {
    fn new() -> Self {
        Self { attempt: 0 }
    }

    fn wait(&mut self, rng: &mut impl Rng) -> Duration {
        let delay = reconnect_delay(self.attempt, rng);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// A session ended after `lasted`. Reset only if it was a connection worth having.
    ///
    /// **Not "reset whenever the handshake succeeded"**, which is the obvious rule and is wrong for
    /// the case this exists for. A room shedding load accepts a connection and drops it again
    /// moments later; resetting on the accept would put that slot into a 500 ms redial loop against
    /// a room that has just said it cannot cope — a load tool converting a shed into a storm. A
    /// session shorter than the longest backoff was not a stable connection, so it escalates.
    fn held(&mut self, lasted: Duration) {
        if lasted >= RECONNECT_MAX {
            self.attempt = 0;
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub room: String,
    pub password: Option<String>,
    /// Connections opened per second across the whole run. 0 opens them all at once.
    pub connect_rate: f64,
    /// Sockets per slot, 1 to 3: the game client, then a text client, then a tracker.
    pub clients_per_slot: usize,
    /// Checks per second **per slot**, not for the room. See [`plan_window`].
    pub rate: f64,
    /// 0 is a flat metronome; 1 is as bursty as this gets.
    pub jitter: f64,
    /// Locations per `LocationChecks` packet.
    pub batch: usize,
    pub items_handling: u8,
    /// Chat lines per second per slot, for exercising a room's filters. Off by default.
    pub say_rate: f64,
    /// How long to keep draining after the last slot goals.
    pub linger: Duration,
    /// Backstop, so a room that misbehaves cannot hang the run.
    pub timeout: Duration,
}

/// What a connection is pretending to be.
///
/// **One player commonly holds three sockets** — the game client, a text client and a tracker —
/// which is why `clients_connected` counts sockets rather than players, and why a load run with one
/// connection per slot understates a real room's fan-out by roughly a third of what it should be.
///
/// The difference is entirely **tags**. `TextOnly` and `Tracker` are Archipelago's non-game tags
/// (`MultiServer.py:956`): carried together with an empty `game`, they skip the game and per-slot
/// version checks (`Client::ignores_game`), and they make the connection `no_locations` — pahoa
/// refuses a check or a goal from one by name. So these two connect, consume the firehose and
/// answer heartbeats, which is exactly the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The game client: checks locations, receives items, goals.
    Player,
    /// `!hint` and chat, and the reference client's own choice of `items_handling`.
    Text,
    /// A tracker watching the same slot.
    Tracker,
}

impl Role {
    /// In connect order, so a second connection is the text client and a third is the tracker.
    pub const EVERY: [Role; 3] = [Role::Player, Role::Text, Role::Tracker];

    fn tags(self) -> Vec<&'static str> {
        match self {
            // `CommonContext.tags` is `{"AP"}`, and each client adds its own.
            Role::Player => vec!["AP"],
            Role::Text => vec!["AP", "TextOnly"],
            Role::Tracker => vec!["AP", "Tracker"],
        }
    }

    /// **Empty for the non-game roles, and that is what makes the tag work.** `ignores_game` needs
    /// *both* an absent game and a non-game tag; naming the game here would put a text client back
    /// under the slot's own version floor for no benefit.
    fn game(self, plan: &SlotPlan) -> &str {
        match self {
            Role::Player => &plan.game,
            Role::Text | Role::Tracker => "",
        }
    }
}

/// What one slot needs to play.
#[derive(Debug, Clone)]
pub struct SlotPlan {
    pub slot: u32,
    pub name: String,
    pub game: String,
    /// The item id that means "you are finished", for this slot's game.
    ///
    /// **`None` for a spectator**, which owns no locations and can never goal. An `Option` rather
    /// than a sentinel because the two states behave differently in three places and a magic id
    /// made them look like one: a spectator connects with nothing to check, and the first version
    /// of this read that as "already finished" and counted it — reporting `goaled 14/12` on a
    /// twelve-player room, and ending runs on a quorum that included slots which could not play.
    pub goal_item: Option<i64>,
}

/// Counters shared across every slot's task, for the progress line.
#[derive(Default)]
pub struct Totals {
    pub checks_sent: AtomicU64,
    pub items_received: AtomicU64,
    pub goaled: AtomicU64,
    /// Slots connected **right now** — it goes down again.
    ///
    /// **It only ever went up, and that hid a room dropping most of the run.** A run that lost 165
    /// of its 200 slots printed `connected 200` on every line afterwards, so the progress display
    /// asserted a full house while the warnings scrolled past above it. A load tool that
    /// misreports its own population is worse than one that reports nothing: every rate below it
    /// is then read as per-200-slots when it is per-35.
    pub connected: AtomicU64,
    /// **Times a connection was lost**, not connections that are gone.
    ///
    /// The distinction arrived with reconnection: a drop is now an event rather than an ending, so
    /// this accumulates and `connected` recovers. Read the two together — `drops` says how hard the
    /// room was shedding, `connected` says how much of the population is there now, and neither
    /// answers the other's question.
    pub drops: AtomicU64,
    /// Times a lost connection came back.
    ///
    /// **`drops - reconnects` is how many are down right now**, and that is the only way to ask the
    /// question after the run: `connected` is zero once the tasks have been joined, because every
    /// connection closed cleanly on the way out. Both counters move only on the unclean path, so
    /// the subtraction cannot drift — a clean ending touches neither.
    pub reconnects: AtomicU64,
    /// Connections opened, counting every redial.
    ///
    /// Not the population, which is `connected`, and not the slot count either. It exists so the
    /// deflate line has an honest denominator: a run that reconnected ten times opened twenty
    /// connections, and "20 negotiated permessage-deflate" beside "connected 10/10" reads as a
    /// contradiction without it.
    pub opened: AtomicU64,
    /// Connections that negotiated permessage-deflate.
    ///
    /// Worth counting rather than assuming: it is the difference between a run whose outbound
    /// bytes resemble real players and one that is the worst case a room can be asked to serve,
    /// and it is decided by a handshake header nobody sees. `pahoa_client_connections_total`
    /// reports the same split from the room's side.
    pub deflated: AtomicU64,
}

/// Keeps [`Totals::connected`] honest for the life of one slot's connection.
///
/// A guard rather than a pair of counter calls because `play` returns through a dozen `?`s, and
/// the one thing that must happen on every one of them is this. The failure it prevents is silent:
/// a count that only rises still *looks* like a working display.
struct Connection<'a> {
    totals: &'a Totals,
    cleanly: bool,
}

impl<'a> Connection<'a> {
    /// `been_up` is whether this connection has ever been established before, and it is set here.
    ///
    /// **Not "is this the first attempt", which is what it was and which counted `back` higher than
    /// `drops`.** A slot whose opening dial fails and whose second succeeds has not *re*connected —
    /// it has connected, late — but the attempt counter had already moved off its first value, so
    /// the guard was created looking like a recovery from a drop that never happened. Observed as
    /// `(drops 20436, back 20437)` on a 2000-connection run: one number that must bound the other,
    /// exceeding it by exactly the number of slots that stumbled on their way in.
    ///
    /// Written this way, `reconnects <= drops` holds by construction: nothing counts a return
    /// except a session that follows an established one, and every established session that ends
    /// unclean counts a drop.
    fn opened(totals: &'a Totals, been_up: &mut bool) -> Self {
        totals.connected.fetch_add(1, Ordering::Relaxed);
        if *been_up {
            totals.reconnects.fetch_add(1, Ordering::Relaxed);
        }
        *been_up = true;
        Self {
            totals,
            cleanly: false,
        }
    }

    /// The run ended and this slot let go of its own accord — not a drop.
    fn closing(&mut self) {
        self.cleanly = true;
    }
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        self.totals.connected.fetch_sub(1, Ordering::Relaxed);
        if !self.cleanly {
            self.totals.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What one slot knows about itself, across however many connections it takes to finish.
///
/// **The point of the struct is that these three outlive a socket and `remaining` does not.** A
/// reconnect re-derives what is left to check from the room's own `Connected`, exactly as a resumed
/// run does — the room is authoritative and the tool's idea of the list is not. What cannot be
/// re-derived is what this slot has already *told the run*, and counting any of it twice corrupts
/// the numbers the run exists to produce.
#[derive(Default)]
struct Session {
    /// Locations still to send, from the current connection's `Connected`.
    remaining: Vec<i64>,
    /// Whether this slot has its Goal. Survives a reconnect: the room does too, in its save.
    goaled: bool,
    /// Whether the run's goal tally has been told, which must happen **once**.
    ///
    /// Separate from `goaled` because the two answer different questions, and conflating them ends
    /// the run early: `totals.goaled >= players` is the run's terminating condition, so a slot that
    /// counted itself twice after a reconnect would stop the run with somebody still playing.
    counted_goal: bool,
    /// How many items this slot has contributed to [`Totals::items_received`].
    ///
    /// **A reconnect replays the slot's entire item history from index zero**, which is how a
    /// resumed slot learns it already won — so the replay after a drop is everything already
    /// counted, plus whatever arrived while the connection was down. Adding it whole would inflate
    /// the run's item total by a slot's history per drop, and that total is the one number checkable
    /// against the room's own tracker. See [`Session::absorb_replay`].
    items_counted: usize,
    /// Whether this slot has already waited at the start gate. It waits at most once.
    ///
    /// A `Barrier` counts arrivals, so a reconnecting slot that waited again would arrive twice and
    /// release the gate for a slot that had not.
    gated: bool,
}

impl Session {
    /// Count the connect-time replay, without counting any of it twice.
    ///
    /// Returns how many were newly counted. The replay is a prefix-superset of what has been seen —
    /// the room replays from index zero every time — so the new items are whatever is past the
    /// high-water mark.
    fn absorb_replay(&mut self, replayed: usize, totals: &Totals) -> usize {
        let fresh = replayed.saturating_sub(self.items_counted);
        self.items_counted = self.items_counted.max(replayed);
        totals
            .items_received
            .fetch_add(fresh as u64, Ordering::Relaxed);
        fresh
    }

    /// Count items that arrived on a live connection.
    fn absorb_live(&mut self, items: usize, totals: &Totals) {
        self.items_counted += items;
        totals
            .items_received
            .fetch_add(items as u64, Ordering::Relaxed);
    }

    /// Record the goal, and say whether this is the first time.
    fn goal(&mut self) -> bool {
        self.goaled = true;
        let first = !self.counted_goal;
        self.counted_goal = true;
        first
    }
}

/// When one slot dials, and — only when everybody dials at once — how long it waits for the rest.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    pub connect_at: Instant,
    /// `None` means start checking as soon as this slot is connected. See [`schedule`].
    pub gate_until: Option<Instant>,
}

/// Deal the connects out over a ramp, one [`Schedule`] per slot.
///
/// **The first live run opened 200 connections at once and the room dropped 165 of them.** It was
/// not the room: the pod peaked at 0.015 cores against a 2-core limit with zero throttled periods,
/// and every drop landed in the one two-minute bucket where the connections were arriving. Each
/// arrival fans out to everybody already connected *and* replays the newcomer's whole item history,
/// so the cost of filling a room all at once grows with the square of its size — outbound queued
/// bytes reached 31.7 MiB against a 64 MiB budget shared across every connection.
///
/// A ramp is also simply what a room looks like: players arrive over minutes, not in one frame. So
/// the default is a ramp and the storm is the thing that has to be asked for — `0`, which is kept
/// because reproducing the storm deliberately is a legitimate measurement rather than a mistake.
///
/// **A ramp replaces the start gate rather than sitting in front of it**, which is Troy's call and
/// the right one. The gate — everybody connects, nobody checks until the last one is in — was
/// written before the ramp existed, to stop a run being measured through a staircase of arrivals.
/// A ramp *is* a staircase, deliberately, so keeping both bought two bad things and nothing else:
/// a dead period the length of the ramp (**6.7 minutes at 2000 slots**, during which the tool looks
/// stuck), and then a synchronized start with every slot beginning its first window in the same
/// instant — the herd shape [`tick_phase`] exists to break up.
///
/// Without the gate, load builds with the population, which is what a room filling up actually
/// looks like.
///
/// **The gate survives for `connect_rate = 0`**, where there is no ramp to replace it: every slot
/// dials at once, so waiting for the rest is the only way the run has a defined start. It is a
/// timed wait rather than a barrier alone, because one slot that cannot connect must not hold the
/// others forever.
pub fn schedule(origin: Instant, slots: usize, connect_rate: f64) -> Vec<Schedule> {
    let ramped = connect_rate > 0.0;
    let spacing = if ramped {
        Duration::from_secs_f64(1.0 / connect_rate)
    } else {
        Duration::ZERO
    };
    (0..slots)
        .map(|i| Schedule {
            connect_at: origin + spacing.mul_f64(i as f64),
            gate_until: (!ramped).then(|| origin + START_GRACE),
        })
        .collect()
}

// ---- what a client sends ----------------------------------------------------------------------

#[derive(Serialize)]
struct Version {
    major: u32,
    minor: u32,
    build: u32,
    class: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "cmd")]
enum Outbound<'a> {
    Connect {
        password: Option<&'a str>,
        game: &'a str,
        name: &'a str,
        uuid: &'a str,
        version: Version,
        items_handling: u8,
        tags: Vec<&'a str>,
        slot_data: bool,
    },
    LocationChecks {
        locations: Vec<i64>,
    },
    StatusUpdate {
        status: u8,
    },
    Say {
        text: String,
    },
}

/// Packets travel as an **array**, in both directions, even when there is one of them.
fn frame(packets: &[Outbound<'_>]) -> Result<String> {
    Ok(serde_json::to_string(packets)?)
}

// ---- what a client reads ----------------------------------------------------------------------

#[derive(Deserialize)]
struct Item {
    item: i64,
}

#[derive(Deserialize)]
#[serde(tag = "cmd")]
enum Inbound {
    Connected {
        slot: u32,
        missing_locations: Vec<i64>,
        checked_locations: Vec<i64>,
    },
    ConnectionRefused {
        #[serde(default)]
        errors: Vec<String>,
    },
    ReceivedItems {
        items: Vec<Item>,
    },
    /// Everything else. Skipped by shape rather than enumerated, so a room that grows a packet
    /// does not break a tool that never cared about it.
    #[serde(other)]
    Other,
}

/// Deal one window's budget across its ticks.
///
/// A flat check every `1/rate` seconds is the one traffic shape a real room never produces, and it
/// is the shape that makes a queue look healthy — nothing ever arrives together, so nothing ever
/// queues. So the rate holds **on average over the window** and swings hard inside it.
///
/// Weights are `(-ln U)^k` normalized, which is a Dirichlet draw in one line: `k = 1` is the
/// ordinary exponential spread, larger `k` concentrates the budget into fewer ticks. `jitter = 0`
/// short-circuits to flat, kept so a run can be made boring deliberately.
pub fn plan_window(budget: u32, jitter: f64, rng: &mut impl Rng) -> Vec<u32> {
    let ticks = TICKS_PER_WINDOW as usize;
    if jitter <= 0.0 || budget == 0 {
        let base = budget / TICKS_PER_WINDOW;
        let extra = budget % TICKS_PER_WINDOW;
        return (0..ticks)
            .map(|i| base + u32::from((i as u32) < extra))
            .collect();
    }

    let k = 1.0 + jitter * 4.0;
    let weights: Vec<f64> = (0..ticks)
        .map(|_| {
            let u: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
            (-u.ln()).powf(k)
        })
        .collect();
    let total: f64 = weights.iter().sum();

    // Largest-remainder, so the window's budget is spent exactly rather than drifting with
    // rounding: the whole promise of this function is that the average comes out true.
    let scaled: Vec<f64> = weights
        .iter()
        .map(|w| w / total * f64::from(budget))
        .collect();
    let mut counts: Vec<u32> = scaled.iter().map(|v| *v as u32).collect();
    let mut remainder = budget - counts.iter().sum::<u32>();
    let mut order: Vec<usize> = (0..ticks).collect();
    order.sort_by(|a, b| {
        let fa = scaled[*a] - scaled[*a].floor();
        let fb = scaled[*b] - scaled[*b].floor();
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    for i in order {
        if remainder == 0 {
            break;
        }
        counts[i] += 1;
        remainder -= 1;
    }
    counts
}

/// One slot's place in its own schedule: how many windows have passed, and how many checks it has
/// been given across all of them.
#[derive(Default)]
struct Pace {
    windows: u32,
    granted: u32,
}

/// How many checks this window gets, from the rate's own running total.
///
/// **A window's budget is a whole number of checks, and rounding it away silently stopped the tool
/// dead.** `--rate 0.01` is one check per slot every hundred seconds — a perfectly reasonable soak
/// — and against a ten-second window that is a budget of `0.1`, which rounded to **zero**. Every
/// tick then wanted nothing, forever: the slots connected, the connections stayed up, the progress
/// line counted `checks 0`, and nothing anywhere explained why. Every rate below 0.05 did this.
///
/// Computed as *how many are owed by now, minus how many have been handed out* rather than by
/// accumulating a remainder — which is the same idea with a bug in it: adding `0.1` ten times gives
/// `0.9999999999999999`, so the first check of a 0.01 run would arrive a window late and every
/// hundredth one after that would slip again. One multiplication against the window count cannot
/// drift.
fn window_budget(rate: f64, pace: &mut Pace) -> u32 {
    pace.windows += 1;
    let due = (rate.max(0.0) * WINDOW.as_secs_f64() * f64::from(pace.windows)).floor();
    // Saturating because a rate cannot go backwards, but a caller could hand this one that did.
    let budget = (due as u32).saturating_sub(pace.granted);
    pace.granted += budget;
    budget
}

/// Where in the tick grid one slot sits, uniform over a tick.
///
/// Drawn from the slot's own rng, which is seeded per slot, so a run is still reproducible from
/// `--seed`: the phases differ between slots and repeat between runs.
fn tick_phase(rng: &mut impl Rng, tick_len: Duration) -> Duration {
    tick_len.mul_f64(rng.r#gen::<f64>())
}

/// Install ring as the process-wide rustls provider.
///
/// **Explicit rather than inferred**, and tolerant of somebody having got there first — the same
/// shape the vendored `db.rs` uses in `puna-core`, and for the same reason: `install_default`
/// returns `Err` when a default already exists, so `expect`ing it turns a harmless second call
/// into a crash.
///
/// Without this *and* the `rustls` dependency that enables the feature, every `wss://` connection
/// panicked inside rustls at the first handshake. Auto-inference works when exactly one provider
/// feature is on; a graph with none is as ambiguous to it as a graph with two.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Where to dial, from what the operator typed.
///
/// **`wss://` unless a scheme says otherwise**, so the ordinary form is `host:port` and gets TLS
/// with the certificate verified against that host — a Puna room is reached by its advertised
/// hostname, which is the one name on its certificate.
///
/// A written-out `ws://` is honored, which is what makes a locally-run pahoa testable: with no
/// `--tls-cert` it serves plaintext, and there is no cert to verify. That is a different thing from
/// a flag that turns verification *off*, which this deliberately does not have — the scheme says
/// plainly what the connection is, in the command line and in the shell history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl Endpoint {
    pub fn parse(room: &str) -> Result<Self> {
        let (tls, rest) = match room.split_once("://") {
            Some(("ws", rest)) => (false, rest),
            Some(("wss", rest)) => (true, rest),
            Some((scheme, _)) => bail!("{scheme}:// is not a room address; use ws:// or wss://"),
            None => (true, room),
        };
        let rest = rest.trim_end_matches('/');
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("{room:?} has no port; a room is host:port"))?;
        if host.is_empty() {
            bail!("{room:?} has no host");
        }
        Ok(Self {
            host: host.to_string(),
            port: port
                .parse()
                .with_context(|| format!("{port:?} is not a port"))?,
            tls,
        })
    }
}

/// Either transport, behind one type.
///
/// `Client` is generic over its stream, so plaintext and TLS are different types — and every
/// function below would otherwise have to be generic too, or exist twice. Boxing costs one indirect
/// call per socket read, which is nothing beside a TLS record or the JSON behind it.
///
/// pahoa made the opposite call for their own `loadtest` example and were right to: that harness
/// exists to measure a plaintext ceiling, where the indirection would land in the number. This one
/// exists to load a room.
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

type Socket = pahoa_net::ws::client::Client<Box<dyn Stream>>;

/// Open one connection to the room, **offering permessage-deflate**.
///
/// The offer is the whole reason this goes through pahoa's client rather than a WebSocket crate:
/// `tungstenite` rejects a frame with RSV1 set outright, so a load run through it is a population
/// the room can share no compression with — 1.2 GB delivered on one measured run, every byte of it
/// uncompressed, which is a worst case rather than a measurement of anything players would do.
///
/// TLS is terminated here and verified against **the name the operator typed**, which is the name
/// on the room's certificate. `handshake` takes that name for the `Host:` header too, so the two
/// cannot disagree.
async fn connect(endpoint: &Endpoint) -> Result<Socket> {
    let stream = tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .with_context(|| format!("connecting to {}:{}", endpoint.host, endpoint.port))?;
    // Checks are small and latency-sensitive; a room's traffic is nothing like bulk transfer.
    stream.set_nodelay(true).ok();

    let stream: Box<dyn Stream> = if endpoint.tls {
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            })
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(endpoint.host.clone())
            .with_context(|| format!("{:?} is not a valid hostname for TLS", endpoint.host))?;
        Box::new(
            tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, stream)
                .await
                .with_context(|| format!("TLS handshake with {}", endpoint.host))?,
        )
    } else {
        Box::new(stream)
    };

    Socket::handshake(stream, &endpoint.host, true)
        .await
        .with_context(|| format!("upgrading to a WebSocket at {}", endpoint.host))
}

/// What is left to check, reconciled against what the room says is already done.
///
/// **The room decides, not the seed.** A tool that sent every location out of the multidata would
/// replay thousands of checks a part-played room answers by ignoring — load that measures nothing
/// while hiding the real rate, and the reason a run cannot simply be restarted from the file.
///
/// `missing_locations` should already exclude `checked_locations`; subtracting anyway costs one
/// pass and means a room that reports them overlapping does not make this send a check twice.
pub fn to_send(missing: Vec<i64>, checked: &[i64]) -> Vec<i64> {
    if checked.is_empty() {
        return missing;
    }
    let seen: HashSet<i64> = checked.iter().copied().collect();
    missing.into_iter().filter(|l| !seen.contains(l)).collect()
}

/// Open one connection in a given role and get as far as having sent `Connect`.
async fn dial(config: &Config, plan: &SlotPlan, role: Role, totals: &Totals) -> Result<Socket> {
    let endpoint = Endpoint::parse(&config.room)?;
    let mut socket = connect(&endpoint).await?;
    totals.opened.fetch_add(1, Ordering::Relaxed);
    if socket.deflate {
        totals.deflated.fetch_add(1, Ordering::Relaxed);
    }

    // RoomInfo arrives unprompted; the handshake is Connect in reply to it.
    wait_for_room_info(&mut socket).await?;

    socket
        .send(&frame(&[Outbound::Connect {
            password: config.password.as_deref(),
            game: role.game(plan),
            name: &plan.name,
            // Distinct per connection: a uuid is how a client identifies itself across a
            // reconnect, and three sockets on one slot are three clients.
            uuid: &format!("puna-tools-{}-{role:?}", plan.slot),
            version: Version {
                major: CLIENT_VERSION.0,
                minor: CLIENT_VERSION.1,
                build: CLIENT_VERSION.2,
                class: "Version",
            },
            items_handling: config.items_handling,
            tags: role.tags(),
            // Nothing here reads slot data, and a large seed's can be megabytes per connection.
            slot_data: false,
        }])?)
        .await?;
    Ok(socket)
}

/// Hold one non-playing connection open, consuming everything the room sends it.
///
/// **This is what makes a load run's fan-out honest.** A room's outbound cost is per *connection*,
/// and a real player holds up to three — so a run with one socket per slot measures a third of the
/// delivery a full room would do, while every rate that looks per-player is really per-socket.
///
/// It sends nothing after `Connect` and that is not laziness: a `TextOnly` or `Tracker` connection
/// is `no_locations` at the server, which refuses a check or a goal from it by name. Its whole job
/// is to drain and to answer heartbeats, and `Client::recv` answers those on the socket it owns.
///
/// **Items it receives are deliberately not counted.** With `items_handling` on, every one of a
/// slot's items arrives once per connection; counting them would multiply the run's item total by
/// the number of clients per slot and destroy the one number that can be checked against the
/// room's own tracker.
pub async fn observe(
    plan: SlotPlan,
    role: Role,
    config: Arc<Config>,
    totals: Arc<Totals>,
    finished: Arc<AtomicBool>,
    schedule: Schedule,
) -> Result<()> {
    tokio::time::sleep_until(tokio::time::Instant::from_std(schedule.connect_at)).await;

    let what = format!("slot {} ({role:?})", plan.slot);
    let mut backoff = Backoff::new();
    let mut rng = backoff_rng();
    // Whether this connection has ever been up, which is what separates a return from a late
    // arrival. See `Connection::opened`.
    let mut been_up = false;

    while !finished.load(Ordering::Relaxed) {
        let began = Instant::now();
        let outcome = observe_once(&plan, role, &config, &totals, &finished, &mut been_up).await;
        match retry(
            &what,
            outcome,
            began.elapsed(),
            &mut backoff,
            &mut rng,
            &finished,
        )
        .await
        {
            Retry::Again => {}
            Retry::Stop => break,
        }
    }
    Ok(())
}

/// One observing connection, from dial to whatever ends it.
async fn observe_once(
    plan: &SlotPlan,
    role: Role,
    config: &Config,
    totals: &Totals,
    finished: &AtomicBool,
    been_up: &mut bool,
) -> Result<Ended> {
    let mut socket = dial(config, plan, role, totals).await?;
    let _connected = handshake(&mut socket, plan).await?;
    let mut connection = Connection::opened(totals, been_up);

    // A wake often enough to notice the run ending, since a quiet room may send this connection
    // nothing at all for long stretches.
    let idle = tokio::time::sleep(Duration::from_secs(1));
    tokio::pin!(idle);

    loop {
        if finished.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            message = socket.recv() => {
                if message?.is_none() {
                    return Ok(Ended::Lost);
                }
            }
            _ = &mut idle => {
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(1));
            }
        }
    }

    connection.closing();
    Ok(Ended::RunOver)
}

/// Play one slot until it goals, then hold the connection open until the run ends.
pub async fn play(
    plan: SlotPlan,
    config: Arc<Config>,
    totals: Arc<Totals>,
    finished: Arc<AtomicBool>,
    start: Arc<Barrier>,
    seed: u64,
    schedule: Schedule,
) -> Result<()> {
    // Wait for this slot's turn on the ramp. Every task is spawned immediately and sleeps here, so
    // the ramp is one clock rather than a spawn loop that would also delay the progress watcher.
    tokio::time::sleep_until(tokio::time::Instant::from_std(schedule.connect_at)).await;

    // **Outside the session, so it survives a reconnect.** The traffic rng in particular: reseeding
    // it per session would make every slot that got dropped replay the same burst pattern from the
    // top, which is a shape the run would then be measuring.
    let mut session = Session::default();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut pace = Pace::default();

    let what = format!("slot {}", plan.slot);
    let mut backoff = Backoff::new();
    let mut backoff_rng = backoff_rng();
    // Whether this connection has ever been up, which is what separates a return from a late
    // arrival. See `Connection::opened`.
    let mut been_up = false;

    while !finished.load(Ordering::Relaxed) {
        let began = Instant::now();
        let outcome = play_once(
            &plan,
            &config,
            &totals,
            &finished,
            &start,
            schedule,
            &mut been_up,
            &mut session,
            &mut rng,
            &mut pace,
        )
        .await;
        match retry(
            &what,
            outcome,
            began.elapsed(),
            &mut backoff,
            &mut backoff_rng,
            &finished,
        )
        .await
        {
            Retry::Again => {}
            Retry::Stop => break,
        }
    }
    Ok(())
}

/// One playing connection, from dial to whatever ends it.
#[allow(
    clippy::too_many_arguments,
    reason = "every one of these is state that outlives the socket, which is the whole point of \
              splitting a session out of `play`. A context struct would group them and would make \
              the thing that matters -- which of them survive a reconnect and which do not -- \
              harder to see rather than easier."
)]
async fn play_once(
    plan: &SlotPlan,
    config: &Config,
    totals: &Totals,
    finished: &AtomicBool,
    start: &Barrier,
    schedule: Schedule,
    been_up: &mut bool,
    session: &mut Session,
    rng: &mut StdRng,
    pace: &mut Pace,
) -> Result<Ended> {
    let mut socket = dial(config, plan, Role::Player, totals).await?;

    let Connected {
        mut remaining,
        replayed,
    } = handshake(&mut socket, plan).await?;
    let mut connection = Connection::opened(totals, been_up);

    // Whatever was already waiting for this slot counts as received, because it was, minus
    // anything a previous session already counted, since the room replays from index zero every
    // time. See `Session::absorb_replay`.
    session.absorb_replay(replayed.len(), totals);

    // Shuffled so slots do not all walk their worlds in ascending id order. Two slots sharing a
    // game have identically-shaped location tables, and unshuffled they would march through them
    // in lockstep, which is traffic with a regularity no room ever sees.
    remaining.shuffle(rng);

    // **Nothing left to check means this slot is done**, which is how a resumed run behaves: point
    // the tool at a room that is already part-played and the slots that finished last time come
    // back with an empty `missing_locations`.
    //
    // Declaring the goal rather than merely going quiet is the part that matters. A slot that has
    // exhausted its world can contribute nothing else, and saying so releases whatever is left in
    // it, which is what keeps the *other* slots' Goals reachable. Staying silent instead is how a
    // resumed run deadlocks: two finished-but-unannounced slots, each holding the other's Goal
    // behind a location neither will ever check again.
    // **A spectator is not "already finished"**: it has nothing to check because it never had
    // anything, which is a different fact from a player who has checked everything. It stops
    // sending either way; only a player declares a goal or counts toward the run's end.
    //
    // Settled **before** the gate rather than after it, because the gate now reads: a slot can
    // receive its Goal from somebody else's resumed history while it waits, and `pump` needs the
    // flag to exist to avoid announcing the same goal twice.
    //
    // **Two ways to arrive already finished, and the second is the one that used to be missed.**
    // An empty `missing_locations` is a slot that checked everything last time; a Goal sitting in
    // the connect-time replay is a slot that *won* last time, which the room announces at connect
    // and which says nothing about how many locations are left. Reading only the first would leave
    // such a slot checking a world it had already finished while the run waited for its goal.
    //
    // **A reconnect arrives here already knowing**, through `session.goaled`, and re-declares
    // anyway. Cheap insurance rather than duplication: a slot that goaled and lost the connection
    // in the same breath may never have got the `StatusUpdate` out, and telling a room a goal it
    // already has is idempotent. What must not repeat is the *run's* tally, which is why the
    // counter moves only on the transition: `totals.goaled >= players` ends the run, so a slot
    // counting itself twice would stop the run with somebody still playing.
    let goal_replayed = plan.goal_item.is_some_and(|goal| replayed.contains(&goal));
    let finished_here = session.goaled || remaining.is_empty() || goal_replayed;
    if finished_here && plan.goal_item.is_some() {
        remaining.clear();
        let first_time = session.goal();
        if first_time {
            tracing::info!(
                slot = plan.slot,
                replayed_goal = goal_replayed,
                "connected already finished; declaring goal"
            );
        }
        socket
            .send(&frame(&[Outbound::StatusUpdate {
                status: STATUS_GOAL,
            }])?)
            .await?;
        if first_time {
            totals.goaled.fetch_add(1, Ordering::Relaxed);
        }
    } else if finished_here {
        // A spectator: nothing to check, nothing to declare.
        remaining.clear();
        session.goaled = true;
    }
    session.remaining = remaining;

    // **Only when there is no ramp**: with one, this slot starts checking the moment it is
    // connected, so load builds with the population instead of arriving all at once after a dead
    // period the length of the ramp. See `schedule`.
    //
    // **Timed out, because a barrier is a deadlock waiting for a bad slot.** One connection refused
    // (a wrong password, a slot name the room does not have, a room that went down mid-start)
    // and every other slot would wait at this gate forever, which reads as a hang rather than as
    // the one error it is.
    //
    // **AND IT KEEPS READING WHILE IT WAITS**, which the first version did not. A slot at the gate
    // holds a live connection, and **pahoa pings, and is the only side that does** (`config.rs`:
    // Archipelago's own clients connect with `ping_interval=None`): 20 s, with 20 s more to
    // answer, so 40 s of silence is a connection the room closes with `no pong within the keepalive
    // timeout`, appearing here as a TLS EOF that names nothing. A storm of two thousand slots takes
    // long enough to connect that this is reachable even with no ramp at all.
    //
    // The barrier future is pinned and polled across iterations rather than recreated: dropping a
    // half-polled `Barrier::wait` and calling it again would count this slot's arrival twice.
    //
    // **A reconnecting slot skips the gate entirely** (`session.gated`), for the same reason: a
    // `Barrier` counts arrivals, so a slot that came back and waited again would arrive twice and
    // release the gate on behalf of one that never showed up.
    if let Some(until) = schedule.gate_until
        && !session.gated
    {
        session.gated = true;
        let gate = start.wait();
        tokio::pin!(gate);
        let gate_deadline = tokio::time::sleep_until(tokio::time::Instant::from_std(until));
        tokio::pin!(gate_deadline);
        loop {
            tokio::select! {
                _ = &mut gate => break,
                _ = &mut gate_deadline => break,
                message = socket.recv() => {
                    let Some(text) = message? else {
                        return Ok(Ended::Lost);
                    };
                    // Items can already be arriving here, since a resumed room replays a slot's
                    // history on connect, so this goes through the same handler rather than
                    // being dropped.
                    if pump(&mut socket, text, plan, totals, session).await? {
                        totals.goaled.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    let mut window: Vec<u32> = Vec::new();
    let mut tick = TICKS_PER_WINDOW as usize;
    let tick_len = WINDOW / TICKS_PER_WINDOW;

    // **Every slot needs its own tick grid, and this line is the whole reason.**
    //
    // The gate releases all of them in the same instant, so a clock started here would put all two
    // hundred on one grid: `next_tick` steps a fixed second from a shared origin and never drifts.
    // The jitter then chooses *which* tick a slot fires on and nothing chooses *when inside it*, so
    // the traffic arrives in ten synchronized clumps a window instead of spread across it: at
    // `--rate 0.1`, twenty checks landing in the same millisecond, each fanning out to two hundred
    // connections. That is a thundering herd the harness manufactures and then measures, on both
    // ends: the room sees one burst, and two hundred client tasks wake together to read the reply.
    //
    // A uniform phase inside the tick decorrelates them, and because the window is ten ticks off
    // the same clock it decorrelates the windows too. The README has claimed since the first draft
    // that slots "burst independently rather than in one synchronized waveform"; until now that was
    // true of which tick and false of the tick itself.
    //
    // **Re-phased on every session**, which a reconnect makes worth saying: the connections a room
    // sheds are shed together, so they reconnect together, and a grid resumed from where it left
    // off would hand the room back a phase-aligned block of the very slots it just dropped.
    let mut next_tick = Instant::now() + tick_phase(rng, tick_len);

    // **A read is armed at all times, and the tick is a peer branch rather than a deadline the
    // reading stops at.** This is the shape the 2000-slot cascade asked for.
    //
    // The previous loop read *until* the tick was due and then left the socket alone while it
    // planned a window, drained locations and awaited a send. At ordinary rates that gap is
    // nothing. Under a goal cascade (247,000 frames a second delivered across two thousand
    // connections) it is the difference between draining and falling behind, and falling behind
    // has a hard floor: pahoa's `budget::reserve` checks the **per-connection** share first, so a
    // client 256 KiB behind on its own socket is dropped by design. 545 of 2000 went that way.
    //
    // Everything the room said pointed away from the room: 0.6 of 2 cores, the room-wide queue at
    // 208 of 562 MiB, mailbox depth zero, shard overflow zero, and the drops stopping the moment
    // the surviving population matched what the harness could drain.
    //
    // **Not `Client::split()`, which is what pahoa built for this**, because `Reader::recv`
    // discards ping frames (the writer half owns writing) and pahoa is the only side that pings,
    // with 40 seconds of silence being a closed connection. A true split would need the reader to
    // hand pings to the writer; one task that always has a read armed gets the same drain behavior
    // and keeps `Client::recv` answering them.
    let tick_at = tokio::time::sleep_until(tokio::time::Instant::from_std(next_tick));
    tokio::pin!(tick_at);

    loop {
        // **Not "this slot goaled"** -- a spectator never does, and gating on it would leave every
        // watching connection hanging after the players had finished and the run was over.
        if finished.load(Ordering::Relaxed) {
            break;
        }

        tokio::select! {
            message = socket.recv() => {
                let Some(text) = message? else {
                    return Ok(Ended::Lost);
                };
                if pump(&mut socket, text, plan, totals, session).await? {
                    totals.goaled.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ = &mut tick_at => {
                // **Missed ticks are dropped rather than chased.** Without the clamp, a slot that
                // fell behind had every later deadline already in the past, so this branch would
                // fire continuously and crowd out the read, which is how an earlier version
                // manufactured its own lag disconnects and made them look like the room's.
                let now = Instant::now();
                next_tick = next_tick.max(now) + tick_len;
                tick_at.as_mut().reset(tokio::time::Instant::from_std(next_tick));

                // Refill the window when its ticks run out.
                //
                // **`pace` outlives the session on purpose, and it does the right thing without
                // being told to.** It counts windows as they are refilled, so a slot that spent
                // ninety seconds backing off simply did not refill during them, where a rate
                // derived from wall-clock elapsed time would hand that slot every check it "owed"
                // the moment it reconnected, aiming a catch-up burst at a room that has just
                // finished shedding load.
                if tick >= window.len() {
                    window = plan_window(
                        window_budget(config.rate, pace),
                        config.jitter,
                        rng,
                    );
                    tick = 0;
                }
                let want = window[tick] as usize;
                tick += 1;

                // Released on goal, so there is nothing left worth checking -- but the connection
                // stays up and the loop keeps draining and answering pings.
                if !session.goaled && want > 0 && !session.remaining.is_empty() {
                    let mut packets = Vec::new();
                    let mut sent = 0;
                    while sent < want && !session.remaining.is_empty() {
                        let take = config
                            .batch
                            .min(want - sent)
                            .min(session.remaining.len())
                            .max(1);
                        let locations: Vec<i64> = session.remaining.drain(..take).collect();
                        sent += locations.len();
                        packets.push(Outbound::LocationChecks { locations });
                    }
                    // **Counted after the send, so a drop mid-write can undercount by at most one
                    // batch.** The other order overcounts, and of the two a load tool must never
                    // claim traffic it did not make. The locations themselves are not lost either
                    // way: a reconnect takes the to-send list from the room's own
                    // `missing_locations`, so anything that did not land comes back.
                    socket.send(&frame(&packets)?).await?;
                    totals.checks_sent.fetch_add(sent as u64, Ordering::Relaxed);
                }

                if !session.goaled
                    && config.say_rate > 0.0
                    && rng.gen_bool((config.say_rate * tick_len.as_secs_f64()).min(1.0))
                {
                    socket
                        .send(&frame(&[Outbound::Say {
                            text: format!("{} is still going", plan.name),
                        }])?)
                        .await?;
                }
            }
        }
    }

    connection.closing();
    // No close handshake: pahoa's client does not model one, and a room reads a dropped socket the
    // same way a real client's crash reads. The run is over by the time this happens.
    Ok(Ended::RunOver)
}

/// Handle one message. Returns whether this call is the one that goaled the slot.
///
/// **Pings are answered by `Client::recv` itself**, on the socket it owns, so a goaled slot that
/// has stopped writing still answers — which is exactly the window where a room would otherwise
/// stop hearing from it. Earlier this file did it by hand because tungstenite queues a pong and
/// flushes it on the *next write*, which a silent client never makes.
async fn pump(
    socket: &mut Socket,
    text: String,
    plan: &SlotPlan,
    totals: &Totals,
    session: &mut Session,
) -> Result<bool> {
    let _ = plan.slot;

    // **Received, then dropped without parsing**, which is the only thing in this loop that is
    // about the harness rather than the room. A check broadcasts a `PrintJSON` and a `RoomUpdate`
    // to *every* connection, so the firehose is two orders of magnitude larger than the traffic
    // this tool acts on (53,042 `PrintJSON` against 16,809 `ReceivedItems` in one measured room)
    // and parsing it all would make the client the expensive half of a measurement of the server.
    // The room still does every bit of its own work: the message was produced, framed and written.
    //
    // A `PrintJSON` whose chat text happens to contain the word costs one wasted parse; a real
    // `ReceivedItems` can never be missed, since the `cmd` is what the check looks for.
    if !text.contains(r#""ReceivedItems""#) {
        return Ok(false);
    }

    let mut newly_goaled = false;
    for packet in serde_json::from_str::<Vec<Inbound>>(&text)? {
        if let Inbound::ReceivedItems { items } = packet {
            session.absorb_live(items.len(), totals);
            let found_goal = plan
                .goal_item
                .is_some_and(|goal| items.iter().any(|i| i.item == goal));
            if !session.goaled && found_goal {
                newly_goaled = session.goal();
                session.remaining.clear();
                socket
                    .send(&frame(&[Outbound::StatusUpdate {
                        status: STATUS_GOAL,
                    }])?)
                    .await?;
            }
        }
    }
    Ok(newly_goaled)
}

async fn wait_for_room_info(socket: &mut Socket) -> Result<()> {
    match socket.recv().await? {
        Some(_) => Ok(()),
        None => bail!("the room closed the connection before RoomInfo"),
    }
}

/// What connecting told us: what is left to check, and whatever was already waiting.
struct Connected {
    remaining: Vec<i64>,
    /// Item ids delivered as part of connecting — the room's replay of this slot's history.
    replayed: Vec<i64>,
}

/// Read until `Connected`, and take the to-send list **from the room**.
///
/// This is what makes the tool restartable: a run stopped and started again against a part-played
/// room picks up where the room is rather than replaying thousands of checks it has already seen,
/// which the room answers by ignoring — load that measures nothing while hiding the real rate.
///
/// **The whole batch is read, not just up to `Connected`**, and that is a fix rather than a
/// nicety. The room answers a connect with `Connected` *and* the slot's item history, commonly in
/// one array, and returning at the first match threw the rest of it away. Two consequences, one
/// merely cosmetic and one not:
///
/// - The tool under-reported items. Measured against a room's own tracker: slots 4, 5 and 6 of a
///   six-player run were short by exactly 1, 2 and 3 — the count rising with connect order,
///   because a ramped slot that dials later has more already waiting for it. 234 of 240.
/// - **A resumed slot could miss its own Goal.** Point the tool at a part-played room where this
///   slot's Goal has already been found, and the Goal arrives in precisely this replay: dropped,
///   the slot never notices it has finished, keeps checking a world it has already won, and the
///   run waits for a goal that has already happened.
///
/// It was invisible until the start gate went away — with everybody waiting for the last connect,
/// nothing had been checked yet and every replay was empty.
async fn handshake(socket: &mut Socket, plan: &SlotPlan) -> Result<Connected> {
    let mut replayed = Vec::new();
    while let Some(text) = socket.recv().await? {
        let mut remaining = None;
        for packet in serde_json::from_str::<Vec<Inbound>>(&text)? {
            match packet {
                Inbound::ReceivedItems { items } => {
                    replayed.extend(items.iter().map(|i| i.item));
                }
                Inbound::Connected {
                    slot,
                    missing_locations,
                    checked_locations,
                } => {
                    if slot != plan.slot {
                        bail!(
                            "connected as slot {slot} but expected {} -- is this the right seed?",
                            plan.slot
                        );
                    }
                    tracing::debug!(
                        slot = plan.slot,
                        missing = missing_locations.len(),
                        checked = checked_locations.len(),
                        "connected"
                    );
                    remaining = Some(to_send(missing_locations, &checked_locations));
                }
                Inbound::ConnectionRefused { errors } => {
                    return Err(anyhow!(
                        "slot {} refused: {}",
                        plan.slot,
                        if errors.is_empty() {
                            "no reason given".to_string()
                        } else {
                            errors.join(", ")
                        }
                    ));
                }
                _ => {}
            }
        }
        if let Some(remaining) = remaining {
            return Ok(Connected {
                remaining,
                replayed,
            });
        }
    }
    bail!("slot {} never reached Connected", plan.slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> StdRng {
        StdRng::seed_from_u64(seed)
    }

    /// The backoff doubles, stops doubling, and never lands twice in the same place.
    ///
    /// **The ceiling is the load-bearing half.** A room that refuses forever is answered by every
    /// slot knocking at 30-second intervals; without the cap, `RECONNECT_BASE << attempt` overflows
    /// a `u32` shift at 32 — a panic in debug and, in release, a wrap to a one-nanosecond backoff,
    /// which is a two-thousand-connection redial storm produced by an integer.
    #[test]
    fn the_backoff_doubles_up_to_a_ceiling() {
        let mut rng = rng(7);

        // Doubling, allowing for the jitter band: attempt n's floor is attempt n-1's floor doubled.
        for attempt in 0..6 {
            let window = RECONNECT_BASE * (1 << attempt);
            for _ in 0..64 {
                let delay = reconnect_delay(attempt, &mut rng);
                assert!(
                    delay >= window / 2 && delay <= window,
                    "attempt {attempt} drew {delay:?} outside [{:?}, {window:?}]",
                    window / 2
                );
            }
        }

        // Capped, and still capped at the shift widths that would overflow.
        for attempt in [7, 31, 32, 64, u32::MAX] {
            let delay = reconnect_delay(attempt, &mut rng);
            assert!(
                delay >= RECONNECT_MAX / 2 && delay <= RECONNECT_MAX,
                "attempt {attempt} drew {delay:?}, which is not the ceiling"
            );
        }
    }

    /// **Jittered, because the connections a room sheds are shed together.**
    ///
    /// A goal cascade dropped 545 of 2000 in twelve seconds. Undelayed or fixed-delay, all 545
    /// redial in the same instant — the connect storm `schedule` exists to prevent, aimed at a room
    /// that has just demonstrated it is at its limit.
    #[test]
    fn the_backoff_does_not_land_every_slot_on_one_instant() {
        let mut rng = rng(11);
        let draws: Vec<Duration> = (0..500).map(|_| reconnect_delay(4, &mut rng)).collect();
        let distinct: std::collections::HashSet<_> = draws.iter().collect();
        assert!(
            distinct.len() > 400,
            "500 slots drew only {} distinct delays",
            distinct.len()
        );
    }

    /// **A session that did not last is not a connection that worked.**
    ///
    /// The obvious rule — reset once the handshake succeeds — is wrong for the case the backoff
    /// exists for: a room shedding load accepts and drops again moments later, and resetting on the
    /// accept puts that slot into a 500 ms redial loop against a room that has just said it cannot
    /// cope. That is a load tool turning a shed into a storm.
    #[test]
    fn a_flapping_connection_keeps_escalating() {
        let mut backoff = Backoff::new();
        let mut rng = rng(3);

        for _ in 0..5 {
            backoff.wait(&mut rng);
            backoff.held(Duration::from_secs(1));
        }
        assert_eq!(
            backoff.attempt, 5,
            "a flapping room must not reset the wait"
        );

        backoff.held(RECONNECT_MAX);
        assert_eq!(
            backoff.attempt, 0,
            "a session that lasted must start the next drop from the bottom"
        );
    }

    /// **A slot that stumbles on the way in has not reconnected**, and `back` must never exceed
    /// `drops`.
    ///
    /// The first version keyed the recovery on "is this the first attempt", which is a different
    /// question and agrees with the right one everywhere except here: a slot whose opening dial
    /// fails and whose second succeeds arrives *late*, having never been up, and was counted as a
    /// return from a drop that never happened. Seen on a 2000-connection run as
    /// `(drops 20436, back 20437)` — a bound violated by exactly the number of slots that stumbled.
    ///
    /// It matters past the display: `drops - reconnects` is what the summary reports as never having
    /// come back, and a negative difference saturates to zero — so a run that really did end a
    /// connection short would have said every connection was up.
    #[test]
    fn a_late_arrival_is_not_a_reconnection() {
        let totals = Totals::default();
        let mut been_up = false;

        let drops = || totals.drops.load(Ordering::Relaxed);
        let back = || totals.reconnects.load(Ordering::Relaxed);

        // Two dials that failed before establishing anything. There is no guard to construct, so
        // neither counter moves -- and, the point, the slot has still never been up.

        // Now it lands for the first time: an arrival, not a return.
        let late = Connection::opened(&totals, &mut been_up);
        assert_eq!(
            back(),
            0,
            "a slot that stumbled on the way in has not returned from anything"
        );
        assert_eq!(drops(), 0);

        // The room drops it and it comes back. *That* is a return.
        drop(late);
        let mut again = Connection::opened(&totals, &mut been_up);
        again.closing();

        assert_eq!(drops(), 1);
        assert_eq!(back(), 1);
        assert!(
            back() <= drops(),
            "back {} exceeded drops {}",
            back(),
            drops()
        );
    }

    /// **A reconnect replays the slot's whole item history, and it must not be counted twice.**
    ///
    /// `totals.items_received` is the one number checkable against the room's own tracker, so
    /// adding a slot's history per drop would inflate exactly the figure a run is verified by --
    /// and it would inflate it *more* the worse the run went, which is the direction that hides a
    /// problem rather than showing one.
    #[test]
    fn a_replayed_history_is_counted_once_however_often_a_slot_reconnects() {
        let totals = Totals::default();
        let mut session = Session::default();

        // First connection: three waiting, then two arrive live.
        assert_eq!(session.absorb_replay(3, &totals), 3);
        session.absorb_live(2, &totals);
        assert_eq!(totals.items_received.load(Ordering::Relaxed), 5);

        // Dropped and back: the room replays all five, of which none is new.
        assert_eq!(session.absorb_replay(5, &totals), 0);
        assert_eq!(totals.items_received.load(Ordering::Relaxed), 5);

        // Dropped again, and this time two arrived while it was away.
        assert_eq!(session.absorb_replay(7, &totals), 2);
        assert_eq!(totals.items_received.load(Ordering::Relaxed), 7);
    }

    /// **The run's goal tally moves once per slot, and the run's end depends on it.**
    ///
    /// `totals.goaled >= players` is what ends a run, so a slot that counted itself again after a
    /// reconnect would stop the run with somebody still playing — a truncated measurement that
    /// reports as a complete one.
    #[test]
    fn a_slot_counts_its_goal_once_across_reconnects() {
        let mut session = Session::default();
        assert!(session.goal(), "the first goal is the one that counts");
        assert!(!session.goal(), "a reconnect must not count the goal again");
        assert!(!session.goal());
        assert!(session.goaled);
    }

    /// **The budget is spent exactly, at every jitter setting.** The whole promise of the window is
    /// that the average comes out true, so a rounding drift would make the tool quietly send fewer
    /// checks than asked for -- and a load tool that lies about its own rate is worse than none.
    #[test]
    fn a_window_spends_its_budget_exactly() {
        for jitter in [0.0, 0.25, 0.5, 1.0] {
            for budget in [0, 1, 7, 10, 13, 100, 999] {
                let plan = plan_window(budget, jitter, &mut rng(u64::from(budget) + 1));
                assert_eq!(plan.len(), TICKS_PER_WINDOW as usize);
                assert_eq!(
                    plan.iter().sum::<u32>(),
                    budget,
                    "jitter={jitter} budget={budget} plan={plan:?}"
                );
            }
        }
    }

    /// **The dials are spaced, and a ramp replaces the gate rather than preceding it.** The second
    /// half is the one worth pinning: with both, a 2000-slot run spent 6.7 minutes connected and
    /// silent before anything was sent, and then started every slot in the same instant.
    #[test]
    fn a_ramp_spaces_the_dials_and_leaves_no_gate() {
        let origin = Instant::now();
        let plan = schedule(origin, 200, 5.0);

        assert_eq!(plan.len(), 200);
        assert_eq!(plan[0].connect_at, origin);
        assert_eq!((plan[199].connect_at - origin).as_millis(), 39_800);
        assert!(
            plan.windows(2).all(|w| w[0].connect_at <= w[1].connect_at),
            "the ramp must not go backwards"
        );

        assert!(
            plan.iter().all(|s| s.gate_until.is_none()),
            "a ramped slot checks as soon as it is connected"
        );
    }

    /// **A slot waiting at the gate must still be reading.** Source lint, because the failure has
    /// no unit test that could reach it and no symptom that names it.
    ///
    /// pahoa is the only side that pings (Archipelago's own clients set `ping_interval=None`), at
    /// 20 s with 20 s more to answer — so 40 s of silence is a connection the room closes. The
    /// default ramp for 200 slots is 39.8 s, which puts the first slot to arrive **exactly on that
    /// boundary**, and from this side the death appears as a TLS EOF that names nothing.
    ///
    /// Proved against a real room rather than argued: with a room pinging every 4 s and a ramp that
    /// holds the first slot for 40 s, draining at the gate loses nothing, and awaiting the barrier
    /// bare loses 4 of 5 slots with four `no pong within the keepalive timeout` lines in the room's
    /// log. This lint is what stops that being rediscovered.
    #[test]
    fn a_slot_keeps_reading_while_it_waits_at_the_gate() {
        let source = include_str!("load.rs");
        let gate = source
            .find("start.wait()")
            .expect("the gate must exist at all");
        let window = &source[gate..source.len().min(gate + 1400)];

        assert!(
            window.contains("socket.recv()"),
            "the barrier is awaited without reading the socket; a slot that goes quiet for 40s at \
             the gate is one pahoa closes for a missing pong"
        );
        assert!(
            !source.contains("timeout_at(\n"),
            "an awaited timeout around the barrier is the shape that stopped draining"
        );
    }

    /// **The floor comes from the pinned crate, not from a comment.** pahoa refuses a client below
    /// `MIN_CLIENT_VERSION` for any seed from a generator at or past 0.6.2, and the failure is a
    /// refusal on every slot at once — worth catching when the pin moves rather than in a run.
    ///
    /// The *other* constraint cannot be asserted here and is stated instead: a room with
    /// `compatibility = 0` demands an exact match with pahoa's own `SERVER_VERSION`, which lives in
    /// `pahoa-room`. This crate reaches that only transitively, so the two are kept equal by hand
    /// until it is worth depending on directly.
    #[test]
    fn the_client_version_clears_pahoas_floor() {
        let claimed =
            pahoa_multidata::Version::new(CLIENT_VERSION.0, CLIENT_VERSION.1, CLIENT_VERSION.2);

        assert!(
            claimed >= pahoa_multidata::MIN_CLIENT_VERSION,
            "a client claiming {claimed} is refused by any modern seed"
        );
    }

    /// Zero is the storm, kept on purpose: opening every connection at once is a measurement worth
    /// being able to take, and it is what the first live run did by accident. **It is also the one
    /// case that still gates**, since there is no ramp to give the run a shape instead.
    #[test]
    fn a_connect_rate_of_zero_opens_everything_at_once_and_keeps_the_gate() {
        let origin = Instant::now();
        let plan = schedule(origin, 200, 0.0);

        assert!(plan.iter().all(|s| s.connect_at == origin));
        assert!(
            plan.iter()
                .all(|s| s.gate_until == Some(origin + START_GRACE)),
            "with no ramp, waiting for the others is the only defined start"
        );
    }

    /// A one-slot run has no ramp to speak of, and must not underflow working that out.
    #[test]
    fn a_single_slot_needs_no_ramp() {
        let origin = Instant::now();
        let plan = schedule(origin, 1, 5.0);

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].connect_at, origin);
        assert!(plan[0].gate_until.is_none(), "nobody to wait for");
        assert!(schedule(origin, 0, 5.0).is_empty());
    }

    /// **Slots must not share a tick grid.** The gate releases them together, so without a phase
    /// every slot's clock starts in the same instant and stays there — and the whole per-slot rate
    /// design collapses into one synchronized waveform, which is the traffic shape a real room
    /// never produces and the one that makes a harness measure itself.
    #[test]
    fn each_slot_lands_somewhere_different_in_the_tick() {
        let tick = Duration::from_secs(1);
        // The seeds `room_load` actually hands out, so this exercises the real spread.
        let phases: Vec<Duration> = (0..200u64)
            .map(|slot| tick_phase(&mut rng(0xC0FFEE ^ slot ^ (slot << 32)), tick))
            .collect();

        assert!(
            phases.iter().all(|p| *p < tick),
            "a phase past the tick would shift the whole grid, not spread it"
        );
        let distinct: std::collections::BTreeSet<u128> =
            phases.iter().map(|p| p.as_millis()).collect();
        assert!(
            distinct.len() > 150,
            "only {} distinct phases across 200 slots",
            distinct.len()
        );
        // Spread over the tick rather than huddled in one corner of it.
        let occupied: std::collections::BTreeSet<u128> =
            phases.iter().map(|p| p.as_millis() / 100).collect();
        assert_eq!(occupied.len(), 10, "every tenth of the tick should be used");
    }

    /// **A source lint, because the test above cannot see whether anything CALLS it.**
    ///
    /// Deleting the phase from `play` leaves every unit test green — `tick_phase` still returns a
    /// good spread, nothing else in the file changes, and the only symptom is two hundred slots
    /// back on one grid, visible solely as a traffic shape on somebody else's dashboard. Same shape
    /// as every other "a thing that is entirely its call site" in this repository, so it gets the
    /// same treatment.
    #[test]
    fn the_slot_clock_is_actually_phased() {
        let source = include_str!("load.rs");
        let starts: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("let mut next_tick ="))
            .collect();

        assert_eq!(
            starts.len(),
            1,
            "expected exactly one tick clock to start; found {starts:?}"
        );
        assert!(
            starts[0].contains("tick_phase("),
            "the tick clock must start on this slot's own phase, not on a shared instant: {}",
            starts[0]
        );
    }

    /// **A rate below one check per window still checks.** The reported failure: `--rate 0.01`
    /// connected every slot and then sent nothing at all, because 0.01 × 10 s rounded to a budget
    /// of zero and every tick honestly wanted nothing.
    #[test]
    fn a_rate_smaller_than_one_check_per_window_is_carried_rather_than_rounded_away() {
        let mut pace = Pace::default();
        let budgets: Vec<u32> = (0..10).map(|_| window_budget(0.01, &mut pace)).collect();

        assert_eq!(
            budgets.iter().sum::<u32>(),
            1,
            "one check per 100s means exactly one in ten windows: {budgets:?}"
        );
        assert_eq!(*budgets.last().unwrap(), 1, "and it lands on the tenth");

        // The old behavior, for the record: this is what the tool did for any rate under 0.05.
        assert_eq!((0.01f64 * WINDOW.as_secs_f64()).round() as u32, 0);
    }

    /// The average has to come out true over a long run, at rates that do not divide evenly.
    #[test]
    fn the_carried_budget_holds_the_rate_over_time() {
        for rate in [0.01, 0.037, 0.1, 0.37, 1.0, 2.5] {
            let mut pace = Pace::default();
            let windows = 100;
            let total: u32 = (0..windows).map(|_| window_budget(rate, &mut pace)).sum();
            let want = rate * WINDOW.as_secs_f64() * f64::from(windows);

            assert!(
                (f64::from(total) - want).abs() <= 1.0,
                "rate {rate}: sent {total} where {want} was asked for"
            );
        }
    }

    /// Zero jitter is the flat metronome, kept so a run can be made boring on purpose.
    #[test]
    fn no_jitter_is_flat() {
        assert_eq!(plan_window(100, 0.0, &mut rng(1)), vec![10; 10]);
        // Not divisible: the remainder is spread rather than dropped or piled on one tick.
        let plan = plan_window(13, 0.0, &mut rng(1));
        assert_eq!(plan.iter().sum::<u32>(), 13);
        assert!(plan.iter().all(|n| (1..=2).contains(n)), "{plan:?}");
    }

    /// **Jitter actually bursts.** A test that only checked the sum would pass against a flat
    /// planner, which is the bug worth catching: the point of the window is the shape inside it.
    #[test]
    fn jitter_concentrates_the_budget() {
        let flat = plan_window(1_000, 0.0, &mut rng(5));
        let bursty = plan_window(1_000, 1.0, &mut rng(5));
        let peak = |p: &[u32]| *p.iter().max().unwrap();
        assert_eq!(peak(&flat), 100);
        assert!(
            peak(&bursty) > 250,
            "heavy jitter produced a nearly flat window: {bursty:?}"
        );
        assert!(
            bursty.iter().any(|n| *n < 50),
            "heavy jitter never went quiet: {bursty:?}"
        );
    }

    /// The client's own packets, on the wire. `class: "Version"` is the one a custom client is
    /// documented to have to send, and a room ignores a version object without it.
    #[test]
    fn a_connect_packet_carries_what_the_room_requires() {
        let json = serde_json::to_string(&[Outbound::Connect {
            password: None,
            game: "Gloomhaven Drift",
            name: "vaultmoth",
            uuid: "puna-tools-4",
            version: Version {
                major: 0,
                minor: 6,
                build: 8,
                class: "Version",
            },
            items_handling: ITEMS_HANDLING_ALL,
            tags: vec![],
            slot_data: false,
        }])
        .expect("serialize");

        assert!(json.starts_with('['), "packets travel as an array: {json}");
        assert!(json.contains(r#""cmd":"Connect""#), "{json}");
        assert!(json.contains(r#""class":"Version""#), "{json}");
        assert!(json.contains(r#""items_handling":7"#), "{json}");
        assert!(json.contains(r#""password":null"#), "{json}");
    }

    /// **A `wss://` connection must not panic inside rustls**, which is what happened the first
    /// time this was pointed at a real room.
    ///
    /// `tokio-tungstenite` brings rustls in with no provider feature, and this crate does not
    /// depend on `puna-core` — the thing that enables `rustls/ring` everywhere else — so rustls
    /// compiled for these binaries could not infer a default and `ClientConfig::builder()`
    /// panicked at the first handshake. Every unit test passed, clippy passed, and the repository's
    /// `aws-lc` grep passed, because that check proves there is no *second* provider and says
    /// nothing about whether a first one exists.
    ///
    /// This builds the same config `connect_async` builds, so the failure lands in `cargo test`
    /// rather than in a load run against a live room.
    #[test]
    fn a_tls_client_config_can_actually_be_built() {
        install_crypto_provider();
        let roots = rustls::RootCertStore {
            roots: webpki_roots_for_test(),
        };
        let _config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
    }

    /// An empty root store is enough: the panic is in provider selection, which happens before any
    /// certificate is looked at.
    fn webpki_roots_for_test() -> Vec<rustls::pki_types::TrustAnchor<'static>> {
        Vec::new()
    }

    /// TLS by default, and plaintext only when the command line says so in as many words.
    ///
    /// **The host is kept as typed, not resolved**, because it is both the name verified against
    /// the certificate and the `Host:` header — a room carries exactly one name and it is the one
    /// the operator wrote.
    #[test]
    fn a_room_address_gets_tls_unless_a_scheme_says_otherwise() {
        let parse = |s: &str| Endpoint::parse(s).expect(s);

        assert_eq!(
            parse("mw.ionium.us:45000"),
            Endpoint {
                host: "mw.ionium.us".into(),
                port: 45000,
                tls: true
            }
        );
        assert_eq!(
            parse("ws://127.0.0.1:38281"),
            Endpoint {
                host: "127.0.0.1".into(),
                port: 38281,
                tls: false
            }
        );
        assert_eq!(
            parse("wss://mw.ionium.us:45000/"),
            parse("mw.ionium.us:45000")
        );

        // A room is host:port. Anything else is a typo worth naming rather than a default worth
        // guessing -- picking 443 for a room address would dial the web tier.
        assert!(Endpoint::parse("mw.ionium.us").is_err());
        assert!(Endpoint::parse("https://mw.ionium.us:45000").is_err());
        assert!(Endpoint::parse("mw.ionium.us:sixty").is_err());
        assert!(Endpoint::parse(":45000").is_err());
    }

    /// **A resumed run picks up where the room is.** The whole reason the to-send list comes from
    /// `Connected` rather than from the seed: pointed at a part-played room, this must send what is
    /// left rather than replaying what is done.
    #[test]
    fn the_to_send_list_is_what_the_room_has_not_seen() {
        assert_eq!(to_send(vec![1, 2, 3], &[]), vec![1, 2, 3]);
        assert_eq!(to_send(vec![2, 3], &[1]), vec![2, 3]);
        // A room that reports them overlapping must not make this send a check twice.
        assert_eq!(to_send(vec![1, 2, 3], &[1, 3]), vec![2]);
        // **Empty is the signal a resumed slot has finished.** `play` turns this into an immediate
        // goal declaration; without that, a run resumed against a room where two slots had already
        // exhausted their worlds would wait forever for each to release the other's Goal.
        assert!(to_send(vec![], &[1, 2, 3]).is_empty());
        assert!(to_send(vec![1, 2], &[1, 2]).is_empty());
    }

    /// A server packet this tool does not know must be skipped, not fatal. A room grows packets;
    /// a load tool that fell over on one would stop working the week somebody added it.
    #[test]
    fn unknown_server_packets_are_skipped() {
        let packets: Vec<Inbound> = serde_json::from_str(
            r#"[{"cmd":"RoomUpdate","checked_locations":[1]},
                {"cmd":"ReceivedItems","index":0,"items":[{"item":7,"location":1,"player":2,"flags":1}]},
                {"cmd":"SomethingFromTheFuture","whatever":true}]"#,
        )
        .expect("a lenient read");
        assert_eq!(packets.len(), 3);
        assert!(matches!(packets[0], Inbound::Other));
        assert!(matches!(packets[2], Inbound::Other));
        match &packets[1] {
            Inbound::ReceivedItems { items } => assert_eq!(items[0].item, 7),
            _ => panic!("the packet we care about did not parse"),
        }
    }
}
