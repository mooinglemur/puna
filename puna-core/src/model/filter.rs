//! Traffic filters: which of a slot's messages a room drops.
//!
//! A filter exists for two problems that turned out to be one. A large sync has more DeathLinks
//! than it can carry and wants them thinned for everybody; or one client is crashing on a malformed
//! bounce that everybody else can see fine, and wants that one message type kept away from it. Both
//! are "drop some of this", so every rule carries a probability and a plain rule is one that always
//! fires.
//!
//! ## The room's filter and a slot's are INDEPENDENT
//!
//! pahoa's rule: a slot's ruleset **replaces** the room's rather than adding to it. Puna keeps that
//! rather than hiding it behind a maintained union — two authorities, one per scope — and its only
//! job across the boundary is to *say what the effective set would be*. That is
//! [`Effective::of`], and it is the whole of Puna's cleverness here.
//!
//! The consequence is a trap the UI has to speak to rather than the model prevent: **adding one
//! rule to a slot stops the room's rules reaching it**, and **deleting a slot's ruleset makes the
//! room's apply at once**. Neither is visible in the rule being edited, which is why
//! [`Effective`] carries what changed rather than only what applies.
//!
//! ## `p` is the probability of DROPPING
//!
//! Absent means always, so an omitted `p` is `1.0` and a plain rule drops everything it matches. To
//! leave a quarter of DeathLinks getting through, `p` is **0.75**, not 0.25.
//!
//! Worth stating loudly rather than assuming, because the two readings are equally natural and
//! nothing on screen distinguishes them: a label built on the wrong one produces filters that do
//! the opposite of what was asked, at the setting an operator is least likely to re-check. Taken
//! from pahoa's `Filter::drops`, which is the authority -- `dropped` iff `roll() < p` -- rather
//! than from prose either side. [`Rule::describe`] spells out the effect instead of printing the
//! number and hoping.
//!
//! An example in pahoa's README and its handoff read the other way round for a while (`p: 0.25`
//! annotated "thin to a quarter"); that is being corrected on their side. The reason it is recorded
//! here at all is that the ambiguity is inherent to the parameter, not to the sentence.

use serde::{Deserialize, Serialize};

/// Which way a message is travelling, in pahoa's words.
///
/// **`FromSlot` / `ToSlot`, never in/out.** Those are relative and nobody remembers to what: a
/// server author reads "inbound" as arriving at the room, an organizer reads it as what a player is
/// sending, and the two are opposites — so a rule read backwards is a filter that silently does
/// nothing. pahoa asked for these words to be carried rather than translated, and they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    FromSlot,
    ToSlot,
}

impl Direction {
    pub const ALL: &'static [Self] = &[Self::FromSlot, Self::ToSlot];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FromSlot => "from_slot",
            Self::ToSlot => "to_slot",
        }
    }

    /// The API's word, glossed. The word stays; the ambiguity does not.
    ///
    /// **"a slot", not "this slot"**, because one picker is rendered at three scopes — a room's
    /// filter, one slot's, and the bulk panel's. The gloss exists to say which end of the wire a
    /// direction names, and it does that without claiming a subject the page it is on may not have.
    /// [`Rule::describe`] is where the subject belongs, and it takes one.
    pub fn label(self) -> &'static str {
        match self {
            Self::FromSlot => "from_slot: what a slot sends",
            Self::ToSlot => "to_slot: what reaches a slot",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|d| d.as_str() == value)
    }
}

/// What sort of message a rule matches. A closed set, transcribed from pahoa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bounce,
    /// A slot's outgoing chat line. **The other half of chat from `PrintJson`**, and the two are
    /// easy to confuse: dropping a `Say` stops a slot being *heard*, dropping a `PrintJson`/`Chat`
    /// stops it *hearing*. One silences a spammer for everybody; the other spares one client a feed
    /// it cannot cope with.
    Say,
    PrintJson,
    Set,
    SetReply,
    Retrieved,
    StatusUpdate,
}

impl Kind {
    /// Ordered as the picker shows them, with the chat pair adjacent: what a slot says, then what
    /// reaches it. `ALL` drives that list, so this order is a UI decision as well as a list.
    pub const ALL: &'static [Self] = &[
        Self::Bounce,
        Self::Say,
        Self::PrintJson,
        Self::Set,
        Self::SetReply,
        Self::Retrieved,
        Self::StatusUpdate,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bounce => "bounce",
            Self::Say => "say",
            Self::PrintJson => "print_json",
            Self::Set => "set",
            Self::SetReply => "set_reply",
            Self::Retrieved => "retrieved",
            Self::StatusUpdate => "status_update",
        }
    }

    /// **The packet's own name, as upstream and pahoa spell it**, for anything a person reads.
    ///
    /// The wire spelling is snake_case because that is what pahoa's filter API takes, and it is the
    /// wrong thing to show: an organizer reaching for a filter knows these as `PrintJSON` and
    /// `SetReply` from the network protocol document and from every client's log, and `print_json`
    /// is a name only this API uses. Transcribed from `ServerPacket`/`ClientPacket` — note
    /// **`PrintJSON`**, which is the one that is not plain PascalCase.
    pub fn label(self) -> &'static str {
        match self {
            Self::Bounce => "Bounce",
            Self::Say => "Say",
            Self::PrintJson => "PrintJSON",
            Self::Set => "Set",
            Self::SetReply => "SetReply",
            Self::Retrieved => "Retrieved",
            Self::StatusUpdate => "StatusUpdate",
        }
    }

    /// **Whether this kind travels this way at all**, transcribed from pahoa's `Kind::travels`.
    ///
    /// Most of these are one-way: a `Set` is something a slot sends and a `PrintJSON` is something
    /// it receives, so only a `Bounce` — the relay, which exists in both directions — can be either.
    ///
    /// pahoa refuses the impossible pairing rather than storing a rule that never fires, and its
    /// reason is the one that matters here too: **a rule that cannot match looks exactly like a
    /// filter that is not working.** Which is precisely what happened — a `from_slot` `PrintJSON`
    /// rule for chat was accepted by Puna, stored, pushed, and refused by the room with a `400`,
    /// while the page went on showing it as the room's filter.
    pub fn travels(self, direction: Direction) -> bool {
        match self {
            Self::Bounce => true,
            Self::Say | Self::Set | Self::StatusUpdate => direction == Direction::FromSlot,
            Self::PrintJson | Self::SetReply | Self::Retrieved => direction == Direction::ToSlot,
        }
    }

    /// The same answer as [`Kind::travels`], space-separated, for a markup attribute.
    ///
    /// **Written out rather than built from `directions()`**, because a `&'static str` for a
    /// computed value costs either a leak or an allocation on a path that runs per request — and
    /// `vocabulary()` runs on every page view. `travels_text_agrees_with_travels` is what keeps the
    /// two in step, so this being a second copy is checked rather than trusted.
    pub fn travels_text(self) -> &'static str {
        match self {
            Self::Bounce => "from_slot to_slot",
            Self::Say | Self::Set | Self::StatusUpdate => "from_slot",
            Self::PrintJson | Self::SetReply | Self::Retrieved => "to_slot",
        }
    }

    /// The directions this kind can travel, for a picker that offers only what can work.
    pub fn directions(self) -> Vec<Direction> {
        Direction::ALL
            .iter()
            .copied()
            .filter(|d| self.travels(*d))
            .collect()
    }

    /// Whether this kind takes a `tag` (bounce) or a `subtype` (print_json). Everything else takes
    /// neither, and offering a narrowing box that does nothing is how a filter gets written that
    /// matches more than its author meant.
    ///
    /// **`Say` takes neither, and it is the one that invites the question**: it carries `text`, but
    /// that is a chat line rather than a key out of a closed set, and pahoa matches a narrowing
    /// exactly. A box that looked like "drop lines containing…" and behaved like an exact match on
    /// the whole message would be worse than no box. Thin a noisy slot with `p` instead.
    pub fn narrows_with(self) -> Option<&'static str> {
        match self {
            Self::Bounce => Some("tag"),
            Self::PrintJson => Some("subtype"),
            _ => None,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == value)
    }
}

/// **Message kinds pahoa recognizes and refuses, with the reason it gives.**
///
/// Refused rather than unknown, and the distinction is the point: these parse, they are things an
/// operator will genuinely reach for while trying to help a broken client, and "unknown kind" would
/// send them looking for a spelling mistake instead of telling them why it cannot work.
///
/// Checked here as well as at the room for the same reason `spec::args` refuses forbidden flags by
/// name: an answer that arrives before the round trip is worth more, and pahoa's own wording is
/// what gets shown either way.
///
/// Each entry is `(wire name, the packet's own name, why)`. The middle one is what a page shows,
/// for the reason [`Kind::label`] gives — these are read beside the kinds that ARE offered, and one
/// list in two spellings reads as two different vocabularies.
pub const REFUSED_KINDS: &[(&str, &str, &str)] = &[
    (
        "received_items",
        "ReceivedItems",
        "dropping an item delivery desynchronizes the slot: the room advances its send index as it \
         sends, so the client would never learn what it missed",
    ),
    (
        "connected",
        "Connected",
        "the slot would never complete its handshake, so it could not play at all",
    ),
    (
        "location_info",
        "LocationInfo",
        "this answers a scout the client asked for, so dropping it leaves the request unanswered \
         forever",
    ),
    (
        "room_update",
        "RoomUpdate",
        "the client would stop learning what the room has done, and drift out of step with it",
    ),
];

/// **Bounce tags an operator is likely to want**, as suggestions and nothing more.
///
/// The set is genuinely open — a bounce carries whatever tags its sender chose, and pahoa's own
/// comment calls the list "a convention rather than a schema, with `TrapLink` already the second
/// entry and not the last". So this is autocomplete, never validation: a tag typed by hand and not
/// on this list is an ordinary rule, because the next link type will exist before this constant
/// hears about it.
///
/// Transcribed from upstream's senders rather than invented: `CommonClient.py:743` sends
/// `["DeathLink"]`, and `worlds/smw/Client.py` sends `["TrapLink"]` and `["RingLink"]`.
pub const BOUNCE_TAGS: &[&str] = &["DeathLink", "TrapLink", "RingLink"];

/// **`print_json` subtypes, which unlike tags ARE a closed set.**
///
/// Transcribed from pahoa's `PrintJsonType` (`crates/pahoa-proto/src/server.rs`), whose `as_text`
/// is documented as "the wire spelling, which is also what a filter rule's `subtype` names" — so
/// this is the same list the room matches against, in the same spelling.
///
/// Still offered as suggestions rather than a picker: pahoa matches case-insensitively and a value
/// it does not recognize is a rule that matches nothing rather than an error, and a client the
/// reference gains a subtype for should be filterable here before Puna is rebuilt.
pub const PRINT_JSON_SUBTYPES: &[&str] = &[
    "ItemSend",
    "ItemCheat",
    "Hint",
    "Join",
    "Part",
    "Chat",
    "ServerChat",
    "Tutorial",
    "TagsChanged",
    "CommandResult",
    "AdminCommandResult",
    "Goal",
    "Release",
    "Collect",
    "Countdown",
];

/// One rule: what to drop, which way, and how often.
///
/// **`PartialEq` but not `Eq`**, because `p` is a float. That is why [`Matcher`] exists separately —
/// identity here is the matcher, not the whole rule, and a set keyed on something un-`Eq` would be
/// awkward for no gain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub direction: Direction,
    pub kind: Kind,
    /// Narrows a `bounce`. Matched case-insensitively, and a bounce matches on **any** of its tags —
    /// a real one carries `["AP", "DeathLink"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Narrows a `print_json`: `Chat`, `ItemSend`, `Hint`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    /// The probability of **dropping** a match. Absent is always, so an omitted `p` drops
    /// everything this rule matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
}

/// Who a rule's sentence is about, which is decided by the scope reading it rather than by the rule.
///
/// The same stored rule is described on two pages. On a slot's, `this slot` is exact. On the
/// **room's**, it is wrong in the way that matters — the page is about a rule applying to everybody,
/// and a sentence saying "sent by this slot" over a room-wide rule reads as though one slot were
/// singled out. The room's page names its own exceptions underneath, in the "does not reach these
/// slots" warning, so `any slot` is not an overclaim there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    /// One slot's page, and the roster's per-slot chip.
    ThisSlot,
    /// The room's own filter, which applies to everybody it reaches.
    AnySlot,
}

impl Subject {
    fn who(self) -> &'static str {
        match self {
            Self::ThisSlot => "this slot",
            Self::AnySlot => "any slot",
        }
    }
}

/// A rule's identity: everything but `p`.
///
/// **Rules are a set keyed on this, not an ordered list**, which is pahoa's design and what makes
/// its `PATCH` and `DELETE` answerable — a `DELETE` names a matcher and does not need to know what
/// `p` was set to. Puna keys on the same thing so the two agree about what "the same rule" means.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Matcher {
    pub direction: Direction,
    pub kind: Kind,
    pub tag: Option<String>,
    pub subtype: Option<String>,
}

impl Rule {
    pub fn matcher(&self) -> Matcher {
        Matcher {
            direction: self.direction,
            kind: self.kind,
            // Lowercased, because pahoa matches case-insensitively: `DeathLink` and `deathlink` are
            // one rule there, and two here would let a UI show a duplicate that the room collapses.
            tag: self.tag.as_ref().map(|t| t.to_lowercase()),
            subtype: self.subtype.as_ref().map(|s| s.to_lowercase()),
        }
    }

    /// How specific this rule is. **The most specific wins**, per pahoa — a rule naming a `tag` or
    /// `subtype` beats one naming only a kind, which is what lets a blanket thin and an exemption
    /// coexist in either order.
    pub fn specificity(&self) -> u8 {
        u8::from(self.tag.is_some()) + u8::from(self.subtype.is_some())
    }

    /// The effect, in words, rather than the number.
    ///
    /// **`p` is the drop probability**, so `p: 0.75` leaves a quarter getting through — the exact
    /// reading pahoa's own example comment contradicts. Printing "p = 0.75" invites the reader to
    /// supply whichever meaning they arrived with; saying what survives does not.
    /// **The packet's own name**, not the wire spelling, for the reason [`Kind::label`] gives.
    pub fn describe(&self, subject: Subject) -> String {
        let what = match (&self.tag, &self.subtype) {
            (Some(tag), _) => format!("{} {tag}", self.kind.label()),
            (_, Some(subtype)) => format!("{} {subtype}", self.kind.label()),
            _ => self.kind.label().to_string(),
        };
        let who = subject.who();
        let way = match self.direction {
            Direction::FromSlot => format!("sent by {who}"),
            Direction::ToSlot => format!("reaching {who}"),
        };
        match self.p {
            None => format!("drop every {what} {way}"),
            Some(p) if p >= 1.0 => format!("drop every {what} {way}"),
            Some(p) if p <= 0.0 => format!("keep every {what} {way} (this rule drops nothing)"),
            Some(p) => format!(
                "drop {:.0}% of {what} {way}, so about {:.0}% still get through",
                p * 100.0,
                (1.0 - p) * 100.0
            ),
        }
    }

    /// Why this rule cannot be used, if it cannot.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(p) = self.p
            && !(0.0..=1.0).contains(&p)
        {
            return Err(format!(
                "a probability is between 0 and 1, and {p} is not. It is the fraction DROPPED, so \
                 0.75 leaves a quarter getting through."
            ));
        }
        // **An impossible direction, refused here rather than at the room.** pahoa answers `400`,
        // and until this existed that arrived as "the room answered 400" over a rule the page was
        // still displaying as the room's filter — which is how a chat filter came to be set,
        // stored, and silently not in force.
        if !self.kind.travels(self.direction) {
            let sends = self.kind.travels(Direction::FromSlot);
            return Err(format!(
                "a {} cannot travel {}: it is something a slot {}, so this rule could never \
                 match. Use {} instead.",
                self.kind.label(),
                self.direction.as_str(),
                if sends { "sends" } else { "receives" },
                if sends {
                    Direction::FromSlot.as_str()
                } else {
                    Direction::ToSlot.as_str()
                }
            ));
        }
        // A narrowing field on a kind that does not take one matches nothing at the room and reads
        // as a working rule here, which is the quietest way to write a filter that does nothing.
        if self.tag.is_some() && self.kind != Kind::Bounce {
            return Err(format!(
                "only a bounce is narrowed by a tag, and this rule names {}",
                self.kind.as_str()
            ));
        }
        if self.subtype.is_some() && self.kind != Kind::PrintJson {
            return Err(format!(
                "only a print_json is narrowed by a subtype, and this rule names {}",
                self.kind.as_str()
            ));
        }
        Ok(())
    }
}

/// A slot's relationship to the room's filter — the three states, as a type.
///
/// **The absent ruleset and the empty one are different**, and holding them in one `Option<Vec<_>>`
/// is how they get confused: `[]` says *filtered by nothing even though the room filters*, which is
/// the only way to say "everybody except this one", and dropping the distinction would leave full
/// exemption reachable only through an inert rule.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotFilter {
    /// No ruleset of its own. Whatever the room does reaches this slot.
    Follows,
    /// An explicitly empty ruleset. Nothing is filtered here, room filter or not.
    Exempt,
    /// Its own rules, **instead of** the room's.
    Own(Vec<Rule>),
}

impl SlotFilter {
    /// How a stored row becomes a state. A row's absence is [`SlotFilter::Follows`], which is why
    /// this takes an `Option` rather than living on the row.
    pub fn from_stored(rules: Option<Vec<Rule>>) -> Self {
        match rules {
            None => Self::Follows,
            Some(rules) if rules.is_empty() => Self::Exempt,
            Some(rules) => Self::Own(rules),
        }
    }

    /// What to store, or `None` to remove the row.
    pub fn to_stored(&self) -> Option<Vec<Rule>> {
        match self {
            Self::Follows => None,
            Self::Exempt => Some(Vec::new()),
            Self::Own(rules) => Some(rules.clone()),
        }
    }

    /// Whether this slot differs from the room — which is what a roster chip marks.
    ///
    /// **Not "is filtered".** With a room filter in force every slot is filtered, so a chip meaning
    /// that lands on every row and distinguishes nothing. What is worth a mark is a slot the room's
    /// rules do not describe, in either direction: one with its own rules, and one deliberately
    /// exempt from rules everybody else has.
    pub fn diverges(&self) -> bool {
        !matches!(self, Self::Follows)
    }
}

/// What actually applies to one slot, and what an operator is about to change about it.
///
/// **This is the whole of Puna's role across the room/slot boundary.** It merges nothing: it reads
/// pahoa's replacement rule and says what comes out, so an operator can see the consequence before
/// choosing rather than after.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    /// The rules that actually apply to this slot right now.
    pub rules: Vec<Rule>,
    /// Whether they came from the room rather than the slot.
    pub from_room: bool,
}

impl Effective {
    pub fn of(room: &[Rule], slot: &SlotFilter) -> Self {
        match slot {
            // The room's, whole — this is the only branch where the room reaches the slot at all.
            SlotFilter::Follows => Self {
                rules: room.to_vec(),
                from_room: true,
            },
            SlotFilter::Exempt => Self {
                rules: Vec::new(),
                from_room: false,
            },
            // **Instead of, not as well as.** The room's rules are absent from this list on
            // purpose: that is the fact the UI has to state at the moment somebody adds a rule
            // here, because nothing about the rule they are typing hints at it.
            SlotFilter::Own(rules) => Self {
                rules: rules.clone(),
                from_room: false,
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// The room's rules that **stop applying** to a slot once it gets its own.
///
/// The warning shown when somebody is about to give a slot a ruleset while a room filter exists:
/// these are what that slot silently loses. Empty when the room does not filter, which is the case
/// where no warning belongs.
pub fn rules_lost_by_diverging(room: &[Rule]) -> Vec<Rule> {
    room.to_vec()
}

/// The room's rules that would **suddenly begin applying** to a slot if its ruleset were removed.
///
/// The mirror warning, and the one more likely to surprise: deleting a slot's filter is a
/// subtraction that adds something, because the room's rules are waiting underneath.
pub fn rules_gained_by_following(room: &[Rule]) -> Vec<Rule> {
    room.to_vec()
}

// --- storage -------------------------------------------------------------------------------------
//
// Rules are stored as the JSON pahoa reads, validated on the way in. Reading them back is a parse
// that can fail -- a row written by a future Puna, or hand-edited -- and every read here degrades to
// "no rules" rather than failing the page, because a roster that will not render is a worse answer
// than a chip that is missing. The one exception is the re-assert path, which must not push a
// half-understood ruleset at a room; that reads through `parse_strict`.

use diesel::sql_types::{BigInt, Integer, Jsonb, Uuid as SqlUuid};
use diesel_async::{AsyncPgConnection, RunQueryDsl};

use crate::ids::RoomId;

#[derive(diesel::QueryableByName)]
struct RulesRow {
    #[diesel(sql_type = Jsonb)]
    rules: serde_json::Value,
}

#[derive(diesel::QueryableByName)]
struct SlotRulesRow {
    #[diesel(sql_type = Integer)]
    slot_number: i32,
    #[diesel(sql_type = Jsonb)]
    rules: serde_json::Value,
}

/// Read a stored ruleset, treating anything unparseable as empty.
///
/// **Lossy on purpose at the read side.** A rule Puna cannot parse is one it cannot render or edit,
/// and refusing to draw the room page over it helps nobody; the room keeps filtering either way,
/// because pahoa holds its own copy. `parse_strict` is what the re-assert path uses, where silently
/// pushing fewer rules than are stored would be a real change nobody asked for.
fn parse_lossy(value: &serde_json::Value) -> Vec<Rule> {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// Read a stored ruleset, or say why it cannot be read.
pub fn parse_strict(value: &serde_json::Value) -> Result<Vec<Rule>, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("stored filter is unreadable: {e}"))
}

/// The room-wide ruleset, or `None` when the room does not filter.
///
/// A room needs no third state: with nothing above it to inherit from, an empty ruleset and no
/// ruleset mean the same thing, so the row is simply absent when it does not filter.
pub async fn room_filter(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Option<Vec<Rule>>, diesel::result::Error> {
    let rows: Vec<RulesRow> =
        diesel::sql_query("SELECT rules FROM room_filters WHERE room_id = $1")
            .bind::<SqlUuid, _>(room)
            .load(conn)
            .await?;

    // `into_iter().next()`, not `first()`: diesel's `RunQueryDsl` is in scope and brings its own
    // `first` along, which resolves ahead of the slice method and produces an unreadable error.
    Ok(rows.into_iter().next().map(|row| parse_lossy(&row.rules)))
}

/// Replace the room's ruleset. An empty slice removes it, because for a room the two are one thing.
pub async fn set_room_filter(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    rules: &[Rule],
    by: i64,
) -> Result<(), diesel::result::Error> {
    if rules.is_empty() {
        return clear_room_filter(conn, room).await;
    }

    let body = serde_json::to_value(rules).map_err(|e| {
        diesel::result::Error::SerializationError(Box::new(std::io::Error::other(e.to_string())))
    })?;

    diesel::sql_query(
        "INSERT INTO room_filters (room_id, rules, set_by, set_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (room_id) DO UPDATE
            SET rules = EXCLUDED.rules, set_by = EXCLUDED.set_by, set_at = now()",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Jsonb, _>(body)
    .bind::<BigInt, _>(by)
    .execute(conn)
    .await?;

    Ok(())
}

pub async fn clear_room_filter(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<(), diesel::result::Error> {
    diesel::sql_query("DELETE FROM room_filters WHERE room_id = $1")
        .bind::<SqlUuid, _>(room)
        .execute(conn)
        .await?;
    Ok(())
}

/// One slot's state. An absent row is [`SlotFilter::Follows`], which is the whole reason this
/// returns a state rather than an `Option<Vec<Rule>>`.
pub async fn slot_filter(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    slot: i32,
) -> Result<SlotFilter, diesel::result::Error> {
    let rows: Vec<RulesRow> = diesel::sql_query(
        "SELECT rules FROM room_slot_filters WHERE room_id = $1 AND slot_number = $2",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Integer, _>(slot)
    .load(conn)
    .await?;

    Ok(SlotFilter::from_stored(
        rows.into_iter().next().map(|row| parse_lossy(&row.rules)),
    ))
}

/// Every slot that has a state of its own, in slot order.
///
/// **This is also the room-filter warning.** Editing the room's filter does not reach any slot in
/// this list, because each has replaced or opted out of it — so the same query answers "what does
/// the roster chip say" and "who will this change miss", and the two cannot disagree.
///
/// **One query for the whole roster**, because the alternative is a read per row on a page that may
/// carry hundreds. Slots absent from the map follow the room, which is [`SlotFilter::Follows`] and
/// is why this returns only the divergent ones rather than a row per slot.
pub async fn slot_filters(
    conn: &mut AsyncPgConnection,
    room: RoomId,
) -> Result<Vec<(i32, SlotFilter)>, diesel::result::Error> {
    let rows: Vec<SlotRulesRow> = diesel::sql_query(
        "SELECT slot_number, rules FROM room_slot_filters WHERE room_id = $1 ORDER BY slot_number",
    )
    .bind::<SqlUuid, _>(room)
    .load(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.slot_number,
                SlotFilter::from_stored(Some(parse_lossy(&row.rules))),
            )
        })
        .collect())
}

/// Set one slot's state, including removing its ruleset entirely.
///
/// [`SlotFilter::Follows`] deletes the row; [`SlotFilter::Exempt`] stores `[]`. Those are different
/// writes for different states, which is the point of taking a state rather than a rule list.
pub async fn set_slot_filter(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    slot: i32,
    state: &SlotFilter,
    by: i64,
) -> Result<(), diesel::result::Error> {
    let Some(rules) = state.to_stored() else {
        diesel::sql_query("DELETE FROM room_slot_filters WHERE room_id = $1 AND slot_number = $2")
            .bind::<SqlUuid, _>(room)
            .bind::<Integer, _>(slot)
            .execute(conn)
            .await?;
        return Ok(());
    };

    let body = serde_json::to_value(&rules).map_err(|e| {
        diesel::result::Error::SerializationError(Box::new(std::io::Error::other(e.to_string())))
    })?;

    diesel::sql_query(
        "INSERT INTO room_slot_filters (room_id, slot_number, rules, set_by, set_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (room_id, slot_number) DO UPDATE
            SET rules = EXCLUDED.rules, set_by = EXCLUDED.set_by, set_at = now()",
    )
    .bind::<SqlUuid, _>(room)
    .bind::<Integer, _>(slot)
    .bind::<Jsonb, _>(body)
    .bind::<BigInt, _>(by)
    .execute(conn)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounce(tag: &str, p: Option<f64>) -> Rule {
        Rule {
            direction: Direction::FromSlot,
            kind: Kind::Bounce,
            tag: Some(tag.into()),
            subtype: None,
            p,
        }
    }

    /// The wire spelling is a contract with another program, so it is pinned rather than derived.
    #[test]
    fn the_vocabulary_keeps_pahoas_spelling() {
        assert_eq!(
            Direction::ALL
                .iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>(),
            ["from_slot", "to_slot"],
            "in/out is exactly what these words exist to avoid"
        );
        assert_eq!(
            Kind::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            [
                "bounce",
                "say",
                "print_json",
                "set",
                "set_reply",
                "retrieved",
                "status_update"
            ]
        );
        for kind in Kind::ALL {
            assert_eq!(
                serde_json::to_value(kind).expect("serialize"),
                serde_json::Value::String(kind.as_str().to_string()),
                "`as_str` and the serde tag disagree for {kind:?}"
            );
        }
        for direction in Direction::ALL {
            assert_eq!(
                serde_json::to_value(direction).expect("serialize"),
                serde_json::Value::String(direction.as_str().to_string())
            );
        }
    }

    /// The shape pahoa's own parser reads.
    #[test]
    fn a_rule_serializes_to_pahoas_wire_form() {
        assert_eq!(
            serde_json::to_value(bounce("DeathLink", Some(0.75))).expect("serialize"),
            serde_json::json!({
                "direction": "from_slot",
                "kind": "bounce",
                "tag": "DeathLink",
                "p": 0.75
            })
        );
        // An absent `p` is omitted rather than sent as null: absent means always, and a null would
        // be a third spelling of it for pahoa's parser to have an opinion about.
        assert_eq!(
            serde_json::to_value(bounce("DeathLink", None)).expect("serialize"),
            serde_json::json!({"direction": "from_slot", "kind": "bounce", "tag": "DeathLink"})
        );
    }

    /// **`p` is the fraction DROPPED**, and the description says what survives.
    ///
    /// This is the assertion that would catch the reading pahoa's example comment implies. Getting
    /// it backwards is a filter that does the opposite of what was asked, and nothing about the
    /// number on screen would give it away.
    #[test]
    fn a_description_says_what_gets_through_rather_than_printing_p() {
        let thinned = bounce("DeathLink", Some(0.75)).describe(Subject::ThisSlot);
        assert!(
            thinned.contains("drop 75%") && thinned.contains("25% still get through"),
            "p is the drop fraction, so 0.75 leaves a quarter: {thinned}"
        );
        assert!(
            bounce("DeathLink", None)
                .describe(Subject::ThisSlot)
                .starts_with("drop every")
        );
        assert!(
            bounce("DeathLink", Some(1.0))
                .describe(Subject::ThisSlot)
                .starts_with("drop every"),
            "p = 1 is the same as absent"
        );
        assert!(
            bounce("DeathLink", Some(0.0))
                .describe(Subject::ThisSlot)
                .contains("nothing"),
            "a rule that drops nothing should say so rather than reading as active"
        );
    }

    /// **The scope decides who the sentence is about**, and the room's page is the one that was
    /// wrong: a room-wide rule described as "sent by this slot" reads as though one slot had been
    /// singled out, on a page with no slot on it.
    #[test]
    fn a_rule_is_described_against_the_scope_reading_it() {
        let rule = bounce("DeathLink", None);
        assert!(
            rule.describe(Subject::ThisSlot)
                .ends_with("sent by this slot"),
            "{}",
            rule.describe(Subject::ThisSlot)
        );
        assert!(
            rule.describe(Subject::AnySlot)
                .ends_with("sent by any slot"),
            "{}",
            rule.describe(Subject::AnySlot)
        );

        let inbound = Rule {
            direction: Direction::ToSlot,
            ..bounce("DeathLink", None)
        };
        assert!(
            inbound
                .describe(Subject::AnySlot)
                .contains("reaching any slot")
        );
    }

    /// **A person reads the packet's name, never the wire spelling.** `print_json` is a name only
    /// pahoa's filter API uses; `PrintJSON` is what the protocol document, every client log and
    /// every organizer already calls it.
    #[test]
    fn a_description_names_the_packet_the_way_upstream_does() {
        let chat = Rule {
            direction: Direction::FromSlot,
            kind: Kind::PrintJson,
            tag: None,
            subtype: Some("Chat".into()),
            p: None,
        };
        let sentence = chat.describe(Subject::AnySlot);
        assert!(sentence.contains("PrintJSON Chat"), "{sentence}");
        assert!(!sentence.contains("print_json"), "{sentence}");

        // And every kind has one, so a kind pahoa adds cannot reach a page as snake_case.
        for kind in Kind::ALL {
            let label = kind.label();
            assert!(
                !label.contains('_') && label.starts_with(|c: char| c.is_ascii_uppercase()),
                "{} is shown to people as {label}",
                kind.as_str()
            );
        }
    }

    /// **The pairing that shipped broken.** A chat filter was written as `from_slot` `PrintJSON`,
    /// which Puna accepted and stored and pahoa answered `400` to — so the room page showed a
    /// filter the room had never taken, and every chat line went on getting through.
    ///
    /// Transcribed from pahoa's `Kind::travels`, which is the authority. The table is written out
    /// rather than derived, because deriving it from the same function it is checking would assert
    /// nothing.
    #[test]
    fn a_kind_that_cannot_travel_that_way_is_refused_before_the_room_sees_it() {
        let one_way = [
            (Kind::PrintJson, Direction::ToSlot),
            (Kind::SetReply, Direction::ToSlot),
            (Kind::Retrieved, Direction::ToSlot),
            // `Say` is a slot's own chat line going up, so it is the mirror of `PrintJson` — and
            // the pair is exactly the confusion this table exists to pin.
            (Kind::Say, Direction::FromSlot),
            (Kind::Set, Direction::FromSlot),
            (Kind::StatusUpdate, Direction::FromSlot),
        ];

        for (kind, works) in one_way {
            let wrong = if works == Direction::ToSlot {
                Direction::FromSlot
            } else {
                Direction::ToSlot
            };
            let rule = Rule {
                direction: wrong,
                kind,
                tag: None,
                subtype: None,
                p: None,
            };
            let message = rule.validate().expect_err(&format!(
                "{} travelling {} is a rule that can never match",
                kind.as_str(),
                wrong.as_str()
            ));
            assert!(message.contains(kind.label()), "{message}");
            // It says which direction to use instead, because the answer is always the other one.
            assert!(message.contains(works.as_str()), "{message}");

            assert!(
                Rule {
                    direction: works,
                    kind,
                    ..rule
                }
                .validate()
                .is_ok(),
                "{} should travel {}",
                kind.as_str(),
                works.as_str()
            );
        }

        // A bounce is the relay and exists in both directions — the only kind with a real choice.
        for direction in Direction::ALL {
            assert!(Kind::Bounce.travels(*direction));
        }
        assert_eq!(Kind::Bounce.directions().len(), 2);
        assert_eq!(Kind::PrintJson.directions(), vec![Direction::ToSlot]);
    }

    /// **The second copy, checked rather than trusted.** `travels_text` is hardcoded so the picker
    /// can carry it in an attribute without allocating per request; if it drifts from `travels`, the
    /// editor offers a direction the room refuses and the refusal arrives as a `400`.
    #[test]
    fn travels_text_agrees_with_travels() {
        for kind in Kind::ALL {
            let derived = kind
                .directions()
                .iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                kind.travels_text(),
                derived,
                "{} advertises directions it cannot travel",
                kind.as_str()
            );
        }
    }

    #[test]
    fn a_rule_that_could_not_work_is_refused_with_a_reason() {
        assert!(bounce("DeathLink", Some(1.5)).validate().is_err());
        assert!(bounce("DeathLink", Some(-0.1)).validate().is_err());
        assert!(bounce("DeathLink", Some(0.5)).validate().is_ok());

        // A narrowing field on a kind that does not take one matches nothing at the room while
        // reading as a working rule here.
        let mut wrong = bounce("DeathLink", None);
        wrong.kind = Kind::Set;
        assert!(wrong.validate().unwrap_err().contains("tag"));

        assert_eq!(Kind::Bounce.narrows_with(), Some("tag"));
        assert_eq!(Kind::PrintJson.narrows_with(), Some("subtype"));
        assert_eq!(Kind::Set.narrows_with(), None);
    }

    /// Identity is the matcher, and it is case-insensitive because pahoa's is.
    #[test]
    fn two_spellings_of_one_tag_are_one_rule() {
        assert_eq!(
            bounce("DeathLink", Some(0.5)).matcher(),
            bounce("deathlink", Some(0.9)).matcher(),
            "`p` is not identity, and the tag is matched case-insensitively"
        );
        let mut other = bounce("TrapLink", None);
        other.direction = Direction::ToSlot;
        assert_ne!(bounce("TrapLink", None).matcher(), other.matcher());
    }

    /// **The three states, and the replacement that surprises people.**
    #[test]
    fn a_slots_rules_replace_the_rooms_rather_than_adding_to_them() {
        let room = vec![bounce("DeathLink", Some(0.75))];

        let follows = Effective::of(&room, &SlotFilter::Follows);
        assert_eq!(follows.rules, room);
        assert!(follows.from_room);

        // The one that catches people: one rule of its own, and the room's thinning is gone.
        let own = Effective::of(
            &room,
            &SlotFilter::Own(vec![Rule {
                direction: Direction::ToSlot,
                kind: Kind::PrintJson,
                tag: None,
                subtype: Some("Chat".into()),
                p: None,
            }]),
        );
        assert_eq!(own.rules.len(), 1, "the room's rule does not survive");
        assert!(!own.rules.iter().any(|r| r.kind == Kind::Bounce));
        assert!(!own.from_room);

        let exempt = Effective::of(&room, &SlotFilter::Exempt);
        assert!(exempt.is_empty(), "exempt means filtered by nothing at all");
        assert!(!exempt.from_room);
    }

    /// `[]` and "no row" must survive a round trip as different things.
    #[test]
    fn an_empty_ruleset_is_not_the_same_as_no_ruleset() {
        assert_eq!(SlotFilter::from_stored(None), SlotFilter::Follows);
        assert_eq!(SlotFilter::from_stored(Some(vec![])), SlotFilter::Exempt);

        assert_eq!(SlotFilter::Follows.to_stored(), None);
        assert_eq!(SlotFilter::Exempt.to_stored(), Some(vec![]));

        // And the chip marks divergence, not "is filtered" -- both of these differ from the room.
        assert!(!SlotFilter::Follows.diverges());
        assert!(SlotFilter::Exempt.diverges());
        assert!(SlotFilter::Own(vec![bounce("DeathLink", None)]).diverges());
    }

    /// Both warnings are empty exactly when the room does not filter.
    #[test]
    fn nothing_is_lost_or_gained_when_the_room_does_not_filter() {
        assert!(rules_lost_by_diverging(&[]).is_empty());
        assert!(rules_gained_by_following(&[]).is_empty());

        let room = vec![bounce("DeathLink", Some(0.75))];
        assert_eq!(rules_lost_by_diverging(&room).len(), 1);
        assert_eq!(rules_gained_by_following(&room).len(), 1);
    }

    /// The refusals are the ones pahoa names, with reasons rather than "unknown kind".
    #[test]
    fn progression_kinds_are_refused_by_name() {
        let names: Vec<&str> = REFUSED_KINDS.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(
            names,
            [
                "received_items",
                "connected",
                "location_info",
                "room_update"
            ]
        );
        // None of them is also a valid kind, or the refusal would be unreachable.
        for (name, label, reason) in REFUSED_KINDS {
            assert!(
                Kind::parse(name).is_none(),
                "{name} is both valid and refused"
            );
            assert!(!reason.is_empty(), "{name} is refused with no reason");
            // Shown beside the kinds that ARE offered, so it is spelled the way they are.
            assert!(
                !label.contains('_') && label.starts_with(|c: char| c.is_ascii_uppercase()),
                "{name} is shown to people as {label}"
            );
        }
    }
}
