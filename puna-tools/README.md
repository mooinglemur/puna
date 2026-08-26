# puna-tools

Two development tools. Neither is deployed — the release jobs build `--bin puna-web` and
`--bin puna-orchestrator` by name, so nothing here reaches an image — but both are built, linted
and tested by `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`,
which is the bar the pickle writer in particular deserves.

## `make-generation` — a synthetic multiworld seed

Builds a generation zip uploadable to Puna as-is. Nothing in it names a real game, player or
Archipelago world.

```console
$ cargo run -p puna-tools --bin make-generation -- --slots 12 --locations 250 --out /tmp/seed.zip
wrote /tmp/seed.zip (33 KiB)
  seed name    16817246627550880712
  --seed       1234   (to reproduce this exact seed)
  slots        12 playing, 0 spectating
  checks       250 per slot, 3000 in the multiworld
  games        Hexcrawl Deluxe, Lanternfall, Nine Lives of Rusk, Gloomhaven Drift
```

`--locations` is the only size knob. Items and locations are the same number by construction —
each game's item table is `--locations` names, being `--locations - 1` ordinary ones plus `Goal` —
because that is upstream's own invariant for a well-behaved apworld: the pool exactly fills the
world. Locations are `<regional> <physical> <noun>`, *"Overworld Blue Goomba"*, drawn without
replacement from a space of a quarter of a million, so a small seed looks different every run.
`--seed` makes a run reproducible after the fact; the one it used is always printed.

Every slot gets exactly one `Goal` item, pooled and shuffled with everything else, so it may land in
its own world or anybody's. The seed embeds **`release_mode: "auto"`**, and that is what lets a room
played by `room-load` reach an end — see below. It also embeds **`collect_mode: "disabled"`**,
against pahoa's own default of `auto`: with collect on, a slot that goals is handed every
outstanding item addressed to it at once, so the cascade delivers twice over and the traffic stops
resembling play. Termination is unaffected — that rests on auto-release.

The generator stamp is **0.6.7**, the newest version upstream has actually released.

Each game's package carries a **checksum**, computed the way Archipelago computes it — sha1 over the
package's canonical JSON. It is not decoration: a client skips any game the server did not give it a
checksum for (`CommonClient.py:652`), never asks for its names, and renders every item and location
as a bare id. Seeds built before this was added do exactly that.

## `room-load` — synthetic check traffic

```console
$ cargo run -p puna-tools --bin room-load -- \
      --generation /tmp/seed.zip --room mw.ionium.us:45000 --rate 0.5
playing 12 slots (12 can goal) against mw.ionium.us:45000 at 0.5/s each, jitter 0.8
      2s  connected 12  checks 14  items 9  goaled 0/12
      ...
  every player has goaled; draining the release cascade for 10s
done: 2874 checks sent, 3000 items received, 12/12 goaled
```

One connection per slot. It reads slot names, games and `Goal` ids from the generation with
`pahoa-multidata` — the same parser Puna and pahoa use — so what it believes about a room cannot
drift from what the room believes.

**`--rate` is per slot.** The room's offered load is `rate × active slots`, which models a room:
a player checks at a human pace and the load is a consequence of how many are playing. It also means
slots burst independently rather than in one synchronized waveform.

**`--clients-per-slot 1-3` models what a player actually holds**: the game client, then a text
client, then a tracker. The extra two consume the firehose and answer heartbeats and nothing else —
which is all they can do, since Archipelago's `TextOnly` and `Tracker` tags make a connection
`no_locations` at the server, refused by name if it tries to check or claim a goal. They carry an
**empty `game`**, which is what lets the tag skip the game and per-slot version checks; that they
connect at all is the proof the tags registered, because an empty game without a non-game tag is
refused as `InvalidGame`.

It matters because **a room's outbound cost is per connection**. One socket per slot understates the
fan-out of a real room by about two thirds, and every rate that looks per-player is really
per-socket. Items received by the extra connections are deliberately not counted — each item arrives
once per socket, and counting them would multiply the run's item total by the clients per slot and
destroy the one number that can be checked against the room's own tracker.

**Any rate works, including a very slow one.** A window's budget is a whole number of checks, so
`--rate 0.01` — one check per slot every hundred seconds, a reasonable soak — used to round to a
budget of zero and send nothing at all, forever, while the connections sat there looking healthy.
The budget now comes from the rate's running total rather than one window's share, so a sub-window
rate simply lands in one window out of ten. The startup line says how long a fresh run would take
and when to expect the first check, because a slow run and a stuck one look identical until then.

**The rate is bursty.** It holds on average over a ten-second window and swings hard inside it,
because a flat check every `1/rate` seconds is the one shape a real room never produces — and the
shape that makes a queue look healthy, since nothing ever arrives together. `--jitter 0` is the flat
metronome, kept so a run can be made boring on purpose.

### Why a run ends

A slot stops checking the moment it receives its own `Goal` — whether it found it or somebody else
did — and **holds the connection open**, draining and answering pings until everyone is done.

Termination comes from `release_mode: auto` rather than from anything here. A goaled slot stops
checking, so its remaining locations would strand every item in them, possibly including another
slot's `Goal`. Auto-release empties that world instead, so goals cascade: nobody has goaled at the
start so everybody is checking, the first `Goal` found triggers a release, and inductively every
`Goal` reaches its owner.

**The linger at the end is the interesting part of the run.** The last goal releases every
unfinished world at once — the biggest `to_slot` burst a room ever produces — and closing at the
moment of the last goal would measure everything except it.

### Resuming

Stop it and start it again against the same room and it picks up where the room is: the to-send list
comes from `Connected`'s `missing_locations`, not from the seed. A slot that comes back with nothing
left to check declares its goal immediately, which is what stops a resumed run deadlocking on two
finished-but-silent slots each holding the other's `Goal`.

**A slot can also come back already *won*.** The room replays a slot's item history at connect, in
the same batch as `Connected`, and if this slot's `Goal` is in it then it finished last time however
many locations it has left. That replay is read rather than discarded — dropping it under-counted
items (234 of 240 on a six-slot run, the shortfall rising with connect order) and, worse, left such
a slot checking a world it had already won while the run waited for a goal that had already
happened.

**A mid-run reconnect is the same code path**, which is why it is safe: a slot that comes back takes
its to-send list from the room and re-reads its own history rather than trusting anything it
remembered. What it does carry across is only what the room cannot tell it — that it has already
counted those items, and that it has already told the run it goaled. Both are counted once however
many times a slot reconnects, because the item total is the one number checkable against the room's
own tracker and the goal tally is what ends the run.

### Connecting

`wss://` with the certificate verified against the host you name, so use the room's advertised
hostname rather than an address. There is deliberately no flag to turn that off.

Room-wide passwords work with `--password`. Per-slot password rooms are not supported.

**Each slot starts checking as soon as it is connected**, so load builds with the population the way
a room filling up does. There is no start gate when a ramp is in effect: the ramp already gives the
run its shape, where the two together bought a dead period the length of the ramp — 6.7 minutes at
2000 slots, looking for all the world like a stuck tool — followed by every slot starting in the
same instant. `--connect-rate 0` still gates, because with everybody dialing at once there is
nothing else to give the run a defined start.

A slot that *is* waiting keeps reading while it waits. pahoa pings every 20 seconds and closes a
peer that has not answered 20 seconds later — it is the only side that pings, since Archipelago's
own clients turn theirs off — so a silent connection is a dead one within 40 seconds, and from this
side it looks like a TLS EOF that names nothing.

**Connections open on a ramp**, `--connect-rate` a second, default 5. The first live run against a
200-slot room opened all 200 at once and lost 165 of them — and it was not the room, which peaked at
0.015 cores against a 2-core limit with zero throttled periods. Every arrival fans out to everybody
already connected and replays the newcomer's item history, so filling a room in one frame costs
about the square of its size. A ramp is also what a room actually looks like: players arrive over
minutes. `--connect-rate 0` opens them all at once, kept because reproducing that storm on purpose
is a measurement rather than a mistake.

**A lost connection comes back.** Every socket reconnects on an exponential backoff of its own —
half a second doubling to a thirty-second ceiling, jittered — and keeps trying for as long as the run
is unfinished. Nothing else about the slot restarts: the to-send list comes from the room's own
`missing_locations` on the way back in, exactly as [resuming](#resuming) does, so nothing is
re-checked and nothing is lost.

Two reasons it matters beyond convenience. **A room that sheds load is supposed to see the shed
clients return** — that is the half of backpressure a tool that gave up was never exercising — and a
run whose population only ever falls is quietly measuring something else, because every number after
the drop is per-survivor.

The backoff resets only after a connection has **held for thirty seconds**, not when the handshake
succeeds. A room under pressure accepts a connection and drops it again moments later, and resetting
on the accept would put that slot into a half-second redial loop against a room that has just said it
cannot cope. The jitter is there for the same reason: the connections a room sheds are shed together,
so undelayed they would all redial in the same instant — the connect storm the ramp exists to
prevent, aimed at a room already at its limit.

The progress line carries `connected 1455/2000 (drops 545, back 540)`: a population against what was
asked for, then how often the room dropped somebody and how many came back. **The first two are
different questions** — a run with 545 drops that ends full is a room that shed and recovered, which
used to look identical here to one that lost 545 slots for good.

**A big run will lose connections, and the tool now says so.** Every check broadcasts a `PrintJSON`
and a `RoomUpdate` to *every* connection, so the full feed costs about 47 KB delivered per check
received on a 200-slot room — 1.2 GB across a few runs, uncompressed. The room drops the connections
that cannot keep up, which is its backpressure working. Two things to do about it: point `--room` at
the room's **filtered port** (`base_port + 1`, shown on the room page), which exists precisely to
drop that firehose for clients that cannot take it, and read `pahoa_lag_disconnects_total` for the
room to confirm what dropped them. Every one of them reconnects, per above, so what a big run
produces now is a population that dips and recovers rather than one that only falls — and the drop
count is what says it happened at all.

**Every connection negotiates permessage-deflate**, which is why the WebSocket layer is pahoa's
(`pahoa-net`) rather than a crate from elsewhere. `tungstenite` rejects a frame with RSV1 set
outright, so a run through it was a population the room could share no compression with — every
broadcast written out in full, and every outbound number a worst case. pahoa made `Client` generic
over its stream at our ask, so this builds the TLS session itself and hands the stream over; the
count that negotiated is printed at the end and `pahoa_client_connections_total{deflate}` says the
same thing from the room's side.

The other consequence is that TLS is terminated here: the host you type is the name verified against
the certificate *and* the `Host:` header, so they cannot disagree.

## Using a synthetic seed as a test fixture

The generation-shaped suites in `puna-core` are gated on `PUNA_TEST_GENERATION_ZIP` and skip without
it, because real seeds are large and carry real players' names. A synthetic one satisfies them:

```console
$ cargo run -p puna-tools --bin make-generation -- --slots 12 --spectators 2 --locations 250 \
      --seed 1234 --out /tmp/synthetic.zip
$ PUNA_TEST_GENERATION_ZIP=/tmp/synthetic.zip cargo test --workspace
```

`ingest`, `names` and `promote` all run against it; `patch` skips itself, since a synthetic seed has
no patch members.
