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
played by `room-load` reach an end — see below.

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

### Connecting

`wss://` with the certificate verified against the host you name, so use the room's advertised
hostname rather than an address. There is deliberately no flag to turn that off.

Room-wide passwords work with `--password`. Per-slot password rooms are not supported.

**Connections open on a ramp**, `--connect-rate` a second, default 5. The first live run against a
200-slot room opened all 200 at once and lost 165 of them — and it was not the room, which peaked at
0.015 cores against a 2-core limit with zero throttled periods. Every arrival fans out to everybody
already connected and replays the newcomer's item history, so filling a room in one frame costs
about the square of its size. A ramp is also what a room actually looks like: players arrive over
minutes. `--connect-rate 0` opens them all at once, kept because reproducing that storm on purpose
is a measurement rather than a mistake.

**A big run will lose connections, and the tool now says so.** Every check broadcasts a `PrintJSON`
and a `RoomUpdate` to *every* connection, so the full feed costs about 47 KB delivered per check
received on a 200-slot room — 1.2 GB across a few runs, uncompressed. The room drops the connections
that cannot keep up, which is its backpressure working. Two things to do about it: point `--room` at
the room's **filtered port** (`base_port + 1`, shown on the room page), which exists precisely to
drop that firehose for clients that cannot take it, and read `pahoa_lag_disconnects_total` for the
room to confirm what dropped them. The progress line carries `(dropped N)` and the summary repeats
it, because a run that lost half its slots measured half a run.

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
