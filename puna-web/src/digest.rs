//! Turning a room's tracker documents into the four tables a browser renders.
//!
//! **Pure**: documents in, view structs out. No Rocket, no database, no clock — `now` is an
//! argument. That is what lets every shape here be asserted from a JSON literal rather than from a
//! live room, which matters because the interesting failures are all *transformations* (a name
//! resolved in the wrong game, a hint counted twice, a slot's data appearing in another's view) and
//! none of them need I/O to be wrong in.
//!
//! ## Why Puna digests at all
//!
//! pahoa's live document is measured at **2.7 MB for a 185-slot room**, dominated by exactly the
//! two arrays these tables render, and the browser needs almost none of it: the multiworld view
//! wants one row per slot, and a slot's view wants one slot's arrays. Digesting server-side turns
//! both into tens of KB. It is also a capability decision — a page showing the multiworld's slot
//! table has no business holding every slot's location list — and it is what makes the *names*
//! possible at all, since the documents carry only numeric ids.
//!
//! ## The name-resolution rule is the reference's, and it is easy to get backwards
//!
//! Transcribed from `WebHostLib/templates/genericTracker.html`:
//!
//! | Value | Resolved in the game of |
//! |---|---|
//! | A received item | the **receiving** slot — whose tracker this is |
//! | A location in that slot's own table | that slot |
//! | A hint's **item** | `receiving_player` |
//! | A hint's **location** | `finding_player` |
//!
//! A hint therefore needs two *different* games' tables. Getting this wrong does not fail: it
//! produces real names of the wrong things, which is the hardest kind of wrong to notice.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use puna_core::artifact::SlotKind;
use puna_core::artifact::names::GameNames;
use puna_core::model::annotation::{PingPreference, ProgressionStatus};
use puna_core::model::slot::Slot;
use serde::Serialize;

/// How stale the underlying documents are, and when the client should ask again.
///
/// Sent on every view so the page never has to guess a polling cadence — the server knows the
/// document's own cache window, and asking faster than that cannot produce new data.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Freshness {
    /// RFC 3339. When the room last answered — **not** when this response was built.
    pub as_of: String,
    /// True when the room did not answer and this is the last thing it said, which for an async is
    /// most of its life.
    pub stale: bool,
    /// What the client should wait before polling again.
    pub next_poll_ms: u64,
}

/// One row of the multiworld slot table.
///
/// Deliberately carries **no rendered prose and no ids beyond the slot number**. `last_activity` is
/// an age in milliseconds computed *by the server*, so a skewed client clock cannot render a
/// negative age — the same discipline `/room/<id>/status` already uses for `since_ms` — and the
/// client keeps it ticking between polls without a fetch.
///
/// It also carries **no tracker id**. The client builds a drill-down from the id already in its own
/// URL; sending each slot's own would hand every viewer of the room tracker every player's
/// independent link, collapsing two deliberately separate capabilities.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SlotRow {
    pub slot: i32,
    pub name: String,
    pub game: String,
    pub spectator: bool,
    pub checks_done: i64,
    pub checks_total: i64,
    pub status: &'static str,
    /// `None` is **never**, and never is not 1970. Rendering an epoch date is the classic way to
    /// make an untouched slot look like an abandoned one.
    pub last_activity_ms_ago: Option<i64>,
    pub hints: usize,
    /// Whether anybody has taken this slot, and **`None` for a viewer not entitled to know**.
    ///
    /// Something the reference cannot show, because it does not know who is playing — which is an
    /// argument that Puna *can* and not that this audience *should*. It is the one column here that
    /// is not about the multiworld: it answers "who signed up", which is Puna's own sign-up sheet
    /// rather than anything about the game, on the page built to be handed to spectators.
    ///
    /// **The tracker id is independent of the room id precisely so it can go to people who should
    /// not get the room**, so "the room page shows this too" is the wrong test — the room page's
    /// audience is the one the organizers chose. Same tier as the roster's own identity column and
    /// as `may_see_spoiler`'s `players`: the room's staff, or somebody who holds a slot in it.
    ///
    /// Omitted from the JSON entirely rather than sent as `false`, so a client cannot mistake
    /// "withheld" for "unclaimed" — see the note in `tracker.js`, where that mistake is one
    /// falsy-check away.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed: Option<bool>,
    /// Who holds the slot and on what terms, for a room running the enhanced tracker.
    ///
    /// Absent for everybody else, which covers three different situations that all mean the same
    /// thing on the wire: the room has the feature off, the viewer is not one of its people, or the
    /// slot is unclaimed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<SlotOwner>,
    /// The player's own account of how far along they are, as a label. **Absent for `unknown`**
    /// rather than sent as the word: a row saying "Unknown" beside four saying something real reads
    /// as a fifth answer, and this way the client renders a chip if and only if there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progression: Option<&'static str>,
    /// The slot's note, where there is one and this viewer may read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Whether this viewer may change the two above: the slot's own holder, or the room's staff.
    ///
    /// **Decided here rather than by the client comparing ids**, which would mean sending every
    /// viewer their own id and every row's owner id and trusting the comparison. The client renders
    /// a control if and only if this says so, and the route re-checks regardless — a control is a
    /// courtesy and the guard is the rule.
    ///
    /// Absent rather than `false` for everybody else, so a row for a viewer with no business
    /// editing anything carries nothing about editing at all.
    #[serde(skip_serializing_if = "is_false")]
    pub editable: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Who holds a slot, as much of it as this viewer is entitled to.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SlotOwner {
    /// The ping-preference chip, always present. **Shown even when the handle is not**, and
    /// deliberately: "no pings" is something the person chose to publish, and a player who can see
    /// it knows not to go looking for another way to reach them.
    pub ping: &'static str,
    /// Absent when this viewer may not know who this is — the owner said `no` and the viewer is not
    /// staff. Its absence is what distinguishes "withheld" from "never signed in", which is
    /// [`Contact::handle`] being null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact: Option<Contact>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Contact {
    /// `None` for somebody who holds a slot and has never signed in, which is the lobby-push case:
    /// the row exists because a push named their Discord id, and the stored username is a stand-in.
    /// Rendered as "never logged in" rather than as a raw snowflake.
    pub handle: Option<String>,
    /// `<@id>`, ready to paste into Discord. **Present even with no handle**, because a mention is
    /// built from the snowflake and pings whether or not that account has ever opened this site.
    pub mention: String,
}

/// A room's people, for the columns only its own participants see.
///
/// Loaded once per request and looked up per row rather than joined, because the roster is already
/// in hand and these are two small maps over the same room.
#[derive(Debug, Default)]
pub struct People {
    /// Discord handles by id. A placeholder — the id as text, written by `ensure_exists` for
    /// somebody who has never signed in — is **not** filtered out here; [`slot_rows`] turns it into
    /// an absent handle, so the two are told apart in one place.
    pub handles: std::collections::HashMap<i64, String>,
    pub preferences: std::collections::HashMap<i64, PingPreference>,
}

/// What this viewer is entitled to see of a room's own people.
///
/// A struct rather than a run of `bool` parameters because the three are read together and the
/// interesting mistakes are all confusions between them — `staff` where `participant` was meant
/// admits everybody holding a slot to a handle its owner withheld.
pub struct Viewer<'a> {
    /// Whoever is looking, when they are signed in. Only ever compared against a slot's owner, to
    /// decide whether they may edit their own annotations.
    pub id: Option<i64>,
    /// Staff, or somebody holding a slot in this room. Gates claim state.
    pub participant: bool,
    /// Staff specifically. Gets a handle **whatever the owner's ping preference says**, because
    /// running the room is the reason `no` still says "organizers and helpers may still ping you".
    pub staff: bool,
    /// `Some` only when the room's enhanced tracker is on **and** this viewer is a participant.
    /// `None` renders the page exactly as it was before the feature existed, which is what the
    /// toggle's default and every anonymous view rely on.
    pub people: Option<&'a People>,
}

impl Viewer<'_> {
    /// The viewer a public response is built for: entitled to nothing beyond the multiworld itself.
    ///
    /// Used by `summary.txt`, which is `public`-cacheable precisely because it is identical for
    /// every reader.
    pub fn outsider() -> Self {
        Self {
            id: None,
            participant: false,
            staff: false,
            people: None,
        }
    }

    /// Whether this viewer may change one slot's annotations: its holder, or the room's staff.
    ///
    /// **Staff may edit anybody's**, which is the room's own rule — an organizer needs to be able to
    /// correct or remove a note. What they may *not* touch is somebody's ping preference, which is
    /// why that lives on a different table with a writer that takes no actor.
    fn may_edit(&self, slot: &Slot) -> bool {
        self.people.is_some() && (self.staff || (self.id.is_some() && self.id == slot.owner_id))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Totals {
    pub slots: usize,
    pub checks_done: i64,
    pub checks_total: i64,
    pub goals: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SlotsView {
    #[serde(flatten)]
    pub freshness: Freshness,
    pub totals: Totals,
    pub slots: Vec<SlotRow>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HintRow {
    pub receiving_slot: i32,
    pub receiving_name: String,
    pub finding_slot: i32,
    pub finding_name: String,
    pub finding_game: String,
    pub item: String,
    pub location: String,
    /// `None` rather than an empty string: the reference renders "Vanilla" for a hint with no
    /// entrance, and that is a rendering choice the client should get to make.
    pub entrance: Option<String>,
    pub found: bool,
    pub classification: &'static str,
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HintsView {
    #[serde(flatten)]
    pub freshness: Freshness,
    pub hints: Vec<HintRow>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocationRow {
    pub id: i64,
    pub name: String,
    pub checked: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocationsView {
    #[serde(flatten)]
    pub freshness: Freshness,
    pub slot: i32,
    pub game: String,
    pub checked_count: usize,
    pub total: usize,
    pub locations: Vec<LocationRow>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ItemRow {
    /// The order it arrived in, 1-based. The reference's received table calls this "Last Order
    /// Received"; keeping the index lets the client render either a log or a tally.
    pub order: usize,
    pub item: String,
    pub classification: &'static str,
    pub from_slot: i32,
    pub from_name: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ItemsView {
    #[serde(flatten)]
    pub freshness: Freshness,
    pub slot: i32,
    pub items: Vec<ItemRow>,
}

/// Name lookups for one generation, with the fallback a missing entry gets.
pub struct Names<'a> {
    pub games: &'a BTreeMap<String, GameNames>,
}

impl Names<'_> {
    /// **A missing name is an id, never an error.** The cache is derived data that may not have
    /// been built yet, and a tracker that refused to render because one game was uncached would
    /// turn a cosmetic gap into an outage.
    ///
    /// The wording matches pahoa's, not the reference WebHost's, because this is the same
    /// multiworld a player sees named in the room's own chat — one vocabulary per room beats
    /// matching a page they are not reading.
    pub fn item(&self, game: &str, id: i64) -> String {
        self.games
            .get(game)
            .and_then(|names| names.items.get(&id))
            .cloned()
            .unwrap_or_else(|| format!("Unknown item (ID:{id})"))
    }

    pub fn location(&self, game: &str, id: i64) -> String {
        self.games
            .get(game)
            .and_then(|names| names.locations.get(&id))
            .cloned()
            .unwrap_or_else(|| format!("Unknown location (ID:{id})"))
    }
}

/// The multiworld slot table, or one slot's row, with the envelope the API sends.
pub fn slots(
    roster: &[Slot],
    live: &serde_json::Value,
    statics: &serde_json::Value,
    freshness: Freshness,
    scope: Option<i32>,
    now: DateTime<Utc>,
    viewer: &Viewer<'_>,
) -> SlotsView {
    let rows = slot_rows(roster, live, statics, scope, now, viewer);

    SlotsView {
        freshness,
        totals: Totals {
            slots: rows.len(),
            checks_done: rows.iter().map(|r| r.checks_done).sum(),
            checks_total: rows.iter().map(|r| r.checks_total).sum(),
            goals: rows.iter().filter(|r| r.status == "goal").count(),
        },
        slots: rows,
    }
}

/// The rows themselves, without the envelope.
///
/// Split out so the server-rendered page and the JSON view are **one implementation**: until Stage
/// C removes that page, two derivations of "how far along is this slot" would be free to disagree,
/// and a tracker whose table and whose API tell different stories is worse than either alone.
pub fn slot_rows(
    roster: &[Slot],
    live: &serde_json::Value,
    statics: &serde_json::Value,
    scope: Option<i32>,
    now: DateTime<Utc>,
    // **Decided by the caller from the session**, never here: this function is handed a roster
    // carrying `owner_id` and a note for every slot, and a client cannot prove it did not render
    // something it was given. See `Viewer`.
    viewer: &Viewer<'_>,
) -> Vec<SlotRow> {
    // **Puna's roster leads, not the document.** A spectator appears in neither per-player array --
    // pahoa mirrors the reference's `get_all_players()` split -- but it is still a slot somebody
    // claimed, and a tracker that silently omitted it would describe a different room from the one
    // on the room page.
    roster
        .iter()
        .filter(|slot| scope.is_none_or(|only| slot.slot_number == only))
        .map(|slot| {
            let n = i64::from(slot.slot_number);
            SlotRow {
                slot: slot.slot_number,
                // From Puna's row, not the document's `alias`, which is whatever the client last
                // called itself. The roster is what the room page shows.
                name: slot.player_name.clone(),
                game: slot.game.clone(),
                spectator: slot.kind == SlotKind::Spectator,
                checks_done: entry(live, "player_checks_done", n)
                    .and_then(|e| e.get("locations")?.as_array())
                    .map_or(0, |l| l.len() as i64),
                checks_total: entry(statics, "player_locations_total", n)
                    .and_then(|e| e.get("total_locations")?.as_i64())
                    .unwrap_or(0),
                status: client_status(
                    entry(live, "player_status", n).and_then(|e| e.get("status")?.as_i64()),
                ),
                last_activity_ms_ago: entry(live, "activity_timers", n)
                    .and_then(|e| e.get("time")?.as_str())
                    .and_then(|time| age_ms(time, now)),
                hints: entry(live, "hints", n)
                    .and_then(|e| e.get("hints")?.as_array())
                    .map_or(0, Vec::len),
                claimed: viewer.participant.then(|| slot.owner_id.is_some()),
                owner: viewer
                    .people
                    .and_then(|people| owner_of(slot, people, viewer.staff)),
                // Both are the room's own participants' business, so they travel with `people`
                // being present rather than being gated a second time.
                progression: viewer
                    .people
                    .and(Some(slot.progression))
                    .filter(|p| *p != ProgressionStatus::Unknown)
                    .map(ProgressionStatus::label),
                note: viewer.people.and_then(|_| slot.note.clone()),
                editable: viewer.may_edit(slot),
            }
        })
        .collect()
}

/// Who holds a slot, reduced to what this viewer may have.
///
/// **Two independent facts, and only one of them is ever withheld.** The ping preference is
/// something its owner published, so every participant gets it — a player who sees "no pings"
/// knows not to go looking for another route. The *handle* is the route, so `no` withholds it from
/// other players and never from staff, who are the ones the preference itself says may still ping.
///
/// The two `None`s below mean different things and must stay distinguishable, which is why one is a
/// missing `contact` and the other is a null `handle` inside a present one:
///
/// * **no `contact`** — this viewer may not know who holds the slot;
/// * **`contact` with no `handle`** — somebody holds it who has never signed in, so there is no
///   handle to show. The mention still works, because it is built from the snowflake.
///
/// An unclaimed slot has no owner at all and gets `None` from the outer function, which is a third
/// thing again and is fine to render the same way: there is nobody to name.
fn owner_of(slot: &Slot, people: &People, staff: bool) -> Option<SlotOwner> {
    let owner = slot.owner_id?;
    let preference = people.preferences.get(&owner).copied().unwrap_or_default();

    let entitled = staff || preference.shows_handle_to_players();
    Some(SlotOwner {
        ping: preference.label(),
        contact: entitled.then(|| Contact {
            // The placeholder is turned into an absent handle **here**, in the one place that knows
            // both, rather than by a client comparing a username against a snowflake. `ensure_exists`
            // writes the id as text for somebody a lobby push named, and rendering that would show a
            // raw Discord id to people with no use for one and read as a bug.
            handle: people
                .handles
                .get(&owner)
                .filter(|name| !puna_core::model::user::is_placeholder(name))
                .cloned(),
            mention: format!("<@{owner}>"),
        }),
    })
}

/// Every hint in the multiworld, or the ones that concern one slot.
///
/// **Deduplicated**, and that is not optional. A hint is filed under *both* the receiving and the
/// finding player, so walking every per-player entry sees each cross-player hint twice — the
/// reference collects them into a set for the same reason (`get_team_hints`). Without this the
/// multiworld table would double most of its rows and the per-slot hint counts would disagree with
/// the table they link to.
pub fn hints(
    roster: &[Slot],
    live: &serde_json::Value,
    names: &Names<'_>,
    freshness: Freshness,
    scope: Option<i32>,
) -> HintsView {
    let by_slot: BTreeMap<i32, &Slot> = roster.iter().map(|s| (s.slot_number, s)).collect();
    let describe = |slot: i32| -> (String, String) {
        by_slot.get(&slot).map_or_else(
            // A slot the roster does not know: name it honestly rather than inventing one. It
            // means the document and Puna's roster disagree, which is worth being able to see.
            || (format!("slot {slot}"), String::new()),
            |s| (s.player_name.clone(), s.game.clone()),
        )
    };

    let mut seen: HashSet<(i64, i64, i64, i64)> = HashSet::new();
    let mut rows = Vec::new();

    for entry in live
        .get("hints")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(list) = entry.get("hints").and_then(serde_json::Value::as_array) else {
            continue;
        };

        for hint in list {
            // `Hint(receiving_player, finding_player, location, item, found, entrance, item_flags,
            // status)` -- NetUtils.py. Positional, so the order is transcribed rather than guessed.
            let Some(fields) = hint.as_array() else {
                continue;
            };
            let at = |i: usize| fields.get(i).and_then(serde_json::Value::as_i64);
            let (Some(receiving), Some(finding), Some(location), Some(item)) =
                (at(0), at(1), at(2), at(3))
            else {
                continue;
            };

            if !seen.insert((receiving, finding, location, item)) {
                continue;
            }

            let receiving_slot = receiving as i32;
            let finding_slot = finding as i32;

            // A slot's view keeps the hints it is either end of -- what it will receive, and what
            // it is holding for somebody else. Both are "about" that player.
            if let Some(only) = scope
                && receiving_slot != only
                && finding_slot != only
            {
                continue;
            }

            let (receiving_name, receiving_game) = describe(receiving_slot);
            let (finding_name, finding_game) = describe(finding_slot);

            rows.push(HintRow {
                receiving_slot,
                receiving_name,
                finding_slot,
                finding_name,
                // The item is the RECEIVER's, the location is the FINDER's. See the module docs.
                item: names.item(&receiving_game, item),
                location: names.location(&finding_game, location),
                finding_game,
                entrance: fields
                    .get(5)
                    .and_then(serde_json::Value::as_str)
                    .filter(|e| !e.is_empty())
                    .map(str::to_string),
                found: fields
                    .get(4)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                classification: classify(at(6).unwrap_or(0)),
                status: hint_status(at(7)),
            });
        }
    }

    HintsView {
        freshness,
        hints: rows,
    }
}

/// One slot's locations, checked and unchecked.
///
/// `all` is the slot's full location list out of the name cache; the document supplies which of
/// them are done. **The item behind an unchecked location is not available here and never was** —
/// the cache stores location ids only, so this function could not leak it if it tried.
pub fn locations(
    slot: &Slot,
    all: &[i64],
    live: &serde_json::Value,
    names: &Names<'_>,
    freshness: Freshness,
) -> LocationsView {
    let checked: HashSet<i64> = entry(live, "player_checks_done", i64::from(slot.slot_number))
        .and_then(|e| e.get("locations")?.as_array())
        .map(|ids| ids.iter().filter_map(serde_json::Value::as_i64).collect())
        .unwrap_or_default();

    let rows: Vec<LocationRow> = all
        .iter()
        .map(|id| LocationRow {
            id: *id,
            name: names.location(&slot.game, *id),
            checked: checked.contains(id),
        })
        .collect();

    LocationsView {
        freshness,
        slot: slot.slot_number,
        game: slot.game.clone(),
        checked_count: rows.iter().filter(|r| r.checked).count(),
        total: rows.len(),
        locations: rows,
    }
}

/// One slot's received items, in the order they arrived.
pub fn items(
    slot: &Slot,
    roster: &[Slot],
    live: &serde_json::Value,
    names: &Names<'_>,
    freshness: Freshness,
) -> ItemsView {
    let by_slot: BTreeMap<i32, &Slot> = roster.iter().map(|s| (s.slot_number, s)).collect();

    let rows: Vec<ItemRow> = entry(live, "player_items_received", i64::from(slot.slot_number))
        .and_then(|e| e.get("items")?.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .filter_map(|(index, received)| {
            // `NetworkItem(item, location, player, flags)`, where `player` is the FINDER.
            let fields = received.as_array()?;
            let at = |i: usize| fields.get(i).and_then(serde_json::Value::as_i64);
            let (item, location, from) = (at(0)?, at(1)?, at(2)?);
            let from_slot = from as i32;

            let (from_name, from_game) = by_slot.get(&from_slot).map_or_else(
                || (format!("slot {from_slot}"), String::new()),
                |s| (s.player_name.clone(), s.game.clone()),
            );

            Some(ItemRow {
                order: index + 1,
                // The item is resolved in THIS slot's game -- it is this player's item, placed in
                // somebody else's world -- while the location is resolved in the finder's.
                item: names.item(&slot.game, item),
                classification: classify(at(3).unwrap_or(0)),
                from_slot,
                from_name,
                location: names.location(&from_game, location),
            })
        })
        .collect();

    ItemsView {
        freshness,
        slot: slot.slot_number,
        items: rows,
    }
}

/// The entry for one slot in one of a document's per-player arrays.
pub fn entry<'a>(
    document: &'a serde_json::Value,
    key: &str,
    slot_number: i64,
) -> Option<&'a serde_json::Value> {
    document
        .get(key)?
        .as_array()?
        .iter()
        .find(|entry| entry.get("player").and_then(serde_json::Value::as_i64) == Some(slot_number))
}

/// **One line of plain text, for a chat bot to paste.**
///
/// `RaysSMS: 50.0% | RaysLM: 25.4% | RaysSM64: goal`, terminated by a single newline.
///
/// The audience is what shapes every decision here: a Twitch bot answering `!progress` gets one
/// message to say everything, and whoever reads it is watching a stream rather than studying a
/// table. So it is one line, and the line carries the two facts a viewer asks about — how far along
/// each world is, and whether it is finished.
///
/// **`goal` REPLACES the percentage only when the world is also complete.** A room without
/// auto-release leaves a goaled slot with locations still unchecked, and saying `goal` there would
/// claim a world was done when a third of it is still out; saying `66.7%` alone would hide that the
/// player has finished. Both facts, so `66.7% (goal)`.
///
/// **The percentage is truncated, not rounded**, which is the one rounding decision that matters
/// here: a slot at 2159 of 2160 rounds to `100.0%`, and a progress line reading 100% on an
/// unfinished world is exactly the wrong answer to the question being asked. Truncating can only
/// ever under-report, and it reaches `100.0%` when the world genuinely is.
///
/// **Spectators are omitted.** They own no locations, so every arithmetic here is 0/0 for them and
/// any rendering of that is a claim about progress they cannot make.
///
/// **The order is the roster's, which is slot order** — `slot::list` sorts on `slot_number`, and
/// nothing here reorders. That is worth keeping rather than sorting by progress: this line is read
/// repeatedly by the same people, so a world stays in the same position from one `!progress` to the
/// next and a viewer can find theirs without reading the whole thing. Sorting by completion would
/// reshuffle it every time somebody found a check.
pub fn summary(rows: &[SlotRow]) -> String {
    let mut parts = Vec::new();

    for row in rows.iter().filter(|row| !row.spectator) {
        let goaled = row.status == "goal";
        // A total of zero is "not known" rather than "nothing to do": it is what a slot missing
        // from the static document reads as. A percentage of an unknown total is not a number, so
        // it does not get one.
        let known = row.checks_total > 0;

        let state = if goaled && (!known || row.checks_done >= row.checks_total) {
            "goal".to_string()
        } else if goaled {
            format!("{} (goal)", percent(row.checks_done, row.checks_total))
        } else if known {
            percent(row.checks_done, row.checks_total)
        } else {
            "?".to_string()
        };

        parts.push(format!("{}: {}", one_line(&row.name), state));
    }

    // A room with nothing to report still has to say something: a bot posting an empty message
    // reads as the command being broken. Unmistakable for a slot, which always carries a colon.
    if parts.is_empty() {
        return "no slots\n".to_string();
    }

    parts.join(" | ") + "\n"
}

/// Truncated to a tenth of a percent. See `summary` for why truncated rather than rounded.
///
/// Clamped at both ends because `checks_done` and `checks_total` come from two *different* upstream
/// documents, so nothing structurally stops the first exceeding the second while the pair is
/// momentarily out of step.
fn percent(done: i64, total: i64) -> String {
    let tenths = (done.saturating_mul(1000) / total.max(1)).clamp(0, 1000);
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// **The whole document is one line, so a name cannot be allowed to add another.**
///
/// A player name is untrusted text out of an uploaded seed, and this is the one response where a
/// newline in it is not a cosmetic problem: a bot that reads a line gets a truncated answer, and
/// the records after the break simply do not exist as far as it is concerned. Every control
/// character goes, rather than newlines alone — a carriage return would overwrite the line in a
/// terminal and an escape would do rather more than that.
fn one_line(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).collect()
}

/// Archipelago's `ClientStatus`, in words.
///
/// The numbers are the protocol's and are sparse (0, 5, 10, 20, 30) because the reference leaves
/// room between them. An unknown value renders as "unknown" rather than as itself: a number in this
/// column would mean nothing to the person reading it.
pub fn client_status(status: Option<i64>) -> &'static str {
    match status {
        Some(5) => "connected",
        Some(10) => "ready",
        Some(20) => "playing",
        Some(30) => "goal",
        _ => "unknown",
    }
}

/// `NetUtils.HintStatus`, in words. Unparenthesized, unlike pahoa's chat label, because this is a
/// table column rather than a phrase inside a sentence.
fn hint_status(status: Option<i64>) -> &'static str {
    match status {
        Some(0) => "unspecified",
        Some(10) => "no priority",
        Some(20) => "avoid",
        Some(30) => "priority",
        Some(40) => "found",
        _ => "unknown",
    }
}

/// The low three bits of `ItemClassification`, which is all that is transmitted
/// (`BaseClasses.py:1587-1589`).
///
/// Checked in the reference's own order (`NetUtils._handle_item_name`): progression first, then
/// useful, then trap — an item can carry more than one bit and the first match is what it is
/// called.
fn classify(flags: i64) -> &'static str {
    match flags {
        f if f & 0b001 != 0 => "progression",
        f if f & 0b010 != 0 => "useful",
        f if f & 0b100 != 0 => "trap",
        _ => "filler",
    }
}

/// An RFC 1123 timestamp as an age in milliseconds, or `None` for "never" and for anything that
/// does not parse.
///
/// Clamped at zero: a room whose clock is ahead would otherwise produce a negative age, which
/// renders as a time in the future and reads as a bug in Puna.
fn age_ms(time: &str, now: DateTime<Utc>) -> Option<i64> {
    let at = DateTime::parse_from_rfc2822(time).ok()?;
    Some(
        now.signed_duration_since(at.with_timezone(&Utc))
            .num_milliseconds()
            .max(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use puna_core::ids::{RoomId, TrackerId};

    fn slot(number: i32, name: &str, game: &str, kind: SlotKind, owner: Option<i64>) -> Slot {
        Slot {
            room_id: RoomId::new(),
            slot_number: number,
            player_name: name.into(),
            game: game.into(),
            kind,
            password: Some("a-secret".into()),
            owner_id: owner,
            claim_token: Some("a-claim-token".into()),
            claimed_at: None,
            tracker_id: TrackerId::new(),
            locked_at: None,
            locked_by: None,
            progression: puna_core::model::annotation::ProgressionStatus::Unknown,
            note: None,
            annotated_at: None,
            annotated_by: None,
        }
    }

    fn roster() -> Vec<Slot> {
        vec![
            slot(1, "Troy", "A Link to the Past", SlotKind::Player, Some(7)),
            slot(2, "Alice", "Timespinner", SlotKind::Player, None),
            slot(4, "Watcher", "Archipelago", SlotKind::Spectator, None),
        ]
    }

    /// **Ids 10 and 200 exist in BOTH games with different names.** That is the whole point: a
    /// resolution done in the wrong game returns a real name of a real thing, so the failure has no
    /// symptom unless a test is built to have one.
    fn game_names() -> BTreeMap<String, GameNames> {
        let mut games = BTreeMap::new();
        games.insert(
            "A Link to the Past".to_string(),
            GameNames {
                items: [(10, "Progressive Sword"), (20, "Bow")]
                    .into_iter()
                    .map(|(i, n)| (i, n.to_string()))
                    .collect(),
                locations: [
                    (100, "Link's House"),
                    (101, "Sanctuary"),
                    (102, "Desert Palace - Big Chest"),
                    (200, "Eastern Palace - Big Chest"),
                ]
                .into_iter()
                .map(|(i, n)| (i, n.to_string()))
                .collect(),
            },
        );
        games.insert(
            "Timespinner".to_string(),
            GameNames {
                items: [(10, "Talaria Attachment")]
                    .into_iter()
                    .map(|(i, n)| (i, n.to_string()))
                    .collect(),
                locations: [(100, "Lake Serene"), (200, "Lake Desolation")]
                    .into_iter()
                    .map(|(i, n)| (i, n.to_string()))
                    .collect(),
            },
        );
        games
    }

    /// One hint: Troy (slot 1, ALTTP) will receive item 10, and it is at location 200 in Alice's
    /// Timespinner world. Filed under BOTH players, as the reference files it.
    fn live() -> serde_json::Value {
        serde_json::json!({
            "player_checks_done": [
                {"team": 0, "player": 1, "locations": [100, 101]},
                {"team": 0, "player": 2, "locations": [200]},
            ],
            "player_items_received": [
                {"team": 0, "player": 1, "items": [[10, 200, 2, 1], [20, 100, 2, 0]]},
            ],
            "hints": [
                {"team": 0, "player": 1, "hints": [[1, 2, 200, 10, false, "", 1, 30]]},
                {"team": 0, "player": 2, "hints": [[1, 2, 200, 10, false, "", 1, 30]]},
            ],
            "activity_timers": [
                {"team": 0, "player": 1, "time": "Mon, 17 Aug 2026 18:00:00 GMT"},
                {"team": 0, "player": 2, "time": null},
            ],
            "player_status": [
                {"team": 0, "player": 1, "status": 20},
                {"team": 0, "player": 2, "status": 30},
            ],
        })
    }

    fn statics() -> serde_json::Value {
        serde_json::json!({
            "player_locations_total": [
                {"team": 0, "player": 1, "total_locations": 3},
                {"team": 0, "player": 2, "total_locations": 2},
            ],
        })
    }

    fn fresh() -> Freshness {
        Freshness {
            as_of: "2026-08-19T18:00:00+00:00".into(),
            stale: false,
            next_poll_ms: 60_000,
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc2822("Mon, 17 Aug 2026 19:00:00 GMT")
            .expect("a fixed instant")
            .with_timezone(&Utc)
    }

    fn names_of(games: &BTreeMap<String, GameNames>) -> Names<'_> {
        Names { games }
    }

    /// A viewer who is one of the room's people but not staff, and whose room has the enhanced
    /// tracker off. The tier most of these tests are about.
    fn participant() -> Viewer<'static> {
        Viewer {
            id: None,
            participant: true,
            staff: false,
            people: None,
        }
    }

    /// Troy holds slot 1 and has said how he wants to be pinged; Ghost holds slot 2 and has never
    /// signed in, which is the lobby-push case.
    fn people(troy: PingPreference) -> People {
        People {
            handles: [
                (7, "troyhandle".to_string()),
                // What `ensure_exists` writes for somebody a lobby push named: the snowflake as
                // text. Rendering it would show a raw Discord id to people with no use for one.
                (8, puna_core::model::user::placeholder_username(8)),
            ]
            .into_iter()
            .collect(),
            preferences: [(7, troy)].into_iter().collect(),
        }
    }

    fn annotated_roster() -> Vec<Slot> {
        let mut roster = roster();
        roster[0].progression = ProgressionStatus::Bk;
        roster[0].note = Some("ping me before 9pm".into());
        roster[1].owner_id = Some(8);
        roster
    }

    /// **The room's own people get the annotations; nobody else learns the feature is on.**
    ///
    /// Three viewers, and the first two must produce byte-identical rows to a room with the toggle
    /// off — which is the promise the option's own hint makes and the reason its default is safe.
    #[test]
    fn the_annotation_columns_reach_participants_and_nobody_else() {
        let people = people(PingPreference::Yes);
        let of = |viewer: &Viewer<'_>| {
            slot_rows(
                &annotated_roster(),
                &live(),
                &statics(),
                None,
                now(),
                viewer,
            )
        };

        for (who, viewer) in [
            ("an anonymous viewer", Viewer::outsider()),
            (
                "a participant of a room with the feature off",
                participant(),
            ),
        ] {
            for row in of(&viewer) {
                assert_eq!(
                    row.owner, None,
                    "{who} was told who holds slot {}",
                    row.slot
                );
                assert_eq!(row.progression, None, "{who} got a progression chip");
                assert_eq!(row.note, None, "{who} got somebody's note");
            }
        }

        let rows = of(&Viewer {
            id: None,
            participant: true,
            staff: false,
            people: Some(&people),
        });
        assert_eq!(rows[0].progression, Some("BK"));
        assert_eq!(rows[0].note.as_deref(), Some("ping me before 9pm"));
        assert_eq!(
            rows[0].owner.as_ref().and_then(|o| o.contact.as_ref()),
            Some(&Contact {
                handle: Some("troyhandle".into()),
                mention: "<@7>".into(),
            })
        );

        // **A stand-in username is an absent handle, not a rendered snowflake**, and the mention
        // still works — which is the whole reason the two are separate fields.
        let ghost = rows[1].owner.as_ref().expect("slot 2 has an owner");
        let contact = ghost
            .contact
            .as_ref()
            .expect("a participant may contact them");
        assert_eq!(contact.handle, None, "a raw Discord id reached the page");
        assert_eq!(contact.mention, "<@8>");

        // An unclaimed slot has nobody to name.
        assert_eq!(rows[2].owner, None);
    }

    /// **Who gets the edit control: the slot's own holder, and the room's staff.**
    ///
    /// Decided here rather than by the client comparing ids, which would mean sending every viewer
    /// their own id and every row's owner id and trusting the arithmetic. The route re-checks
    /// regardless — this only decides whether a control is offered — but a control offered to the
    /// wrong person is a refusal somebody has to be told about, which is its own small failure.
    ///
    /// **Staff may edit anybody's**, which is the room's rule: an organizer needs to be able to
    /// correct or remove a note. What they may not touch is a ping preference, which lives on a
    /// different table with a writer that takes no actor at all.
    #[test]
    fn the_edit_control_reaches_a_slots_holder_and_the_rooms_staff() {
        let people = people(PingPreference::Yes);
        let rows = |id: Option<i64>, staff: bool| {
            slot_rows(
                &annotated_roster(),
                &live(),
                &statics(),
                None,
                now(),
                &Viewer {
                    id,
                    participant: true,
                    staff,
                    people: Some(&people),
                },
            )
        };

        // Troy holds slot 1 and nothing else. Ghost (8) holds slot 2; slot 4 is unclaimed.
        let troy = rows(Some(7), false);
        assert!(troy[0].editable, "a player cannot edit their own slot");
        assert!(!troy[1].editable, "a player may edit somebody else's slot");
        assert!(
            !troy[2].editable,
            "an unclaimed slot offered an edit control"
        );

        // Staff get every row, including the unclaimed one -- which is harmless and is what
        // "organizers may change anything a player can" means when nobody holds it yet.
        let organizer = rows(Some(99), true);
        assert!(organizer.iter().all(|r| r.editable));

        // A signed-out viewer of a room with the feature on is not a participant at all, and a
        // participant of a room with it OFF has no `people` -- neither gets a control.
        assert!(rows(None, false).iter().all(|r| !r.editable));
        for row in slot_rows(
            &annotated_roster(),
            &live(),
            &statics(),
            None,
            now(),
            &Viewer {
                id: Some(7),
                participant: true,
                staff: true,
                people: None,
            },
        ) {
            assert!(
                !row.editable,
                "the room has the feature off and still offered an edit control"
            );
        }
    }

    /// **`no` withholds the handle from other players and never from staff**, which is exactly what
    /// that option promises: "organizers and helpers may still choose to ping you".
    ///
    /// The chip survives either way, and that is the half worth stating: "no pings" is something its
    /// owner published, and a player who sees it stops looking for another way to reach them. Hiding
    /// the chip along with the handle would make a slot whose owner said no indistinguishable from
    /// one nobody holds, and somebody would go asking in chat.
    #[test]
    fn saying_no_hides_a_handle_from_players_and_never_from_staff() {
        let refused = people(PingPreference::No);
        let rows = |staff: bool| {
            slot_rows(
                &annotated_roster(),
                &live(),
                &statics(),
                None,
                now(),
                &Viewer {
                    id: None,
                    participant: true,
                    staff,
                    people: Some(&refused),
                },
            )
        };

        let player_view = rows(false)[0].owner.clone().expect("an owner");
        assert_eq!(player_view.ping, "no pings", "the chip is not the secret");
        assert_eq!(
            player_view.contact, None,
            "a player was handed a handle its owner withheld"
        );

        let staff_view = rows(true)[0].owner.clone().expect("an owner");
        assert_eq!(staff_view.ping, "no pings");
        assert_eq!(
            staff_view.contact.and_then(|c| c.handle).as_deref(),
            Some("troyhandle"),
            "staff cannot reach somebody they are told they may still ping"
        );

        // Every other value is a form of yes, so a player gets the handle under all of them.
        for preference in PingPreference::ALL {
            if preference == PingPreference::No {
                continue;
            }
            let permissive = people(preference);
            let rows = slot_rows(
                &annotated_roster(),
                &live(),
                &statics(),
                None,
                now(),
                &Viewer {
                    id: None,
                    participant: true,
                    staff: false,
                    people: Some(&permissive),
                },
            );
            assert!(
                rows[0]
                    .owner
                    .as_ref()
                    .and_then(|o| o.contact.as_ref())
                    .is_some(),
                "{:?} withheld a handle, and only `no` may",
                preference.as_sql()
            );
        }
    }

    /// **Who has signed up is not part of a tracker link.**
    ///
    /// The tracker id is independent of the room id *precisely* so it can be handed to an audience
    /// the organizers did not choose — a stream chat, a spectator — so it is the widest-shared
    /// surface Puna has. Claim state is the one column on it that says nothing about the multiworld
    /// and everything about Puna's own sign-up sheet, so it goes to the room's own people and
    /// nobody else: the same tier the roster applies to who holds a slot.
    ///
    /// **Absent, not `false`.** Serializing `false` for a viewer who may not know would leave the
    /// client one falsy check away from tagging every slot `unclaimed` for exactly the audience the
    /// server just declined to tell — so the withheld case is asserted on the wire and not only on
    /// the struct.
    #[test]
    fn an_outsider_is_not_told_which_slots_are_unclaimed() {
        let hidden = slots(
            &roster(),
            &live(),
            &statics(),
            fresh(),
            None,
            now(),
            &Viewer::outsider(),
        );
        for row in &hidden.slots {
            assert_eq!(
                row.claimed, None,
                "slot {} leaked its claim state",
                row.slot
            );
        }

        let body = serde_json::to_string(&hidden).expect("serializes");
        assert!(
            !body.contains("claimed"),
            "the field reaches the wire at all, so a falsy check reads it as unclaimed: {body}"
        );

        // And the room's own people still get it, or the gate has eaten the feature rather than
        // narrowing it.
        let shown = slots(
            &roster(),
            &live(),
            &statics(),
            fresh(),
            None,
            now(),
            &participant(),
        );
        assert!(
            shown.slots.iter().all(|r| r.claimed.is_some()),
            "a participant is no longer told who has signed up"
        );
        assert!(
            serde_json::to_string(&shown)
                .expect("serializes")
                .contains("claimed"),
        );
    }

    #[test]
    fn the_slot_table_leads_with_punas_roster() {
        let view = slots(
            &roster(),
            &live(),
            &statics(),
            fresh(),
            None,
            now(),
            &participant(),
        );

        assert_eq!(
            view.slots.len(),
            3,
            "the spectator is a slot somebody claimed"
        );
        assert_eq!(view.totals.goals, 1, "Alice has goaled");
        assert_eq!(view.totals.checks_done, 3);

        let troy = &view.slots[0];
        assert_eq!(troy.checks_done, 2);
        assert_eq!(troy.checks_total, 3);
        assert_eq!(troy.status, "playing");
        assert_eq!(troy.hints, 1);
        assert_eq!(troy.claimed, Some(true));
        assert_eq!(troy.last_activity_ms_ago, Some(3_600_000), "one hour");

        // A spectator appears in neither per-player array, so it must not read as a player who has
        // done nothing -- and `null` activity is *never*, not 1970.
        let watcher = &view.slots[2];
        assert!(watcher.spectator);
        assert_eq!(watcher.checks_total, 0);
        assert_eq!(view.slots[1].last_activity_ms_ago, None);
    }

    /// **The failure with no symptom.** A hint's item belongs to the RECEIVER's game and its
    /// location to the FINDER's. Swap them and both lookups still succeed, returning real names of
    /// real things -- so this asserts the exact strings, not merely that something resolved.
    #[test]
    fn a_hints_item_and_location_resolve_in_different_games() {
        let games = game_names();
        let view = hints(&roster(), &live(), &names_of(&games), fresh(), None);

        assert_eq!(view.hints.len(), 1);
        let hint = &view.hints[0];

        assert_eq!(hint.receiving_slot, 1);
        assert_eq!(hint.receiving_name, "Troy");
        assert_eq!(hint.finding_slot, 2);
        assert_eq!(hint.finding_name, "Alice");
        assert_eq!(hint.finding_game, "Timespinner");

        // Item 10 in Troy's game, NOT "Talaria Attachment", which is what id 10 is in Alice's.
        assert_eq!(hint.item, "Progressive Sword");
        // Location 200 in Alice's game, NOT "Eastern Palace - Big Chest", which is what id 200 is
        // in Troy's.
        assert_eq!(hint.location, "Lake Desolation");

        assert_eq!(hint.classification, "progression");
        assert_eq!(hint.status, "priority");
        assert!(!hint.found);
        assert_eq!(hint.entrance, None, "an empty entrance is None, not \"\"");
    }

    /// A hint is filed under both players, so walking every entry sees it twice. The reference
    /// collects hints into a set for this reason; without it the multiworld table doubles its rows.
    #[test]
    fn a_hint_filed_under_both_players_appears_once() {
        let games = game_names();
        let view = hints(&roster(), &live(), &names_of(&games), fresh(), None);
        assert_eq!(
            view.hints.len(),
            1,
            "the cross-player hint was counted twice"
        );
    }

    /// A slot's hint table keeps what it will receive AND what it is holding for someone else --
    /// both are about that player -- and nothing else.
    #[test]
    fn a_slots_hints_are_the_ones_it_is_either_end_of() {
        let games = game_names();
        let mut document = live();
        // A hint between two other players, which slot 1 must not see.
        document["hints"] = serde_json::json!([
            {"team": 0, "player": 1, "hints": [[1, 2, 200, 10, false, "", 1, 30]]},
            {"team": 0, "player": 2, "hints": [[2, 2, 100, 10, true, "Cave", 0, 40]]},
        ]);

        let receiver = hints(&roster(), &document, &names_of(&games), fresh(), Some(1));
        assert_eq!(receiver.hints.len(), 1);
        assert_eq!(receiver.hints[0].receiving_slot, 1);

        let finder = hints(&roster(), &document, &names_of(&games), fresh(), Some(2));
        assert_eq!(finder.hints.len(), 2, "slot 2 finds one and receives one");

        // And the rendered view of slot 1 names nobody it should not -- the assertion that holds
        // however the filtering is implemented.
        let rendered = serde_json::to_string(&receiver).expect("serializes");
        assert!(
            rendered.contains("Alice"),
            "the finder is named, which is the point of a hint"
        );
        assert!(
            !rendered.contains("Cave"),
            "another pair's hint leaked: {rendered}"
        );
    }

    #[test]
    fn locations_show_checked_and_unchecked_from_the_slots_own_game() {
        let games = game_names();
        let roster = roster();
        let view = locations(
            &roster[0],
            &[100, 101, 102],
            &live(),
            &names_of(&games),
            fresh(),
        );

        assert_eq!(view.total, 3);
        assert_eq!(view.checked_count, 2);
        assert_eq!(view.game, "A Link to the Past");

        assert_eq!(view.locations[0].name, "Link's House");
        assert!(view.locations[0].checked);
        assert_eq!(view.locations[2].name, "Desert Palace - Big Chest");
        assert!(
            !view.locations[2].checked,
            "the unchecked location is the thing the reference cannot show"
        );
    }

    /// A received item is the RECEIVER's, placed in the FINDER's world -- so the two halves of one
    /// row resolve in two different games, the same trap as a hint.
    #[test]
    fn a_received_items_name_and_location_resolve_in_different_games() {
        let games = game_names();
        let roster = roster();
        let view = items(&roster[0], &roster, &live(), &names_of(&games), fresh());

        assert_eq!(view.items.len(), 2);
        let first = &view.items[0];
        assert_eq!(first.order, 1);
        assert_eq!(
            first.item, "Progressive Sword",
            "resolved in Troy's own game"
        );
        assert_eq!(
            first.location, "Lake Desolation",
            "resolved in Alice's game"
        );
        assert_eq!(first.from_slot, 2);
        assert_eq!(first.from_name, "Alice");
        assert_eq!(first.classification, "progression");
        assert_eq!(view.items[1].classification, "filler");
    }

    /// A generation with no cached names renders ids, not an error. The cache is derived data that
    /// may not have been built, and a tracker that refused to render over it would turn a cosmetic
    /// gap into an outage.
    #[test]
    fn missing_names_degrade_to_ids() {
        let empty = BTreeMap::new();
        let roster = roster();
        let view = items(&roster[0], &roster, &live(), &names_of(&empty), fresh());

        assert_eq!(view.items[0].item, "Unknown item (ID:10)");
        assert_eq!(view.items[0].location, "Unknown location (ID:200)");
    }

    /// **The property the whole tier exists for**, now asserted on the JSON rather than the markup,
    /// because Stage C moves the tables out of the template the old test covered.
    #[test]
    fn no_view_carries_a_credential_an_address_or_an_id() {
        let games = game_names();
        let roster = roster();
        let rendered = [
            serde_json::to_string(&slots(
                &roster,
                &live(),
                &statics(),
                fresh(),
                None,
                now(),
                &participant(),
            )),
            serde_json::to_string(&hints(&roster, &live(), &names_of(&games), fresh(), None)),
            serde_json::to_string(&locations(
                &roster[0],
                &[100, 101, 102],
                &live(),
                &names_of(&games),
                fresh(),
            )),
            serde_json::to_string(&items(
                &roster[0],
                &roster,
                &live(),
                &names_of(&games),
                fresh(),
            )),
        ]
        .map(|r| r.expect("serializes"));

        // The plain-text summary is a fifth view of the same data and is the one served with no
        // session at all, so it is held to the same rule as the four beside it.
        let rendered: Vec<String> = rendered
            .into_iter()
            .chain(std::iter::once(summary(&slot_rows(
                &roster,
                &live(),
                &statics(),
                None,
                now(),
                &Viewer::outsider(),
            ))))
            .collect();

        for body in &rendered {
            assert!(!body.contains("a-secret"), "a slot password: {body}");
            assert!(!body.contains("a-claim-token"), "a claim token: {body}");
            assert!(!body.contains("/room/"), "a link back to the room: {body}");
            assert!(!body.contains("mw."), "the advertised hostname: {body}");

            // No slot tracker id: sending one would hand every viewer of the room tracker every
            // player's independent link, collapsing two deliberately separate capabilities.
            for slot in &roster {
                assert!(
                    !body.contains(&slot.tracker_id.to_string()),
                    "a slot's tracker id: {body}"
                );
                assert!(
                    !body.contains(&slot.room_id.to_string()),
                    "the room id: {body}"
                );
            }

            // Nothing port-shaped, in the 40000-49999 range the rooms use.
            assert!(
                !body
                    .split(|c: char| !c.is_ascii_digit())
                    .filter_map(|run| run.parse::<u32>().ok())
                    .any(|n| (40000..=49999).contains(&n)),
                "something that reads as an address: {body}"
            );
        }
    }

    /// A row, for the cases the shared fixture cannot reach: a finished world, an unknown total.
    fn row(name: &str, done: i64, total: i64, status: &'static str) -> SlotRow {
        SlotRow {
            slot: 1,
            name: name.into(),
            game: "A Link to the Past".into(),
            spectator: false,
            checks_done: done,
            checks_total: total,
            status,
            last_activity_ms_ago: None,
            hints: 0,
            claimed: Some(false),
            editable: false,
            owner: None,
            progression: None,
            note: None,
        }
    }

    /// The shape a bot pastes, end to end from the shared fixture.
    ///
    /// Troy is 2 of 3 and playing; Alice is 1 of 2 and has goaled, which is the without-auto-release
    /// case; the spectator is absent. Written as one exact string rather than as assertions about
    /// its parts, because the **whole document** is the contract — a stray space or a lost newline
    /// is a broken `!progress` command and nothing about the parts would say so.
    #[test]
    fn the_summary_is_one_line_a_bot_can_paste() {
        let text = summary(&slot_rows(
            &roster(),
            &live(),
            &statics(),
            None,
            now(),
            &Viewer::outsider(),
        ));

        assert_eq!(text, "Troy: 66.6% | Alice: 50.0% (goal)\n");
    }

    /// **`goal` alone means finished; `(goal)` means finished playing with checks left behind.**
    ///
    /// The second is a room without auto-release, and collapsing the two would either claim a world
    /// was complete when a third of it is unchecked, or hide that the player is done.
    #[test]
    fn a_goal_replaces_the_percentage_only_when_the_world_is_complete() {
        assert_eq!(
            summary(&[row("Done", 216, 216, "goal")]),
            "Done: goal\n",
            "a finished world says so and nothing else"
        );
        assert_eq!(
            summary(&[row("Left", 108, 216, "goal")]),
            "Left: 50.0% (goal)\n",
            "goaled with checks outstanding needs both facts"
        );
        assert_eq!(
            summary(&[row("Full", 216, 216, "playing")]),
            "Full: 100.0%\n",
            "every check found is not the same as having goaled"
        );
    }

    /// **Truncated, never rounded**, which is the one rounding decision that matters: a progress
    /// line reading `100.0%` on a world with a check outstanding is exactly the wrong answer to the
    /// question a viewer is asking.
    #[test]
    fn a_percentage_never_rounds_up_to_a_hundred() {
        assert_eq!(summary(&[row("N", 2159, 2160, "playing")]), "N: 99.9%\n");
        assert_eq!(summary(&[row("N", 2160, 2160, "playing")]), "N: 100.0%\n");
        assert_eq!(summary(&[row("N", 0, 2160, "playing")]), "N: 0.0%\n");

        // The two counts come from different upstream documents, so nothing structurally stops the
        // first exceeding the second while the pair is out of step.
        assert_eq!(summary(&[row("N", 2161, 2160, "playing")]), "N: 100.0%\n");
    }

    /// A total of zero is **not known**, not "nothing to do", so it gets no percentage — and a
    /// goaled slot still reports the one thing that is known about it regardless.
    #[test]
    fn an_unknown_total_is_not_reported_as_a_percentage() {
        assert_eq!(summary(&[row("N", 0, 0, "playing")]), "N: ?\n");
        assert_eq!(summary(&[row("N", 0, 0, "goal")]), "N: goal\n");
    }

    /// **The document is one line, so a name may not add another.** A player name is untrusted text
    /// out of an uploaded seed, and here a newline is not cosmetic: a bot reading a line gets a
    /// truncated answer, and everything after the break does not exist as far as it is concerned.
    #[test]
    fn a_name_cannot_break_the_one_line_document() {
        let text = summary(&[
            row("Ray\r\nAdmin: 100.0%", 1, 4, "playing"),
            row("B", 1, 4, "x"),
        ]);

        assert_eq!(text, "RayAdmin: 100.0%: 25.0% | B: 25.0%\n");
        assert_eq!(text.lines().count(), 1, "one line, always");
    }

    /// A spectator owns no locations, so every number here is 0/0 for one and any rendering of that
    /// is a claim about progress it cannot make. When that leaves nothing, the document still says
    /// something: a bot posting an empty message reads as the command being broken.
    #[test]
    fn a_spectator_is_not_progress_and_an_empty_room_still_answers() {
        let mut watcher = row("Watcher", 0, 0, "connected");
        watcher.spectator = true;

        assert_eq!(summary(&[watcher.clone()]), "no slots\n");
        assert_eq!(summary(&[]), "no slots\n");
        assert_eq!(
            summary(&[row("Troy", 1, 4, "playing"), watcher]),
            "Troy: 25.0%\n"
        );
    }

    /// A room whose clock is ahead would otherwise produce a negative age, which renders as a time
    /// in the future and reads as a bug in Puna.
    #[test]
    fn an_age_never_goes_negative() {
        let future = "Mon, 17 Aug 2026 20:00:00 GMT";
        assert_eq!(age_ms(future, now()), Some(0));
        assert_eq!(age_ms("not a date", now()), None);
    }

    #[test]
    fn classification_checks_progression_first() {
        // An item can carry more than one bit; the first match is what it is called, in the
        // reference's own order.
        assert_eq!(classify(0b011), "progression");
        assert_eq!(classify(0b010), "useful");
        assert_eq!(classify(0b100), "trap");
        assert_eq!(classify(0), "filler");
    }
}
