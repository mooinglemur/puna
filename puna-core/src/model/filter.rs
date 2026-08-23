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
//! Worth stating this loudly because pahoa's handoff carries an example whose comment reads the
//! other way round ("thin what this slot SENDS to a quarter" against `p: 0.25`) while its prose
//! gives the rule above. A UI label built on the wrong reading produces filters that do the
//! opposite of what was asked, so [`Rule::describe`] spells out the effect rather than printing the
//! number and hoping.

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
    pub fn label(self) -> &'static str {
        match self {
            Self::FromSlot => "from_slot — what this slot sends",
            Self::ToSlot => "to_slot — what reaches this slot",
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
    PrintJson,
    Set,
    SetReply,
    Retrieved,
    StatusUpdate,
}

impl Kind {
    pub const ALL: &'static [Self] = &[
        Self::Bounce,
        Self::PrintJson,
        Self::Set,
        Self::SetReply,
        Self::Retrieved,
        Self::StatusUpdate,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bounce => "bounce",
            Self::PrintJson => "print_json",
            Self::Set => "set",
            Self::SetReply => "set_reply",
            Self::Retrieved => "retrieved",
            Self::StatusUpdate => "status_update",
        }
    }

    /// Whether this kind takes a `tag` (bounce) or a `subtype` (print_json). Everything else takes
    /// neither, and offering a narrowing box that does nothing is how a filter gets written that
    /// matches more than its author meant.
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
pub const REFUSED_KINDS: &[(&str, &str)] = &[
    (
        "received_items",
        "dropping an item delivery desynchronizes the slot: the room advances its send index as it \
         sends, so the client would never learn what it missed",
    ),
    (
        "connected",
        "the slot would never complete its handshake, so it could not play at all",
    ),
    (
        "location_info",
        "this answers a scout the client asked for, so dropping it leaves the request unanswered \
         forever",
    ),
    (
        "room_update",
        "the client would stop learning what the room has done, and drift out of step with it",
    ),
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
    pub fn describe(&self) -> String {
        let what = match (&self.tag, &self.subtype) {
            (Some(tag), _) => format!("{} {tag}", self.kind.as_str()),
            (_, Some(subtype)) => format!("{} {subtype}", self.kind.as_str()),
            _ => self.kind.as_str().to_string(),
        };
        let way = match self.direction {
            Direction::FromSlot => "sent by this slot",
            Direction::ToSlot => "reaching this slot",
        };
        match self.p {
            None => format!("drop every {what} {way}"),
            Some(p) if p >= 1.0 => format!("drop every {what} {way}"),
            Some(p) if p <= 0.0 => format!("keep every {what} {way} (this rule drops nothing)"),
            Some(p) => format!(
                "drop {:.0}% of {what} {way} — about {:.0}% still get through",
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
        let thinned = bounce("DeathLink", Some(0.75)).describe();
        assert!(
            thinned.contains("drop 75%") && thinned.contains("25% still get through"),
            "p is the drop fraction, so 0.75 leaves a quarter: {thinned}"
        );
        assert!(
            bounce("DeathLink", None)
                .describe()
                .starts_with("drop every")
        );
        assert!(
            bounce("DeathLink", Some(1.0))
                .describe()
                .starts_with("drop every"),
            "p = 1 is the same as absent"
        );
        assert!(
            bounce("DeathLink", Some(0.0))
                .describe()
                .contains("nothing"),
            "a rule that drops nothing should say so rather than reading as active"
        );
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
        let names: Vec<&str> = REFUSED_KINDS.iter().map(|(name, _)| *name).collect();
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
        for (name, reason) in REFUSED_KINDS {
            assert!(
                Kind::parse(name).is_none(),
                "{name} is both valid and refused"
            );
            assert!(!reason.is_empty(), "{name} is refused with no reason");
        }
    }
}
