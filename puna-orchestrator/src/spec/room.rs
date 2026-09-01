//! What a room's pod should be, and the fingerprint that says whether it already is.
//!
//! ## The spec hash decides when a room gets bounced, so what it covers is a contract
//!
//! The reconciler compares the hash on a running Deployment against the one the row describes, and
//! a difference means delete-and-recreate: roughly a minute of downtime with clients reconnecting
//! on their own. Most of that is the two reconcile intervals a recreate crosses, not the pod: one
//! tick stops the room, the next starts it. So every field is a decision about whether a change is
//! worth that, and two of them are the reason this is not simply "hash the manifest":
//!
//!   * **`slot_auth` is covered**, though it moves nothing in the pod spec. The password mode
//!     reaches pahoa through the Secret with `envFrom`, so without folding it in, turning passwords
//!     on would change the Secret and never restart the room that reads it at startup, and the room
//!     would stay open while the UI said locked.
//!   * **Per-slot password *values* are not covered.** They can be rotated on a live room over the
//!     admin API, and hashing them would bounce a room every time one player rotated a password.
//!
//! Both fall out of one rule: **the hash covers everything pahoa reads once at startup, and nothing
//! it can be told later.** The room-wide password is covered (pahoa reads `PAHOA_PASSWORD` at
//! startup and has no live setter, deliberately). The admin token is covered (same, and a rotated
//! token that has not reached the pod makes every console call fail with a `404` that reads as an
//! old image). The slot map's *keys* are covered, because a slot added to a per-slot room needs its
//! password in the environment before anyone can use it.
//!
//! Those keys briefly came from the draft rather than the map, while Puna expressed a lock by
//! withholding a slot from it: a lock is something pahoa can be told later, so it had to stay out
//! of the fingerprint. pahoa's native `lock` verb removed the conflation, so the map's keys mean
//! exactly "who holds a credential" again and reading them here is honest.
//!
//! **It is deliberately not a hash of the rendered manifest.** That would be deterministic and
//! wrong: a `k8s-openapi` upgrade that reordered one serialization would change every room's hash
//! and recreate every pod in the cluster, for nothing. The canonical string below is Puna's own, so
//! only Puna's own decisions move it.

use puna_core::hash::sha256_hex;
use puna_core::ids::RoomId;
use puna_core::model::room::SlotAuth;

use crate::cluster::RoomSpec;
use crate::spec::secret::SecretData;

/// The canonical form's version.
///
/// Bumping it recreates every room on the next tick, which is occasionally the right thing and never
/// an accident: a change to what the fingerprint *means* has to be distinguishable from a change to
/// what it is fingerprinting.
const CANONICAL_VERSION: &str = "puna/room-spec/1";

/// Everything a room's pod is, before it is fingerprinted.
///
/// Deliberately not a `RoomSpec` with a placeholder hash. A struct whose hash is a lie for the
/// duration of a function call is a struct that eventually escapes one, and `spec_hash` is compared
/// against a live Deployment, so a wrong value there is a room that either never converges or gets
/// recreated on every tick.
#[derive(Debug, Clone)]
pub struct Draft {
    pub room_id: RoomId,
    pub image: String,
    pub base_port: u16,
    pub wants_filtered: bool,
    /// Every slot in the multidata, **groups included**: this sizes the memory request, and pahoa
    /// derives its outbound budget from `slot_info.len()`, so the connectable count under-requests.
    pub slot_count: i32,
    pub save_interval_secs: i32,
    pub use_embedded_options: bool,
}

impl Draft {
    /// Fingerprint the draft and hand back the spec the cluster is asked for.
    ///
    /// `env` is the room's whole environment, as `spec::secret::build` produced it. Passing it in
    /// rather than the room row is what keeps the exclusion rule honest: the only thing this can
    /// exclude is a value it was given, so a new key added to the Secret is covered by default and
    /// leaving it out has to be written down.
    pub fn build(self, slot_auth: SlotAuth, env: &SecretData) -> RoomSpec {
        let spec_hash = sha256_hex(self.canonical(slot_auth, env).as_bytes());
        RoomSpec {
            room_id: self.room_id,
            spec_hash,
            image: self.image,
            base_port: self.base_port,
            wants_filtered: self.wants_filtered,
            slot_count: self.slot_count,
            save_interval_secs: self.save_interval_secs,
            use_embedded_options: self.use_embedded_options,
        }
    }

    /// The exact bytes that get hashed. One `key=value` per line, fixed order.
    ///
    /// The room's id is **not** in it: two rooms with identical settings should hash the same, and
    /// the hash's job is to answer "is this pod the pod this room's row describes", which is asked
    /// per room already.
    fn canonical(&self, slot_auth: SlotAuth, env: &SecretData) -> String {
        let mut out = String::with_capacity(512);
        out.push_str(CANONICAL_VERSION);
        out.push('\n');

        for line in [
            format!("image={}", self.image),
            format!("base_port={}", self.base_port),
            format!("filtered={}", self.wants_filtered),
            format!("slot_count={}", self.slot_count),
            // Derived from `slot_count`, so these move only when Puna's own derivation does, which
            // is exactly when a running room needs recreating to pick up a different fan-out, and
            // is a change `slot_count` alone could never express.
            format!("shards={}", shards(self.slot_count)),
            format!("shard_queue_depth={}", shard_queue_depth(self.slot_count)),
            format!("save_interval={}", self.save_interval_secs),
            format!("use_embedded_options={}", self.use_embedded_options),
            format!("slot_auth={}", slot_auth.as_sql()),
        ] {
            out.push_str(&line);
            out.push('\n');
        }

        // `SecretData` is a BTreeMap, so this walks in key order without sorting, which is also
        // why it is a BTreeMap rather than a HashMap.
        for (key, value) in env {
            match key.as_str() {
                // The one exclusion, and the reason live rotation is live. The KEYS still count: a
                // slot added to a per-slot room cannot connect until its password is in the pod's
                // environment, so the map's shape has to be able to move the hash even though its
                // contents must not.
                "PAHOA_SLOT_PASSWORDS" => {
                    out.push_str("env=PAHOA_SLOT_PASSWORDS=slots:");
                    out.push_str(&slot_numbers(value).join(","));
                    out.push('\n');
                }
                _ => {
                    out.push_str("env=");
                    out.push_str(key);
                    out.push('=');
                    out.push_str(value);
                    out.push('\n');
                }
            }
        }

        out
    }
}

/// The slot numbers a `PAHOA_SLOT_PASSWORDS` map covers, in order, without its values.
///
/// A parse failure yields no numbers rather than an error: this is a fingerprint input, and the
/// Secret builder has already refused every shape that could get here malformed. Returning the raw
/// string instead would fold the passwords back into the hash, which is the one thing this function
/// exists to prevent.
fn slot_numbers(json: &str) -> Vec<String> {
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) {
        Ok(map) => {
            let mut keys: Vec<String> = map.into_iter().map(|(k, _)| k).collect();
            // Numeric where possible, so "10" sorts after "9": the ordering only has to be
            // stable, but one that reads correctly is easier to eyeball in a diff.
            keys.sort_by_key(|k| (k.parse::<i64>().unwrap_or(i64::MAX), k.clone()));
            keys
        }
        Err(_) => Vec::new(),
    }
}

/// The room's outbound queue cap **in MiB**, which is the unit `--outbound-budget` is spelled in.
///
/// `max(64 MiB, slots × 3 × 96 KiB)` rounded **up** to a whole MiB: three connections per slot,
/// because one player commonly holds a game client, a text client and a tracker, at 96 KiB of
/// headroom each. A 2000-slot room lands at 562.5 MiB and is passed as 563.
///
/// The expression is pahoa's `config::outbound_budget_for`, and it used to be a *transcription* of
/// it: the same number computed twice from one input with nothing checking the two agreed. It is
/// passed on the argv now, so this is the number the room actually uses and [`memory_limit_bytes`]
/// is sized against a fact rather than a guess about another repository.
///
/// ## MiB is the whole point of this function existing
///
/// **The first version passed bytes**, because the option's presence was checked and its *unit* was
/// not: `main.rs` reads `--outbound-budget` as `Some(mib) => mib * 1024 * 1024`, and its help text
/// says `<MiB>`. So `589824000`, meant as 562.5 MiB, configured **562 TiB**, and every room
/// deployed with it had no room-wide backstop at all.
///
/// That failure is invisible from every direction anybody would look. pahoa accepts the value, the
/// startup banner reports it without comment, per-connection caps still shed slow clients so lag
/// disconnects look normal, and the only symptom is the room-wide budget never binding, which is
/// indistinguishable from a healthy room until many clients stall at once, at which point the room
/// queues until the kernel kills it. **It re-armed the exact OOM this milestone existed to remove.**
///
/// Rounded **up** rather than down so the cap is never quieter than pahoa's own default would have
/// been, and returned in MiB so the unit is in the name and the byte value is derived from it,
/// rather than the two being converted at a call site, which is where this went wrong.
pub fn outbound_budget_mib(slot_count: i32) -> i64 {
    const PER_CONNECTION: i64 = 96 * 1024;
    const CONNECTIONS_PER_SLOT: i64 = 3;
    const FLOOR: i64 = 64 * 1024 * 1024;

    let slots = i64::from(slot_count.max(0));
    let bytes = slots
        .saturating_mul(CONNECTIONS_PER_SLOT)
        .saturating_mul(PER_CONNECTION)
        .max(FLOOR);
    // Written out rather than `div_ceil`, which is still unstable for signed integers.
    (bytes + MIB - 1) / MIB
}

const MIB: i64 = 1024 * 1024;

/// The same cap in bytes, for sizing the container against it.
///
/// Derived from [`outbound_budget_mib`] rather than computed beside it, so the number Puna sizes
/// the memory limit against is exactly the number the room was told to use, including the rounding.
pub fn outbound_budget_bytes(slot_count: i32) -> i64 {
    outbound_budget_mib(slot_count).saturating_mul(MIB)
}

/// Fan-out width: pahoa's `--shards`, and a **reliability** number before a throughput one.
///
/// A broadcast that will not fit a shard's inbox closes *every connection that shard owns*, because
/// the audience is expanded inside the shard and the actor that dropped the message does not know
/// who it was for. So the blast radius of one dropped frame is `connections / shards`, and this
/// function is choosing that radius.
///
/// ## It used to be `limits.cpu`, which was wrong twice over
///
/// pahoa derived the width from the cgroup CPU quota, so Puna's `limits.cpu: "2"` gave **every room
/// in the fleet two shards**, and a 2000-slot room therefore put half its population behind one
/// queue. A dev-cluster run at ~5000 connections shed ~2,500 on the first overflow and never
/// recovered: they all came back at once, each buying a full item-history replay, which costs the
/// room far more than shedding them saved.
///
/// Wrong because a CPU ceiling is a **scheduling** decision and fan-out width is a **topology** one,
/// and wrong again because it made a reliability parameter a side effect of a value chosen for
/// something else. pahoa split them at Puna's request; this is Puna's half.
///
/// ## Transcribed from `config::shards_for`, and passed rather than left to default
///
/// pahoa derives exactly this from the seed when the flag is absent, so passing it is redundant for
/// the room and **not** redundant for the container: [`shard_queue_bytes`] is memory the outbound
/// budget does not account for, and Puna sizes the limit against it. Same rule as
/// [`outbound_budget_mib`]: the value Puna sized for and the value the room runs at must not be
/// able to disagree.
pub fn shards(slot_count: i32) -> i64 {
    /// Connections one shard may own before it is worth splitting: the blast radius a dropped
    /// broadcast costs, chosen as "how many players may this disconnect" rather than as throughput.
    const CONNECTIONS_PER_SHARD: i64 = 512;
    const FLOOR: i64 = 2;
    /// Past this the per-shard compression of every broadcast costs more than the narrower blast
    /// radius buys. pahoa refuses a larger value outright.
    const CEILING: i64 = 32;

    let connections = expected_connections(slot_count);
    // Written out rather than `div_ceil`, which is still unstable for signed integers.
    ((connections + CONNECTIONS_PER_SHARD - 1) / CONNECTIONS_PER_SHARD).clamp(FLOOR, CEILING)
}

/// How far behind one shard may fall before it starts closing connections: `--shard-queue-depth`.
///
/// **The larger of two bursts that scale in opposite directions**, which is pahoa's rule and was
/// briefly Puna's alone. Their first default was per-connection only; the burst that actually
/// overflowed a Puna room is per-*release*, and the two are unrelated quantities. Puna overrode the
/// depth for one release, reported why, and pahoa adopted the release term verbatim in `9c382ab`,
/// so this is a transcription again rather than a divergence, and the two agree at every size.
///
/// It is still **passed** rather than left to their default, and their own note on the field says
/// why: *"it sizes the container against these, so the value it sized for and the value the room
/// runs at must not be able to disagree."* Dropping the flag would not remove the second
/// computation ([`shard_queue_bytes`] still needs this number to size the memory limit) it would
/// only stop anything checking that the two agree, which is exactly how the `--outbound-budget`
/// unit bug reached the cluster.
///
/// ## Why the width does not size this, and why widening it did not help
///
/// `Shards::broadcast` enqueues one copy of the message into **every** shard's inbox. So the number
/// of broadcasts a room can buffer is exactly the depth, *however many shards there are*: widening
/// the fan-out multiplies the total memory and leaves broadcast headroom exactly where it was. That
/// is why the second dev-cluster run collapsed at the same point as the first, on 12 shards instead
/// of 2, with CPU at **0.3 of 2 cores**: nothing was compute-bound and nothing had more room.
///
/// ## What the burst actually is
///
/// A release fans out twice. The full feed chunks 140 items into one broadcast and is cheap. The
/// scoped feed does not amortize at all. `room.rs` ends a release with
///
/// ```text
/// for (target, messages) in scoped {
///     for chunk in messages.chunks(PRINT_JSON_CHUNK) {
///         out.broadcast(Recipients::SlotScopedText(target), chunk);
///     }
/// }
/// ```
///
/// **one broadcast per distinct receiver slot.** Every Puna room publishes the filtered port and
/// `wants_filtered` defaults on, so those recipients are real. One release is therefore about
/// `min(locations-per-slot, slots)` broadcasts, and pahoa's own note agrees: *"a 2000-slot release
/// produces ~2,860 broadcast frames"*.
///
/// ## The connection term alone was a constant, which is what made this necessary
///
/// pahoa's first derivation had only the reconnect-storm half, and `shards_for` divides by 512 while
/// it multiplied the per-shard connection count by 8, so the two cancelled: **4,096 for every room
/// from 1 slot to ~5,461**, rising only once the shard count clamped at 32. The release burst grows
/// linearly with slots and that term did not grow at all, so headroom measured in concurrent
/// releases was `4096 / slots`: about 20 at 200 slots, 8 at 500, and **2 at 2000**. The room that
/// failed was taking 14 goals a second.
///
/// The storm term is kept rather than replaced, because it is the right model for the burst it
/// names and dominates where the release term does not: a room past pahoa's 32-shard ceiling has
/// each shard owning far more than 512 connections. Taking the larger costs nothing and needs no
/// argument about which case a given room is in.
///
/// **It is bounded by slots rather than by locations on purpose.** The receiver count caps the
/// burst, so slots alone is an upper bound, which over-provisions a shallow seed by a few MiB of
/// envelopes and needs nothing plumbed through [`crate::cluster::RoomSpec`] that is not already
/// there. The safe direction, cheaply.
///
/// ## What this does not fix
///
/// Depth absorbs a *burst*. It does nothing about sustained overproduction: at 14 goals a second the
/// room was producing broadcasts faster than it could deliver them, and a deeper queue defers that
/// rather than preventing it. What it buys is the ability to tell the two apart: a room that
/// collapses again with a deep queue *and* CPU pinned at its limit is a compute problem, which is a
/// different lever and one this project has deliberately not pulled yet.
pub fn shard_queue_depth(slot_count: i32) -> i64 {
    /// Headroom per connection one shard owns, for the reconnect storm. Divides by the width,
    /// because that burst really is bounded by the connections a single shard holds.
    const MESSAGES_PER_CONNECTION: i64 = 8;
    /// Simultaneous releases to keep room for, for the release tail. Divides by nothing: each costs
    /// up to one broadcast per receiver slot, and a broadcast occupies a slot in *every* shard's
    /// inbox. Troy's number, adopted by pahoa verbatim.
    const CONCURRENT_RELEASES: i64 = 16;
    /// pahoa's own default, and the floor here for the same reason it is theirs: it is what every
    /// room ran at before either knob existed, and it has never been the thing that failed alone.
    const FLOOR: i64 = 4096;
    /// pahoa refuses more, on the grounds that a room this far behind will not catch up by queuing.
    /// Left where it is deliberately: broadcast headroom costs `shards × depth` and buys `depth`,
    /// so raising the ceiling is the most expensive way to buy it. The cheap way is pahoa's next
    /// change: stop broadcasting slot-scoped traffic to shards that own nobody.
    const CEILING: i64 = 65536;

    let slots = i64::from(slot_count.max(0));
    let reconnect_storm = {
        let shards = shards(slot_count);
        // Written out rather than `div_ceil`, which is still unstable for signed integers.
        ((expected_connections(slot_count) + shards - 1) / shards)
            .saturating_mul(MESSAGES_PER_CONNECTION)
    };
    let release_tail = slots.saturating_mul(CONCURRENT_RELEASES);

    reconnect_storm.max(release_tail).clamp(FLOOR, CEILING)
}

/// Memory the shard inboxes reserve up front, which **the outbound budget does not cover**.
///
/// Asked of pahoa and answered explicitly: `outbound_budget_bytes` is charged in `deliver()`, which
/// runs *after* a shard has expanded the audience and is queuing a frame for a specific connection.
/// A message still sitting in a shard's inbox has not been expanded yet, so nothing has reserved for
/// it, which is why the dev room could queue **zero** bytes while its shards overflowed. Two
/// queues, and only the second one is metered.
///
/// So this is a third term in the memory sizing rather than something already inside the budget.
/// It is the **envelope** cost: `shards × depth` messages, reserved by `mpsc::channel` at startup
/// rather than grown on demand, so it is resident from the first second regardless of load. The
/// payloads the envelopes point at are refcounted `Bytes`, one allocation per broadcast however many
/// shards hold a handle, so their footprint follows what the room has in flight rather than the
/// depth; pahoa's guidance is to size for the envelopes and ignore the payloads.
///
/// `size_of::<ShardMsg>()` is 72 on x86-64. pahoa declines to pin a floor to it and states it in
/// their README as stable enough to size against, so a drift here costs a slightly-off limit rather
/// than a failure, which is the right sort of dependency to take on another repository's layout.
pub fn shard_queue_bytes(slot_count: i32) -> i64 {
    const SHARD_MSG_BYTES: i64 = 72;

    shards(slot_count)
        .saturating_mul(shard_queue_depth(slot_count))
        .saturating_mul(SHARD_MSG_BYTES)
}

/// Connections a room of this size is expected to hold.
///
/// One player commonly runs a game client, a text client and a tracker. The same rule the outbound
/// budget uses, and pahoa's own `CONNECTIONS_PER_SLOT`, which is what makes the fan-out numbers
/// here reproduce theirs exactly.
fn expected_connections(slot_count: i32) -> i64 {
    const CONNECTIONS_PER_SLOT: i64 = 3;

    i64::from(slot_count.max(0)).saturating_mul(CONNECTIONS_PER_SLOT)
}

/// Everything in the process that is **not** the outbound queue: the restored save, the location
/// and item tables, per-connection state, and the allocator's retained slack.
///
/// ## Measured, which the note this replaces asked for
///
/// That note said to *"replace with measurement once `/admin/v1/status` reports `resident_bytes`"*.
/// It does, the orchestrator has been re-exporting it since M11, and two 2000-slot rooms were
/// OOM-killed before anybody read it. Taken from Prometheus across every room size dev has run,
/// as `process_resident_memory_bytes - pahoa_outbound_queued_bytes`:
///
/// | slots | at rest | under load |
/// |---|---|---|
/// | 1 | 46 MiB | 50 MiB (1 connection) |
/// | 4 | 49 MiB | 70 MiB (2 connections) |
/// | 200 | 66 MiB | 74 MiB (201 connections) |
/// | 2000 | 125 MiB | **494 MiB** (1993 connections) |
///
/// **The load column is not a function of connections alone**, which is the finding that shaped
/// these constants. Two 2000-slot rooms at ~2000 connections measured 208 MiB and 494 MiB; the
/// difference is churn: the second had taken 20,000 reconnects, and freed connection state is
/// retained by the allocator rather than returned. At rest a 2000-slot room is flat to within
/// 3 MiB for hours, so this is fragmentation and not a leak, but a limit has to cover the churny
/// case because a room under load is exactly the room that is being dropped and re-joined.
///
/// So the per-slot term is fitted to the **worst** observation rather than the median.
///
/// ## The floor is deliberately far above what a small room measured
///
/// It began at 192 MiB against 46-70 MiB observed, chosen so no room's limit got smaller than the
/// one it already ran under. Troy's call, and the reasoning was that those small-room samples come
/// from rooms with one or two connections (nothing about them says what a 200-slot room does
/// mid-cascade) so generosity cost little and tightening later could be done against data.
///
/// ## It is 160 MiB now, and the 32 MiB came off for a reason rather than a preference
///
/// **Every measurement in that table included a term that no longer exists.** pahoa's journal
/// writer used a flat `sync_channel(1 << 19)`, sized for the worst burst a 2000-slot seed can
/// produce and then allocated in *every* room: 28 MiB resident from startup whether or not
/// anything was ever queued. It now sizes from the seed's own location count, which pahoa measured
/// as a flat **30.7 MB** off every room from 1 slot to 96.
///
/// So the fit was carrying a constant that has been removed, and it was carrying it in the one term
/// that is itself a constant. `PER_SLOT` is untouched: a 2000-slot seed's ring is what the old
/// figure was sized for, so the largest rooms saved nothing and their term should not move.
///
/// **This is still generous, which is the point of doing it without new load.** A minimal room is
/// ~14 MB after the change (pahoa's breakdown: ~10 MB of mimalloc arena, ~2 MB of binary and
/// runtime, ~2 MB of journal), so 160 MiB is an order of magnitude above the floor case and around
/// 4× the largest small-room observation once its ring is subtracted.
///
/// It does give up the "nothing shrinks" property the 192 was chosen for: a room under 228 slots
/// now limits at ~330 MiB rather than 352. That was a transition rule for a fleet already running,
/// not a standing invariant, and the room it applies to has 30 MB less in it than when the rule was
/// written.
///
/// **The measurements below are deliberately left alone.** They are pre-repin, so they overstate
/// every room by the ring, and holding the limit to them is stricter than reality, which is the safe
/// direction, and re-fitting them properly wants a week of `process_resident_memory_bytes` from a
/// fleet on the new image. `PUNA_PAHOA_IMAGE` reaching every room is what makes that data exist.
fn non_queue_bytes(slot_count: i32) -> i64 {
    /// See above. **160 MiB, down from 192**, because every measurement the fit was drawn from
    /// included a term pahoa has since removed.
    const FLOOR: i64 = 160 * 1024 * 1024;
    /// Fitted to the 2000-slot room under churn: 2000 × 288 KiB = 562 MiB over the floor, so 754
    /// against 494 measured.
    ///
    /// **Its equality with pahoa's own per-slot budget contribution (3 × 96 KiB) is a coincidence:**
    /// both scale with the connections a slot implies, but one is queue depth and this is
    /// resident state. Do not "simplify" this function to `budget + FLOOR`: pahoa owns the 96 KiB
    /// and can change it, and the two would then move together for no reason.
    const PER_SLOT: i64 = 288 * 1024;

    let slots = i64::from(slot_count.max(0));
    FLOOR.saturating_add(slots.saturating_mul(PER_SLOT))
}

/// What to request, in bytes: the room's ordinary footprint plus room to queue.
///
/// A quarter of the budget rather than all of it, because the request is the **scheduling
/// reservation** (what the node sets aside and what protects the room from eviction) and
/// reserving a queue depth the room reaches only under a cascade would price every room at its
/// worst minute. The limit is where the worst minute is covered.
///
/// [`shard_queue_bytes`] is in at **full** value rather than a quarter, because unlike the outbound
/// queue it is not a depth the room grows into: `mpsc::channel` reserves it at startup, so it is
/// resident in the first second of an empty room.
pub fn memory_request_bytes(slot_count: i32) -> i64 {
    non_queue_bytes(slot_count)
        + shard_queue_bytes(slot_count)
        + outbound_budget_bytes(slot_count) / 4
}

/// What to request, in **millicores**: steady-state demand, which scales with the room.
///
/// ## A flat request prices every room like the largest one
///
/// This was 50m for every room, which Troy measured as overstating the smallest by nearly an order
/// of magnitude. The request is what the scheduler subtracts from a node and what a ResourceQuota
/// charges, so it decides how many rooms fit on the fleet, and a one-slot async holding three
/// sockets and writing a save every thirty seconds is not the same workload as a 2000-slot sync
/// mid-cascade. Sizing them alike means either over-reserving the small end or under-reserving the
/// large one, and 50m did the first.
///
/// **10m at the floor, one core at 2000 slots, linear between.** Troy's numbers. The top of the
/// scale is generous against measurement rather than fitted to it: a 2000-slot room under a full
/// goal cascade measured **0.6 of 2 cores**, and 0.2 during a clean connect ramp, so a whole core
/// reserved is roughly 1.7× the worst thing observed.
///
/// ## Capped at a core, which is a correctness guard and not only a policy
///
/// Kubernetes **rejects a pod whose request exceeds its limit**, and the limit is 2 cores. Extended
/// linearly this crosses 2000m at about 4000 slots, and **nothing bounds a room's slot count**: it
/// is whatever the uploaded seed's `slot_info` declares, ingest imposes no maximum, and
/// `generations.slots` is a plain `INTEGER`. (The port range bounds how many rooms run at once, not
/// how large one is.) M38's sizing table already reasons out to 6000 slots.
///
/// So an uncapped ramp makes a large room unschedulable, with the failure arriving as an admission
/// error naming resources rather than anything about the room.
///
/// Capping at the top of Troy's stated scale keeps the request honest and leaves the burst headroom
/// where it belongs: a 4000-slot room still bursts to the 2-core limit, it just does not reserve
/// more than the largest measured room needs.
pub fn cpu_request_millicores(slot_count: i32) -> i64 {
    /// What a room costs while it is doing nothing, which is nearly all of the time.
    const FLOOR: i64 = 10;
    /// The top of the ramp, and the cap. See above for why the cap is load-bearing.
    const CEILING: i64 = 1000;
    /// The slot count at which the ceiling is reached.
    const FULL_AT: i64 = 2000;

    let slots = i64::from(slot_count.max(0));
    let scaled = FLOOR + (CEILING - FLOOR) * slots / FULL_AT;
    scaled.min(CEILING)
}

/// What to limit at, in bytes: the **whole** outbound budget, plus half again the base.
///
/// ## The invariant, and it was violated
///
/// A room must be able to reach its own outbound cap before the kernel reaches the room. That is
/// the entire point of the cap: pahoa sheds one slow client rather than dying, so if the cgroup
/// binds first then everybody's connection dies instead of one, and the backpressure that was
/// supposed to protect the room never gets to run.
///
/// The previous formula (`budget × 3/2 + 256 MiB`) did not hold it at scale. For a 2000-slot
/// room that is 1099 MiB against a 562 MiB budget, leaving 537 MiB above the queue for everything
/// else; the room measured 494 MiB of base at 2000 connections and was OOM-killed three times.
/// The 256 MiB was flat, and the thing it was covering is not: it scales with slots and with the
/// connections they imply.
///
/// Asserted by `the_limit_lets_a_room_reach_its_own_outbound_cap`, which is the test that would
/// have caught this before a room did.
///
/// ## The third term
///
/// [`shard_queue_bytes`] is added at face value, with no headroom multiplier, because it is a fixed
/// reservation rather than something the room grows into. It is small (3.4 MiB for a 2000-slot
/// room, under 10 MiB at any size this derivation produces) and it is here because it is
/// **structurally outside the outbound budget**, which is the one thing about it worth remembering.
///
/// It does double-count by about 576 KiB against the base measurements above, which were taken on
/// rooms running two shards of 4096, inside the noise of a fit whose per-slot term is drawn from
/// the worst observation, and in the safe direction.
pub fn memory_limit_bytes(slot_count: i32) -> i64 {
    outbound_budget_bytes(slot_count)
        + shard_queue_bytes(slot_count)
        + non_queue_bytes(slot_count) * 3 / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft {
            room_id: RoomId::new(),
            image: "registry.example/pahoa:sha-abc123".into(),
            base_port: 40000,
            wants_filtered: true,
            slot_count: 96,
            save_interval_secs: 30,
            use_embedded_options: true,
        }
    }

    fn env(pairs: &[(&str, &str)]) -> SecretData {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn token_only() -> SecretData {
        env(&[("PAHOA_ADMIN_TOKEN", &"a".repeat(52))])
    }

    fn hash(draft: &Draft, slot_auth: SlotAuth, env: &SecretData) -> String {
        draft.clone().build(slot_auth, env).spec_hash
    }

    #[test]
    fn the_same_inputs_always_hash_the_same() {
        let draft = draft();
        assert_eq!(
            hash(&draft, SlotAuth::None, &token_only()),
            hash(&draft, SlotAuth::None, &token_only())
        );
        // Two rooms with identical settings agree, because the id is deliberately not an input.
        let mut other = draft.clone();
        other.room_id = RoomId::new();
        assert_eq!(
            hash(&draft, SlotAuth::None, &token_only()),
            hash(&other, SlotAuth::None, &token_only())
        );
    }

    /// Pins the canonical form. A change here is a change to what every existing room's annotation
    /// means, so it should cost a deliberate edit, and `CANONICAL_VERSION` is the honest way to
    /// make one.
    #[test]
    fn the_canonical_form_is_pinned() {
        let draft = Draft {
            room_id: RoomId::new(),
            image: "pahoa:test".into(),
            base_port: 40000,
            wants_filtered: true,
            slot_count: 4,
            save_interval_secs: 30,
            use_embedded_options: true,
        };
        let env = env(&[("PAHOA_ADMIN_TOKEN", "token")]);
        assert_eq!(
            draft.canonical(SlotAuth::None, &env),
            "puna/room-spec/1\n\
             image=pahoa:test\n\
             base_port=40000\n\
             filtered=true\n\
             slot_count=4\n\
             shards=2\n\
             shard_queue_depth=4096\n\
             save_interval=30\n\
             use_embedded_options=true\n\
             slot_auth=none\n\
             env=PAHOA_ADMIN_TOKEN=token\n"
        );
    }

    /// Every field of the pod spec has to move it, or a change to that field never reaches a
    /// running room.
    #[test]
    fn every_spec_field_moves_the_hash() {
        let base = hash(&draft(), SlotAuth::None, &token_only());

        /// A named change to one field, so the failure message says which field went unhashed.
        type Mutation = (&'static str, Box<dyn Fn(&mut Draft)>);

        let mutations: Vec<Mutation> = vec![
            (
                "image",
                Box::new(|d: &mut Draft| d.image = "pahoa:next".into()),
            ),
            ("base_port", Box::new(|d: &mut Draft| d.base_port = 40002)),
            (
                "wants_filtered",
                Box::new(|d: &mut Draft| d.wants_filtered = false),
            ),
            ("slot_count", Box::new(|d: &mut Draft| d.slot_count = 97)),
            (
                "save_interval",
                Box::new(|d: &mut Draft| d.save_interval_secs = 60),
            ),
            (
                "use_embedded_options",
                Box::new(|d: &mut Draft| d.use_embedded_options = false),
            ),
        ];

        for (field, mutate) in mutations {
            let mut draft = draft();
            mutate(&mut draft);
            assert_ne!(
                hash(&draft, SlotAuth::None, &token_only()),
                base,
                "changing {field} must recreate the pod"
            );
        }
    }

    /// The mode moves nothing in the manifest, which is exactly why it has to be in the hash: it
    /// arrives through the Secret, and pahoa reads it once at startup.
    #[test]
    fn the_password_mode_moves_the_hash_though_the_manifest_is_identical() {
        let draft = draft();
        let none = hash(&draft, SlotAuth::None, &token_only());

        let room_mode = hash(
            &draft,
            SlotAuth::Room,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_PASSWORD", "open-sesame"),
            ]),
        );
        let per_slot = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"a","2":"b"}"#),
            ]),
        );

        assert_ne!(none, room_mode);
        assert_ne!(none, per_slot);
        assert_ne!(room_mode, per_slot);
    }

    /// The one exclusion, and the whole reason `POST /admin/v1/slots/<n>/password` is worth having:
    /// rotating one player's password must not bounce everyone else's room.
    #[test]
    fn rotating_a_slot_password_does_not_move_the_hash() {
        let draft = draft();
        let before = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"old","2":"b"}"#),
            ]),
        );
        let after = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"new","2":"b"}"#),
            ]),
        );
        assert_eq!(before, after);
    }

    /// ...but the map's shape does move it. A slot with no entry is refused under the fail-closed
    /// rule, so its password has to reach the pod, and only a restart does that.
    #[test]
    fn adding_a_slot_to_a_per_slot_room_moves_the_hash() {
        let draft = draft();
        let two = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"a","2":"b"}"#),
            ]),
        );
        let three = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"a","2":"b","3":"c"}"#),
            ]),
        );
        assert_ne!(two, three);

        // The map's own key order must not matter: it is a JSON object, and only its membership
        // is a fact about the room.
        let reordered = hash(
            &draft,
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"2":"b","1":"a"}"#),
            ]),
        );
        assert_eq!(two, reordered);
    }

    /// Startup-only values are covered, all of them.
    ///
    /// A rotated admin token that has not reached the pod makes every console call fail with a
    /// `404`, which reads as "this room is running an old image": the most confusing possible
    /// symptom for the most routine possible operation.
    #[test]
    fn every_startup_only_credential_moves_the_hash() {
        let draft = draft();

        let old_token = hash(
            &draft,
            SlotAuth::None,
            &env(&[("PAHOA_ADMIN_TOKEN", "old")]),
        );
        let new_token = hash(
            &draft,
            SlotAuth::None,
            &env(&[("PAHOA_ADMIN_TOKEN", "new")]),
        );
        assert_ne!(
            old_token, new_token,
            "rotating the admin token needs a restart"
        );

        let before = hash(
            &draft,
            SlotAuth::Room,
            &env(&[("PAHOA_ADMIN_TOKEN", "t"), ("PAHOA_PASSWORD", "old")]),
        );
        let after = hash(
            &draft,
            SlotAuth::Room,
            &env(&[("PAHOA_ADMIN_TOKEN", "t"), ("PAHOA_PASSWORD", "new")]),
        );
        assert_ne!(
            before, after,
            "pahoa has no live password setter, deliberately, so this is a restart"
        );

        // And a key nobody thought about is covered by default: the exclusion is a list of one.
        let with_server_password = hash(
            &draft,
            SlotAuth::None,
            &env(&[("PAHOA_ADMIN_TOKEN", "t"), ("PAHOA_SERVER_PASSWORD", "s")]),
        );
        let without = hash(&draft, SlotAuth::None, &env(&[("PAHOA_ADMIN_TOKEN", "t")]));
        assert_ne!(with_server_password, without);
    }

    /// **No password value may reach the canonical form**, whatever the map contains.
    ///
    /// The rendered string is what gets hashed, so this reads it directly rather than comparing two
    /// hashes: a hash comparison proves the values did not *change* the fingerprint, and this
    /// proves they were never in it. Cheap, and it is the assertion that survives somebody
    /// rewriting how the line is built.
    #[test]
    fn no_slot_password_reaches_the_canonical_form() {
        let canonical = draft().canonical(
            SlotAuth::PerSlot,
            &env(&[
                ("PAHOA_ADMIN_TOKEN", "t"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1":"hunter2","2":"swordfish"}"#),
            ]),
        );

        assert!(canonical.contains("env=PAHOA_SLOT_PASSWORDS=slots:1,2\n"));
        for password in ["hunter2", "swordfish"] {
            assert!(
                !canonical.contains(password),
                "a slot password reached the fingerprint:\n{canonical}"
            );
        }
    }

    /// Pahoa's own numbers, so a drift in either direction is visible here.
    #[test]
    fn the_memory_budget_matches_pahoas_heuristic() {
        // The floor: a small room does not get a cap so low it binds during ordinary play.
        assert_eq!(outbound_budget_mib(1), 64);
        assert_eq!(outbound_budget_mib(0), 64);
        // 228 slots is where three connections at 96 KiB each first passes the floor; at 227 the
        // formula is still below it and the floor is what binds.
        assert_eq!(outbound_budget_mib(227), 64);
        assert_eq!(
            outbound_budget_mib(228),
            (228 * 3 * 96 * 1024i64 + MIB - 1) / MIB
        );
        // pahoa's own heuristic puts a 2000-slot room at 562.5 MiB; rounded up so the cap is never
        // quieter than the default it replaces.
        assert_eq!(outbound_budget_mib(2000), 563);
        assert!(outbound_budget_bytes(2000) >= 562 * MIB + 512 * 1024);

        // A negative slot count cannot come from the database, but it must not become a huge
        // request if it ever does.
        assert_eq!(outbound_budget_mib(-5), 64);
    }

    /// **The argv value is in MiB, and the sizing is in bytes, and they must mean the same cap.**
    ///
    /// This is the assertion that was missing when Puna passed `--outbound-budget=589824000`
    /// intending 562.5 MiB and configured 562 TiB. Nothing else could have caught it: pahoa accepts
    /// the number, the banner prints it without comment, and per-connection caps keep shedding slow
    /// clients so a room with no room-wide backstop looks entirely normal, right up until many
    /// clients stall at once and it queues until the kernel kills it.
    #[test]
    fn the_argv_budget_is_the_same_cap_the_container_is_sized_against() {
        use crate::cluster::RoomSpec;
        use crate::spec::args::serve;

        for slots in [1, 4, 96, 200, 228, 2000] {
            let spec = RoomSpec {
                room_id: RoomId::new(),
                spec_hash: "hash".into(),
                image: "registry.example/pahoa:sha-abc123".into(),
                base_port: 40000,
                wants_filtered: true,
                slot_count: slots,
                save_interval_secs: 30,
                use_embedded_options: true,
            };

            let arg = serve(&spec)
                .into_iter()
                .find_map(|a| a.strip_prefix("--outbound-budget=").map(str::to_string))
                .expect("every room is told its own budget");
            let mib: i64 = arg.parse().expect("a bare number of MiB");

            // The unit, stated as the multiplication pahoa itself performs.
            assert_eq!(
                mib * MIB,
                outbound_budget_bytes(slots),
                "{slots} slots: the room is told {mib} MiB but sized for \
                 {} MiB of queue",
                outbound_budget_bytes(slots) / MIB
            );
            // And a sanity bound, because the failure this exists for was six orders of magnitude:
            // no room's cap belongs anywhere near a terabyte.
            assert!(
                mib < 1024 * 1024,
                "{slots} slots: a cap of {mib} MiB is not a room's outbound queue"
            );
        }
    }

    /// Both numbers are pahoa's own derivation, and this pins the values rather than the formula.
    ///
    /// Puna passes both flags, so Puna wins wherever the two disagree, which makes a disagreement
    /// a container sized for a fan-out nobody chose, in silence. The depth was briefly Puna's alone;
    /// pahoa adopted the release term in `9c382ab`, so the rows below are equally theirs and ours
    /// and a drift in either direction fails here.
    #[test]
    fn the_fan_out_is_pahoas_own_derivation() {
        // pahoa's own worked table for the width. The two-shard row is what the first dev-cluster
        // run failed at; the twelve-shard row is what a 2000-slot room gets now.
        for (slots, want_shards) in [(1, 2), (4, 2), (200, 2), (2000, 12), (6000, 32)] {
            assert_eq!(shards(slots), want_shards, "{slots} slots: fan-out width");
        }

        // The depth: 16 concurrent releases' worth, floored at pahoa's constant.
        for (slots, want_depth) in [
            (1, 4096),
            (200, 4096),
            (256, 4096),
            (500, 8000),
            (2000, 32_000),
            (6000, 65_536),
        ] {
            assert_eq!(
                shard_queue_depth(slots),
                want_depth,
                "{slots} slots: inbox depth"
            );
        }

        // **The release term is what binds on every room worth worrying about**, and that is the
        // finding rather than an implementation detail. If it stopped dominating, the sizing would
        // quietly go back to being connection-shaped: the shape that gave a 2000-slot room two
        // concurrent releases of headroom and collapsed twice.
        let storm = |slots: i32| {
            let shards = shards(slots);
            ((expected_connections(slots) + shards - 1) / shards) * 8
        };
        for slots in [500, 1000, 2000, 4000, 6000] {
            let release = i64::from(slots) * 16;
            assert!(
                release > storm(slots),
                "{slots} slots: the reconnect term ({}) now exceeds the release term ({release}), \
                 so the depth is no longer sized against the burst that failed",
                storm(slots)
            );
        }

        // **The reconnect term never wins, at any size**, and that is asserted rather than assumed
        // because the first draft of this test assumed the opposite and was wrong.
        //
        // It falls out of `shards_for`: below the 32-shard clamp the width is at least
        // `connections / 512`, so the storm term is at most `512 × 8 = 4096` (the floor) and
        // above the clamp it is `3 × slots / 32 × 8`, which is `0.75 × slots` against a release term
        // of `16 × slots`. So `max()` is the release term or the floor, everywhere.
        //
        // Kept anyway, and transcribed rather than simplified away, because it is pahoa's
        // expression: the redundancy is a property of their two constants rather than of the shape,
        // and simplifying here would mean Puna silently not tracking a change to either.
        for slots in [1, 4, 200, 256, 500, 2000, 6000, 200_000] {
            assert!(
                storm(slots) <= (i64::from(slots) * 16).max(4096),
                "{slots} slots: the reconnect term ({}) now binds -- the comment above this \
                 assertion is stale and the sizing has changed shape",
                storm(slots)
            );
        }

        // The blast radius is the point of the exercise, so state it as itself rather than leaving
        // it implied by the width. 3000 is what shed ~2500 connections at once on the cluster.
        assert_eq!(expected_connections(2000) / shards(2000), 500);
        assert_eq!(expected_connections(2000) / 2, 3000);

        // pahoa's bounds. A value outside them is a refused start, so it must be unrepresentable.
        for slots in [0, -5, 1, 200, 2000, 6000, i32::MAX] {
            let (s, d) = (shards(slots), shard_queue_depth(slots));
            assert!(
                (1..=32).contains(&s),
                "{slots} slots: {s} shards is out of range"
            );
            assert!(
                (4096..=65536).contains(&d),
                "{slots} slots: a depth of {d} is out of range"
            );
        }

        // The envelopes stay a small term against the limit at every size, which is what makes
        // adding them at face value rather than with headroom the right call. 6000 slots is the
        // worst case the bounds allow: 32 x 65,536 x 72 = 144 MiB.
        for slots in [1, 200, 2000, 6000] {
            assert!(
                shard_queue_bytes(slots) * 20 < memory_limit_bytes(slots),
                "{slots} slots: {} MiB of envelopes is no longer a small term",
                shard_queue_bytes(slots) / MIB
            );
        }
    }

    /// The headroom this sizing exists to buy, stated in the units the failure was measured in.
    ///
    /// A release broadcasts once per distinct receiver slot, so a room absorbs `depth / slots`
    /// simultaneous releases. Under pahoa's flat 4,096 a 2000-slot room absorbed **two**, and the
    /// run that collapsed was taking fourteen goals a second.
    #[test]
    fn a_room_absorbs_sixteen_simultaneous_releases() {
        for slots in [500, 1000, 2000, 4000] {
            let concurrent = shard_queue_depth(slots) / i64::from(slots);
            assert!(
                concurrent >= 16,
                "{slots} slots: only {concurrent} simultaneous releases of headroom"
            );
            let under_pahoas_default = 4096 / i64::from(slots);
            assert!(
                concurrent > under_pahoas_default,
                "{slots} slots: no better than the {under_pahoas_default} the default gave"
            );
        }
    }

    /// The mirror of `the_argv_budget_is_the_same_cap_the_container_is_sized_against`, for the term
    /// the outbound budget does not cover.
    #[test]
    fn the_argv_fan_out_is_what_the_container_is_sized_against() {
        use crate::cluster::RoomSpec;
        use crate::spec::args::serve;

        for slots in [1, 4, 200, 2000] {
            let spec = RoomSpec {
                room_id: RoomId::new(),
                spec_hash: "hash".into(),
                image: "registry.example/pahoa:sha-abc123".into(),
                base_port: 40000,
                wants_filtered: true,
                slot_count: slots,
                save_interval_secs: 30,
                use_embedded_options: true,
            };
            let argv = serve(&spec);
            let read = |flag: &str| -> i64 {
                argv.iter()
                    .find_map(|a| a.strip_prefix(flag))
                    .unwrap_or_else(|| panic!("{slots} slots: every room is told its {flag}"))
                    .parse()
                    .expect("a bare number")
            };

            let (told_shards, told_depth) = (read("--shards="), read("--shard-queue-depth="));
            assert_eq!(told_shards, shards(slots));
            assert_eq!(told_depth, shard_queue_depth(slots));
            // The product is what the memory limit reserves for, stated as pahoa's own arithmetic.
            assert_eq!(
                told_shards * told_depth * 72,
                shard_queue_bytes(slots),
                "{slots} slots: the room is told {told_shards}×{told_depth} but the container is \
                 sized for {} bytes of envelopes",
                shard_queue_bytes(slots)
            );
        }
    }

    #[test]
    fn the_limit_leaves_headroom_over_the_request() {
        for slots in [1, 96, 2000] {
            let request = memory_request_bytes(slots);
            let limit = memory_limit_bytes(slots);
            assert!(
                limit > request,
                "{slots} slots: {limit} must exceed {request}"
            );
            assert!(
                request > non_queue_bytes(slots),
                "the request has to reserve the room's ordinary footprint"
            );
        }
    }

    /// **A room must be able to reach its own outbound cap before the kernel reaches the room.**
    ///
    /// This is the property two dev rooms broke. pahoa's budget exists so that a room sheds one
    /// slow client instead of dying; if the container limit binds first then everybody's connection
    /// dies rather than one, and the backpressure never runs at all. It is one subtraction, and
    /// nothing asserted it: the old formula failed it at 2000 slots by 43 MiB against a base that
    /// measured 494.
    #[test]
    fn the_limit_lets_a_room_reach_its_own_outbound_cap() {
        for slots in [0, 1, 4, 96, 200, 227, 228, 1000, 2000, 5000] {
            let budget = outbound_budget_bytes(slots);
            let limit = memory_limit_bytes(slots);
            let base = non_queue_bytes(slots);
            assert!(
                limit - budget >= base,
                "{slots} slots: a limit of {limit} over a budget of {budget} leaves \
                 {} for a base of {base} -- the room is killed before its own cap binds",
                limit - budget
            );

            // **The shard envelopes are a fourth thing that is live at the same time**, and they
            // are outside the budget's accounting, so the cap is only reachable if the limit
            // holds all three at once.
            //
            // This does NOT hold the envelope term in place, and it is worth saying so rather than
            // letting a later reader assume it does: the `base × 3/2` headroom is 114 MiB at 96
            // slots against 576 KiB of envelopes, so the assertion stays true with the term
            // deleted. That is the honest state of it (the envelopes are a rounding error in the
            // limit) and `the_memory_numbers_are_pinned` is what actually catches the deletion.
            let envelopes = shard_queue_bytes(slots);
            assert!(
                limit >= budget + base + envelopes,
                "{slots} slots: a limit of {limit} does not hold a {budget}-byte queue, a \
                 {base}-byte base and {envelopes} bytes of shard inbox at the same time"
            );
        }
    }

    /// **The ramp, and the guard that stops a large room becoming unschedulable.**
    ///
    /// Troy's numbers: 10m at the floor, a whole core at 2000 slots. The values in between are
    /// linear and are asserted so a change to the shape has to be deliberate.
    ///
    /// The cap is the half worth testing hardest. Kubernetes **rejects a pod whose request exceeds
    /// its limit**, and `limits.cpu` is 2 cores; extended linearly this ramp crosses 2000m at about
    /// 4000 slots. **Nothing bounds a room's slot count** (it is whatever the uploaded seed
    /// declares, and M38's sizing table reasons out to 6000) so without the cap that room fails
    /// admission with an error about resources and nothing about the room.
    #[test]
    fn the_cpu_request_ramps_from_a_floor_to_one_core_and_stops_there() {
        assert_eq!(cpu_request_millicores(0), 10, "the floor");
        assert_eq!(cpu_request_millicores(1), 10);
        assert_eq!(cpu_request_millicores(96), 57);
        assert_eq!(cpu_request_millicores(200), 109);
        assert_eq!(cpu_request_millicores(2000), 1000, "a core at the top");

        // Past the anchor it stops rather than continuing, and the reason is admission rather than
        // policy: `limits.cpu` is 2 cores and a request above a limit is a pod Kubernetes refuses.
        for slots in [2001, 4000, 6000, i32::MAX] {
            assert_eq!(
                cpu_request_millicores(slots),
                1000,
                "{slots} slots requests more than the top of the ramp"
            );
        }

        // The invariant behind the cap, stated over the limit rather than over the constant, so it
        // still holds if either moves.
        const LIMIT_MILLICORES: i64 = 2000;
        for slots in [0, 1, 96, 200, 2000, 6000, i32::MAX] {
            assert!(
                cpu_request_millicores(slots) <= LIMIT_MILLICORES,
                "{slots} slots requests more CPU than the limit allows, so the pod is rejected"
            );
        }
    }

    /// Nothing here reserves more than it is allowed to burst to, in either resource.
    ///
    /// The memory pair has held since M37 and is asserted there; this is the CPU half, which had no
    /// upper relationship at all while the request was a constant somebody could raise.
    #[test]
    fn no_room_requests_more_than_its_limit() {
        for slots in [0, 1, 4, 96, 200, 500, 2000, 6000] {
            assert!(
                memory_request_bytes(slots) <= memory_limit_bytes(slots),
                "{slots} slots reserves more memory than it may use"
            );
        }
    }

    /// The rendered numbers, pinned, because two of the terms are too small to be held by any
    /// assertion about the invariant.
    ///
    /// A limit that merely equals `memory_limit_bytes` asserts nothing, and the shard envelopes are
    /// a rounding error against the base headroom, so dropping them from either formula changes
    /// no property, only the number. Pinning the number is the only thing that notices. Same shape
    /// as `the_canonical_form_is_pinned`, and the same rule: a change here should be a deliberate
    /// edit with the arithmetic re-done, not a test updated to whatever the code now returns.
    #[test]
    fn the_memory_numbers_are_pinned() {
        // Spelled as the arithmetic rather than as a number, so a reader can check the terms rather
        // than trust the total. Each is `base + envelopes + budget/4` for the request and
        // `budget + envelopes + base × 3/2` for the limit; `base` is the 160 MiB floor plus the
        // per-slot term.

        // 96 slots: 64 MiB budget (its floor) + 2×4096×72 envelopes + (160 MiB + 96×288 KiB) base.
        const BASE_96: i64 = 160 * 1024 * 1024 + 96 * 288 * 1024;
        assert_eq!(memory_request_bytes(96), BASE_96 + 589_824 + 67_108_864 / 4);
        assert_eq!(
            memory_limit_bytes(96),
            67_108_864 + 589_824 + BASE_96 * 3 / 2
        );

        // 2000 slots: 563 MiB budget + 12×32000×72 envelopes + (160 MiB + 2000×288 KiB) base.
        // The envelope term is 26.4 MiB here rather than 3.4: the depth is Puna's own, sized for
        // sixteen simultaneous releases rather than pahoa's flat 4,096.
        const BASE_2000: i64 = 160 * 1024 * 1024 + 2000 * 288 * 1024;
        assert_eq!(
            memory_request_bytes(2000),
            BASE_2000 + 27_648_000 + 590_348_288 / 4
        );
        assert_eq!(
            memory_limit_bytes(2000),
            590_348_288 + 27_648_000 + BASE_2000 * 3 / 2
        );

        // The totals, as flat numbers, because the expressions above would follow the constants if
        // somebody changed one: these are what a reviewer compares against a manifest.
        assert_eq!(memory_request_bytes(96), 213_450_752);
        assert_eq!(memory_limit_bytes(96), 361_824_256);
        assert_eq!(memory_request_bytes(2000), 932_831_232);
        assert_eq!(memory_limit_bytes(2000), 1_754_390_528);
    }

    /// The sizing is held to what the cluster actually measured, so tightening it later means
    /// arguing with the numbers rather than editing a constant.
    ///
    /// Worst base observed per room size, from Prometheus as
    /// `process_resident_memory_bytes - pahoa_outbound_queued_bytes`. The 2000-slot figure is the
    /// churned one, not the calm one: the same size measured 208 MiB after a quiet run and 494 MiB
    /// after twenty thousand reconnects, and a limit that only covers the calm case is a limit that
    /// holds until somebody is actually playing.
    #[test]
    fn the_limit_covers_every_base_measured_on_the_cluster() {
        const MIB: i64 = 1024 * 1024;
        for (slots, measured_mib) in [(1, 50), (4, 70), (200, 74), (2000, 494)] {
            let measured = measured_mib * MIB;
            let headroom = memory_limit_bytes(slots) - outbound_budget_bytes(slots);
            assert!(
                headroom >= measured * 5 / 4,
                "{slots} slots: {} MiB of non-queue headroom against {measured_mib} MiB measured \
                 leaves under a quarter to spare",
                headroom / MIB
            );
        }

        // The exact limit three rooms were killed at, which must now be unreachable.
        assert!(
            memory_limit_bytes(2000) > 1099 * MIB,
            "the 2000-slot limit is back at the value that OOM-killed three times"
        );
    }
}
