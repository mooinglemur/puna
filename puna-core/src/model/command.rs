//! The console's command set, and who may run each one.
//!
//! Commands are **rows, not RPCs**. That is what gives them an audit trail, durability across an
//! orchestrator restart, and one place to enforce tiering — and it is why the web tier never needs
//! pahoa's credential to run one.
//!
//! ## The capability table is a `match`, checked in exactly one place
//!
//! [`RoomCommand::required_role`] is the only authority on who may run what. Adding a command means
//! answering "which tier?" in the same expression that defines it, rather than remembering to guard
//! a route — and the compiler makes the answer mandatory, because the match is exhaustive.
//!
//! ## Three semantics the UI must reflect rather than assume
//!
//! - **An admin is not bound by the modes that gate players.** `--release-mode disabled` stops
//!   `!release` and does *not* stop `{"command":"release"}` — acting for somebody who cannot is the
//!   point. So the console must not grey out commands based on the room's options.
//! - **`hint` has two modes.** `force: true` grants outright and spends nothing; the default charges
//!   the slot's own points as `!hint` would and **may grant fewer than asked, or none**. `granted`
//!   in the output is the truth, not the request.
//! - **`kick` is a disconnect, not a ban.** Every socket the slot holds closes and an immediate
//!   reconnect is possible.
//!
//! ## What is deliberately not here
//!
//! **`rotate_password` IS a command here, and §6 says it should not be.** That section was written
//! before the tier boundary existed: it assumed rotation would be
//! `POST /admin/v1/slots/<n>/password` called directly, and the web tier has **no egress to room
//! pods at all**. Only the orchestrator can reach a room, so asking it through this queue is the
//! only shape available.
//!
//! Its stated objection is answered rather than ignored — the variant carries **no password**, only
//! a slot number, so the audit trail records what was rotated and by whom without holding the value.
//! What remains true is that it is not a *pahoa command*: the dispatcher handles it before the
//! passthrough, because pahoa's own set is the fifteen others and it would answer `400`.
//!
//! `lock` used to be the second such exception and is not any more — pahoa shipped the verb, so it
//! is an ordinary passthrough. See [`RoomCommand::LockSlot`].
//!
//! **There is no room-wide password setter and there will not be one** (P18, settled): pahoa
//! declined it because a change it cannot persist reverts at the next restart in every deployment.

use serde::{Deserialize, Serialize};

use super::member::RoomRole;

/// One thing an organizer or helper can ask a room to do.
///
/// Transcribed from pahoa's `http/command.rs` — the tags and field names are its wire format, and
/// a mismatch is a `400` the room will explain but Puna cannot have anticipated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum RoomCommand {
    Status,
    Say {
        text: String,
    },
    Countdown {
        seconds: i64,
    },
    Release {
        slot: i32,
    },
    Collect {
        slot: i32,
    },
    SendItem {
        slot: i32,
        item: String,
    },
    Hint {
        slot: i32,
        item: String,
        /// `true` grants outright and spends nothing. The default charges the slot's own points and
        /// **may grant fewer than asked, or none** — so the caller renders the answer, not the ask.
        #[serde(default)]
        force: bool,
    },
    /// Hint at what is *in* a location, where [`Hint`](Self::Hint) hints at where an item *is*.
    ///
    /// A separate verb rather than a flag, because that is how the reference names it and an
    /// operator who knows `/hint_location` looks for that word. The location is in the target
    /// slot's own world, so it resolves in **that slot's game** — which is also the game whose
    /// name table an autocomplete must read.
    HintLocation {
        slot: i32,
        location: String,
        #[serde(default)]
        force: bool,
    },
    /// **Check** a location, sending out whatever it holds — one step past hinting at it.
    ///
    /// The same distinction `!hint` and the reference's `/send_location` draw, and the reason this
    /// is not a mode of [`HintLocation`](Self::HintLocation): one tells somebody where to look and
    /// the other reaches in and takes it.
    SendLocation {
        slot: i32,
        location: String,
    },
    /// Several copies of one item.
    ///
    /// **`amount` is required and pahoa caps it at 100**, deliberately: every copy is queued on
    /// both of a slot's item streams and replayed from index zero on each reconnect, so a stray
    /// extra digit is a room that never finishes sending. A default of one would make a
    /// `send_multiple` that did a fraction of its job look like it worked, which is why there is no
    /// default here either — `send_item` is the one-copy spelling.
    SendMultiple {
        slot: i32,
        item: String,
        amount: i64,
    },
    /// Exempt one slot from the room's `release_mode`, or return it to the mode.
    ///
    /// **An exemption, not a third permission level**, and the trap is the `false` case: it clears
    /// the exemption and returns that slot to whatever the room's mode says — which may still
    /// permit releasing. It does **not** forbid it. The reference spells these as two commands and
    /// the second is called `/forbid_release`, which reads like a denial and is not one; pahoa made
    /// it one command with a boolean for exactly that reason, and the UI has to carry the same
    /// care. There is no collect equivalent, in pahoa or upstream.
    AllowRelease {
        slot: i32,
        allowed: bool,
    },
    /// Set or clear **another** player's alias, which `!alias` only lets a player do for
    /// themselves. Empty clears it; pahoa truncates to 16 characters as the chat command does.
    Alias {
        slot: i32,
        alias: String,
    },
    /// Change one of the room's gameplay options on the **running** room.
    ///
    /// **The verb that changes what Puna can do**, and the only one here that is an organizer's.
    /// Before it existed, a room's rules could be changed by a chat user holding the server
    /// password and *not* by a token holder — which inverted the trust ordering, since the bearer
    /// token is the stronger credential and the one Puna holds.
    ///
    /// **These changes PERSIST**, and that is the opposite of the password contract in §4: the save
    /// is authoritative for gameplay options, so a restart restores what was set here over whatever
    /// flag the room was started with. §7's "gameplay flags are an initial value, never a setting"
    /// rule is unchanged by this — what changes is that Puna finally has a write path, which §7 said
    /// a settings UI would need before Puna could store any of them.
    ///
    /// `value` is a string on the wire even for a number or a boolean. pahoa accepts all three and
    /// parses from text either way, and a string keeps this type `Eq` — `serde_json::Value` is not,
    /// because of floats. The two passwords are **recognized and refused by name** with an
    /// explanation rather than an "unknown option", so sending one is an answer rather than a bug.
    #[serde(rename = "option")]
    SetOption {
        name: String,
        value: String,
    },
    /// Push a slot's **already-rotated** password to the Secret and then to the running room.
    ///
    /// **The one variant that is not a pahoa command**, and the departure from §6 is deliberate.
    /// That section says rotation is `POST /admin/v1/slots/<n>/password` called directly rather than
    /// a ninth command — written before the tier boundary was drawn. The web tier has **no egress to
    /// room pods at all** (its NetworkPolicy says so, and calls it the point rather than an
    /// omission), so the only process that can reach a room is the orchestrator. This queue is how
    /// you ask it to.
    ///
    /// §6's stated objection was that a command variant "would put a credential in the audit
    /// trail". It carries **no password** for exactly that reason: the new value is already in
    /// `room_slots` and the orchestrator reads it there. This row records that slot 3 was rotated,
    /// by whom, and when — which is what an audit trail is for.
    ///
    /// The dispatcher must handle it **before** the generic passthrough. Serialized into a pahoa
    /// `/admin/v1/command` body it would be a `400`, since pahoa's command set is the eight above.
    RotatePassword {
        slot: i32,
    },
    /// Bar one slot from connecting, or let it back in.
    ///
    /// **pahoa's own verb since it shipped `lock`**, and an ordinary passthrough. Puna used to
    /// achieve this by omitting the slot from `PAHOA_SLOT_PASSWORDS` and relying on the fail-closed
    /// rule — which worked, and was worse on four counts: it needed per-slot mode to be in force at
    /// all, it took a Secret write plus a live push with an ordering between them, it overloaded a
    /// password map with an access decision, and it made "which slots have credentials" and "who is
    /// barred" the same field. **Locking now works in every password mode.**
    ///
    /// **It bars the next login and disconnects nobody.** Those are separate decisions and separate
    /// commands, and the order matters: `lock` then [`Kick`](Self::Kick), because kicking first
    /// leaves a window in which they reconnect. The obvious reading of a control called "Lock" is
    /// that it ejects somebody, so anything offering it has to say otherwise.
    ///
    /// **A locked slot is refused with `["InvalidSlot", "SlotLocked"]`** — both, in that order. The
    /// protocol's reason list is closed and has nothing for this; `InvalidSlot` is what makes a
    /// stock client stop cleanly instead of retrying on a doubling delay, and `SlotLocked` is what
    /// lets a reader tell a lock from a typo. The accepted cost is that **a stock client tells a
    /// locked player their slot name is invalid**, which staff need to know before somebody reports
    /// it as a bug.
    ///
    /// **Puna's `room_slots.locked_at` stays the record of intent**, and that is deliberate rather
    /// than redundant: pahoa persists the lock in `room.save`, so it goes with a save that is reset
    /// or a PVC that is recreated — which the old Secret-based lock survived, because it lived in
    /// Puna's own state. Puna keeps the intent and the audit trail (`locked_by`, `locked_at`, which
    /// pahoa does not record), and re-applies it when a room starts.
    #[serde(rename = "lock")]
    LockSlot {
        slot: i32,
        /// `true` locks, `false` lets them back in. pahoa defaults an absent `locked` to true; Puna
        /// sends it either way, for the reason it does on `allow_release` — a body that says which
        /// way it meant is worth more than a byte saved, on a command whose two directions are easy
        /// to confuse.
        locked: bool,
    },
    Kick {
        slot: i32,
        /// Optional on the wire; kicking without a stated reason is allowed and the client is
        /// simply not told one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl RoomCommand {
    /// The wire tag, which is also how the command is named in an event row and a log line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Say { .. } => "say",
            Self::Countdown { .. } => "countdown",
            Self::Release { .. } => "release",
            Self::Collect { .. } => "collect",
            Self::SendItem { .. } => "send_item",
            Self::SendMultiple { .. } => "send_multiple",
            Self::Hint { .. } => "hint",
            Self::HintLocation { .. } => "hint_location",
            Self::SendLocation { .. } => "send_location",
            Self::AllowRelease { .. } => "allow_release",
            Self::Alias { .. } => "alias",
            Self::SetOption { .. } => "option",
            Self::Kick { .. } => "kick",
            Self::RotatePassword { .. } => "rotate_password",
            Self::LockSlot { .. } => "lock",
        }
    }

    /// **The capability table.** The one authority on who may run what.
    ///
    /// **Every command is a helper's**, and the split that decides it is *the room versus the
    /// game inside it*. A helper is somebody an organizer trusts to run the multiworld day to
    /// day — answering a stuck player, releasing a world whose owner has gone quiet, rotating a
    /// password somebody pasted in the wrong channel. Making them fetch an organizer for each of
    /// those makes the tier useless and the organizer a bottleneck.
    ///
    /// What a helper may *not* do is not expressible here at all, which is why this table reads
    /// uniform rather than empty: starting, stopping and closing the room, changing its password
    /// mode, and touching the roster are ordinary routes guarded by
    /// [`crate::model::member::RoomRole::Organizer`], not commands. The boundary is that a helper
    /// runs the room and cannot change who runs it or whether it runs at all.
    ///
    /// It stays a table rather than collapsing into a constant deliberately: a new pahoa command
    /// should have to be *given* a tier, and the day one wants `Organizer` this is where that is
    /// said. See `the_capability_table_matches_the_design`, which pins the current answer.
    ///
    /// **`option` is the day that came.** It is the first command that is not a helper's, and it is
    /// the same line M20 drew everywhere else: a helper runs the multiworld, an organizer decides
    /// whether it runs, *how it is configured*, and who is trusted with it. Every other verb here
    /// acts on one slot's game; `option` changes the rules the whole room plays by, and — unlike
    /// everything else on this list — it **persists into the save**, so it outlives the person who
    /// set it.
    pub fn required_role(&self) -> RoomRole {
        match self {
            // Reads and speech. Nothing here changes a player's game.
            Self::Status | Self::Say { .. } | Self::Countdown { .. } => RoomRole::Helper,
            // `hint` costs the slot's points, and is still a helper's: that is the support action
            // the tier exists for -- a player stuck on a lost item asks, and a helper answers.
            // `hint_location` is the same act pointed the other way round.
            Self::Hint { .. } | Self::HintLocation { .. } => RoomRole::Helper,
            // These reach into somebody's game or end their session, and they are a helper's too:
            // each is a thing a player asks staff for, and none of them changes the room itself.
            // `kick` in particular is a disconnect rather than a ban -- the player may reconnect
            // immediately -- so it is moderation, which is the work this tier is for.
            Self::Release { .. }
            | Self::Collect { .. }
            | Self::SendItem { .. }
            | Self::SendMultiple { .. }
            | Self::SendLocation { .. }
            // Granting one slot an exemption from `release_mode` is strictly WEAKER than releasing
            // for them, which is a helper's two lines up: this lets the player do it themselves.
            // A tier that may do the thing must be able to permit it.
            | Self::AllowRelease { .. }
            // Renaming somebody who named themselves something the room should not have to read is
            // moderation in its plainest form.
            | Self::Alias { .. }
            | Self::Kick { .. }
            // Rotating one slot's password is a credential change WITHIN a mode, and locking a slot
            // is the same endpoint saying nobody. Changing the MODE is the organizer's decision and
            // is a room restart, so it is a settings route rather than a command.
            | Self::RotatePassword { .. }
            | Self::LockSlot { .. } => RoomRole::Helper,
            // See the note above: the room's own rules, and they persist.
            Self::SetOption { .. } => RoomRole::Organizer,
        }
    }

    /// The slot this acts on, where it acts on one.
    ///
    /// Every targeted command carries its target **explicitly**, because pahoa's underlying
    /// handlers are connection-scoped — `cmd_release(conn, out)` releases *the caller's* slot — and
    /// the admin variants supply the target rather than inferring it. There is no caller to infer
    /// from here.
    pub fn target_slot(&self) -> Option<i32> {
        match self {
            // `option` is room-wide, which is exactly what makes it the organizer's one.
            Self::Status | Self::Say { .. } | Self::Countdown { .. } | Self::SetOption { .. } => {
                None
            }
            Self::Release { slot }
            | Self::Collect { slot }
            | Self::SendItem { slot, .. }
            | Self::SendMultiple { slot, .. }
            | Self::Hint { slot, .. }
            | Self::HintLocation { slot, .. }
            | Self::SendLocation { slot, .. }
            | Self::AllowRelease { slot, .. }
            | Self::Alias { slot, .. }
            | Self::Kick { slot, .. }
            | Self::RotatePassword { slot }
            | Self::LockSlot { slot, .. } => Some(*slot),
        }
    }
}

/// What a room answered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutput {
    /// **`false` is an ANSWER, not a failure.** The room understood and said no — no such slot,
    /// nobody to kick, a countdown out of range. It lands in a terminal state with `output` saying
    /// why, because retrying it would loop forever and, under the 10/min limit, lock the room out.
    pub ok: bool,
    /// Rendered verbatim: pahoa's phrasing is what an Archipelago organizer expects to read.
    #[serde(default)]
    pub output: Vec<String>,
    #[serde(default)]
    pub affected_slots: Vec<i32>,
}

/// How a room's HTTP answer maps onto `room_commands.state`.
///
/// **Getting this wrong causes retry storms**, which is why it is a type rather than a series of
/// `if`s at the call site: under pahoa's 10-failures-per-minute limit a loop locks Puna out of the
/// room for the rest of the window, and the lockout applies to the correct token too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The room answered, whether yes or no. Terminal, and `result.ok` carries which.
    Answered,
    /// Malformed — unknown command, missing field, wrong type. **A Puna bug**, not a caller's:
    /// this set is generated from a typed enum, so a `400` means the two sides have drifted.
    /// Terminal, and worth alerting on.
    Malformed,
    /// Rate limited. Terminal for this attempt; the caller honors `Retry-After` and must not retry
    /// inside the window.
    RateLimited,
    /// Transport or `5xx`. Terminal for this attempt, retryable by a person.
    Failed,
}

impl Disposition {
    /// The `command_state` this lands in. Every disposition is terminal: a dispatcher that left a
    /// command `pending` on a refusal would re-run it every tick forever.
    pub fn state(self) -> &'static str {
        match self {
            Self::Answered => "ok",
            Self::Malformed | Self::RateLimited | Self::Failed => "failed",
        }
    }

    pub fn from_status(status: u16) -> Self {
        match status {
            200..=299 => Self::Answered,
            400 => Self::Malformed,
            429 => Self::RateLimited,
            _ => Self::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **pahoa's fifteen verbs, each beside the JSON pahoa's own parser reads.**
    ///
    /// One list rather than two, because the previous shape — a list of commands here and a series
    /// of indexed assertions below — meant a command could be added to the set and quietly not
    /// checked against the wire, which is the only thing this file is really for.
    ///
    /// Transcribed from `pahoa-net/src/http/command.rs`, **not** from the handoff's summary table:
    /// the table omits that `allowed` defaults to true, that an empty `alias` clears, and that
    /// `value` is accepted as a bare string.
    fn the_pahoa_set() -> Vec<(RoomCommand, serde_json::Value)> {
        use serde_json::json;
        vec![
            (RoomCommand::Status, json!({"command": "status"})),
            (
                RoomCommand::Say { text: "hi".into() },
                json!({"command": "say", "text": "hi"}),
            ),
            (
                RoomCommand::Countdown { seconds: 10 },
                json!({"command": "countdown", "seconds": 10}),
            ),
            (
                RoomCommand::Release { slot: 3 },
                json!({"command": "release", "slot": 3}),
            ),
            (
                RoomCommand::Collect { slot: 3 },
                json!({"command": "collect", "slot": 3}),
            ),
            (
                RoomCommand::SendItem {
                    slot: 3,
                    item: "Bow".into(),
                },
                json!({"command": "send_item", "slot": 3, "item": "Bow"}),
            ),
            (
                RoomCommand::SendMultiple {
                    slot: 3,
                    item: "Rupee".into(),
                    amount: 5,
                },
                json!({"command": "send_multiple", "slot": 3, "item": "Rupee", "amount": 5}),
            ),
            (
                RoomCommand::Hint {
                    slot: 3,
                    item: "Progressive Sword".into(),
                    force: false,
                },
                json!({
                    "command": "hint", "slot": 3, "item": "Progressive Sword", "force": false
                }),
            ),
            (
                RoomCommand::HintLocation {
                    slot: 3,
                    location: "Attic".into(),
                    force: false,
                },
                json!({
                    "command": "hint_location", "slot": 3, "location": "Attic", "force": false
                }),
            ),
            (
                RoomCommand::SendLocation {
                    slot: 3,
                    location: "Attic".into(),
                },
                json!({"command": "send_location", "slot": 3, "location": "Attic"}),
            ),
            (
                // Sent explicitly in both directions rather than omitted when true. pahoa defaults
                // an absent `allowed` to true, so the two agree -- but this command's whole hazard
                // is that `false` reads like a denial and is not one, and a body that says which
                // way it meant is worth more than a byte saved.
                RoomCommand::AllowRelease {
                    slot: 3,
                    allowed: true,
                },
                json!({"command": "allow_release", "slot": 3, "allowed": true}),
            ),
            (
                RoomCommand::Alias {
                    slot: 3,
                    alias: "Organizer".into(),
                },
                json!({"command": "alias", "slot": 3, "alias": "Organizer"}),
            ),
            (
                // A STRING, even for an integer option. pahoa accepts a string, a number or a
                // boolean and parses from text either way -- and a string is what keeps this enum
                // `Eq`, since `serde_json::Value` is not.
                RoomCommand::SetOption {
                    name: "hint_cost".into(),
                    value: "20".into(),
                },
                json!({"command": "option", "name": "hint_cost", "value": "20"}),
            ),
            (
                RoomCommand::Kick {
                    slot: 3,
                    reason: Some("afk".into()),
                },
                json!({"command": "kick", "slot": 3, "reason": "afk"}),
            ),
            (
                // The tag is `lock`, not `lock_slot`: the Rust name says which noun it acts on,
                // the wire name is pahoa's.
                RoomCommand::LockSlot {
                    slot: 3,
                    locked: true,
                },
                json!({"command": "lock", "slot": 3, "locked": true}),
            ),
        ]
    }

    fn every_command() -> Vec<RoomCommand> {
        the_pahoa_set().into_iter().map(|(c, _)| c).collect()
    }

    /// **`rotate_password` is not pahoa's, and `every_command` above is the pahoa set.**
    ///
    /// The wire-shape test walks that list against pahoa's parser; this one would fail it, because
    /// pahoa has no such command and would answer `400`. It is a Puna instruction that happens to
    /// travel on the same queue, and the dispatcher must intercept it before the passthrough — a
    /// source lint over `dispatch.rs` asserts that ordering, since getting it wrong is a `400`
    /// logged as "Puna generated a body the room could not read", which is true and unhelpful.
    ///
    /// **`lock` used to be the second one and is not any more.** pahoa shipped the verb, so it moved
    /// into the list above and out of the intercept. It is asserted there rather than here.
    ///
    /// It still round-trips through the row, because it is stored there like any other.
    #[test]
    fn the_rotation_command_is_not_part_of_pahoas_set_but_still_round_trips() {
        let command = RoomCommand::RotatePassword { slot: 3 };
        assert!(
            !every_command().contains(&command),
            "the rotation command reached the list this crate walks against pahoa's wire format"
        );

        let stored = serde_json::to_value(&command).expect("serializes");
        assert_eq!(
            serde_json::from_value::<RoomCommand>(stored).expect("parses"),
            command
        );

        // It carries a slot and NO password: the value is in `room_slots`, and this row is read by
        // anybody who can read the room's command history.
        assert_eq!(command.target_slot(), Some(3));
        let body = serde_json::to_string(&command).expect("serializes");
        assert!(
            !body.contains("\"password\":"),
            "a credential reached the audit trail: {body}"
        );
    }

    /// **The wire format is pahoa's, and a mismatch is a `400` nothing on this side can anticipate.**
    /// Transcribed from `pahoa-net/src/http/command.rs`, so this is the test that fails if either
    /// side renames a field.
    #[test]
    fn commands_serialize_to_pahoas_wire_shape() {
        for (command, expected) in the_pahoa_set() {
            assert_eq!(
                serde_json::to_value(&command).expect("serializes"),
                expected,
                "{} no longer matches pahoa's wire shape",
                command.name()
            );
        }

        // A kick with no reason omits the key rather than sending null: pahoa treats absent and
        // null alike, but an omitted optional is the shape its parser documents.
        assert_eq!(
            serde_json::to_value(RoomCommand::Kick {
                slot: 3,
                reason: None
            })
            .unwrap(),
            serde_json::json!({"command": "kick", "slot": 3})
        );
    }

    /// **pahoa's set is fifteen verbs, and the count is asserted so a new one cannot arrive
    /// untested.**
    ///
    /// The list above is hand-written, so a variant added to the enum and not to it would simply
    /// never be checked against the wire — the failure this whole module exists to prevent, arriving
    /// by omission rather than by error. Naming them individually is what makes the diff say which
    /// one appeared.
    #[test]
    fn the_pahoa_command_set_is_the_fifteen_verbs_it_shipped() {
        let names: Vec<&str> = every_command().iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            [
                "status",
                "say",
                "countdown",
                "release",
                "collect",
                "send_item",
                "send_multiple",
                "hint",
                "hint_location",
                "send_location",
                "allow_release",
                "alias",
                "option",
                "kick",
                "lock",
            ]
        );
    }

    /// **`allow_release` false is an exemption being cleared, not a prohibition**, and the wire has
    /// to say which way it meant rather than leaning on pahoa's default.
    ///
    /// Pinned separately because the hazard is semantic rather than syntactic: a reader who assumes
    /// the reference's `/forbid_release` naming will expect `false` to forbid releasing, and it
    /// returns the slot to `release_mode` — which may well still permit it.
    #[test]
    fn clearing_a_release_exemption_says_so_explicitly() {
        assert_eq!(
            serde_json::to_value(RoomCommand::AllowRelease {
                slot: 3,
                allowed: false
            })
            .unwrap(),
            serde_json::json!({"command": "allow_release", "slot": 3, "allowed": false})
        );
    }

    /// An empty `alias` is how a player's alias is cleared, so it must reach the wire as an empty
    /// string rather than being dropped as though the field were optional.
    #[test]
    fn an_empty_alias_is_sent_rather_than_omitted() {
        assert_eq!(
            serde_json::to_value(RoomCommand::Alias {
                slot: 3,
                alias: String::new()
            })
            .unwrap(),
            serde_json::json!({"command": "alias", "slot": 3, "alias": ""})
        );
    }

    /// The row stores the command as JSONB and the dispatcher reads it back, so a command that
    /// cannot round-trip is one that executes as something else after a restart.
    #[test]
    fn every_command_round_trips_through_the_row() {
        for command in every_command() {
            let stored = serde_json::to_value(&command).expect("serializes");
            let read: RoomCommand = serde_json::from_value(stored).expect("parses back");
            assert_eq!(read, command);
        }
    }

    /// Every command has a tier, and the compiler enforces it — but the *split* is a decision, so
    /// it is pinned here where changing it is visible in review.
    #[test]
    fn the_capability_table_matches_the_design() {
        use RoomRole::{Helper, Organizer};

        // Spelled out by name rather than derived from `required_role`, which would make this
        // test agree with the code by construction and assert nothing.
        for (name, expected) in [
            ("status", Helper),
            ("say", Helper),
            ("countdown", Helper),
            ("release", Helper),
            ("collect", Helper),
            ("send_item", Helper),
            ("send_multiple", Helper),
            ("hint", Helper),
            ("hint_location", Helper),
            ("send_location", Helper),
            ("allow_release", Helper),
            ("alias", Helper),
            ("kick", Helper),
            ("lock", Helper),
            ("rotate_password", Helper),
            // The one that is not, and the only one that changes the room rather than a game
            // inside it. See `required_role`.
            ("option", Organizer),
        ] {
            let command = every_command_including_punas()
                .into_iter()
                .find(|c| c.name() == name)
                .unwrap_or_else(|| panic!("{name} is no longer a command"));
            assert_eq!(command.required_role(), expected, "{name} changed tier");
        }

        // The ladder is `Ord`, so every check is `role >= required` -- an organizer may do
        // everything a helper may.
        assert!(Organizer >= Helper);
        assert!(
            every_command_including_punas()
                .iter()
                .all(|c| Organizer >= c.required_role())
        );
    }

    /// Every command, pahoa's and Puna's own, for the tier and target tables.
    fn every_command_including_punas() -> Vec<RoomCommand> {
        let mut all = every_command();
        all.push(RoomCommand::RotatePassword { slot: 3 });
        all
    }

    /// **A helper runs the multiworld; an organizer owns the room.** The split, asserted as a
    /// property rather than left to the per-command table above.
    ///
    /// It reads as one exception because it is one: everything a helper is trusted with acts on a
    /// single slot's game and can be undone by acting again, while `option` changes the rules the
    /// whole room plays by **and persists into the save**, outliving whoever set it and the pod it
    /// was set on.
    ///
    /// The consequence for the UI is the reason this is its own test: the console offers its menu
    /// unconditionally except here, so a command moving tier means a gate appearing or disappearing
    /// in `console.html`. A control that is visible and refuses teaches people the tool is broken.
    #[test]
    fn a_helper_runs_every_command_except_the_one_that_configures_the_room() {
        let withheld: Vec<&str> = every_command_including_punas()
            .iter()
            .filter(|c| c.required_role() > RoomRole::Helper)
            .map(|c| c.name())
            .collect();

        assert_eq!(
            withheld,
            ["option"],
            "the helper/organizer split moved, so the console's gating has to move with it"
        );
    }

    /// A targeted command that lost its target would act on nobody, or on pahoa's reserved slot 0.
    #[test]
    fn every_targeted_command_carries_its_target() {
        for command in every_command_including_punas() {
            match command {
                // `option` is room-wide, which is what puts it on the other side of the tier line.
                RoomCommand::Status
                | RoomCommand::Say { .. }
                | RoomCommand::Countdown { .. }
                | RoomCommand::SetOption { .. } => {
                    assert_eq!(command.target_slot(), None, "{}", command.name());
                }
                _ => assert_eq!(
                    command.target_slot(),
                    Some(3),
                    "{} lost its slot",
                    command.name()
                ),
            }
        }
    }

    /// **The mapping that prevents a retry storm.** `ok: false` is an answer and must be terminal;
    /// only a genuine transport problem is worth a person retrying, and `429` must never be folded
    /// in with it.
    #[test]
    fn a_refusal_is_terminal_and_only_transport_failures_are_not() {
        assert_eq!(Disposition::from_status(200), Disposition::Answered);
        assert_eq!(Disposition::from_status(202), Disposition::Answered);
        assert_eq!(Disposition::from_status(400), Disposition::Malformed);
        assert_eq!(Disposition::from_status(429), Disposition::RateLimited);
        assert_eq!(Disposition::from_status(503), Disposition::Failed);
        assert_eq!(Disposition::from_status(401), Disposition::Failed);

        // A room that said no is `ok` -- the command completed, and `result.ok` is the answer.
        // Marking it failed would invite a retry, and retrying a refusal loops forever.
        assert_eq!(Disposition::Answered.state(), "ok");

        // Everything else is terminal too. A dispatcher that left one `pending` would re-run it
        // on every tick, which under a 10-per-minute limit locks Puna out of the room.
        for disposition in [
            Disposition::Malformed,
            Disposition::RateLimited,
            Disposition::Failed,
        ] {
            assert_eq!(disposition.state(), "failed");
        }
    }

    /// `output` and `affected_slots` are optional on the wire, so a terse answer must not fail to
    /// parse — the console renders what it got.
    #[test]
    fn an_output_with_only_ok_still_parses() {
        let terse: CommandOutput = serde_json::from_value(serde_json::json!({"ok": true}))
            .expect("a minimal answer parses");
        assert!(terse.ok);
        assert!(terse.output.is_empty());
        assert!(terse.affected_slots.is_empty());
    }
}

// --- the queue ------------------------------------------------------------------------------------
//
// Shared because both tiers touch it: the web tier inserts and reads, the orchestrator claims and
// answers. Neither owns the shape, so neither declares it.

use chrono::{DateTime, Utc};
use diesel::sql_types::{BigInt, Jsonb, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use diesel_async::scoped_futures::ScopedFutureExt;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};

use crate::ids::{CommandId, RoomId};

/// The channel the orchestrator wakes on, and the one the web tier waits on.
pub const REQUEST_CHANNEL: &str = "puna_command";
pub const DONE_CHANNEL: &str = "puna_command_done";

/// One command in flight or finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRow {
    pub id: CommandId,
    pub room_id: RoomId,
    pub requested_by: i64,
    pub requested_role: RoomRole,
    pub command: RoomCommand,
    pub state: String,
    pub result: Option<CommandOutput>,
    pub error: Option<String>,
    pub requested_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl CommandRow {
    /// Whether the caller can stop waiting.
    pub fn is_finished(&self) -> bool {
        matches!(self.state.as_str(), "ok" | "failed" | "rejected")
    }
}

#[derive(diesel::QueryableByName)]
struct RawRow {
    #[diesel(sql_type = SqlUuid)]
    id: CommandId,
    #[diesel(sql_type = SqlUuid)]
    room_id: RoomId,
    #[diesel(sql_type = BigInt)]
    requested_by: i64,
    #[diesel(sql_type = Text)]
    requested_role: String,
    #[diesel(sql_type = Jsonb)]
    command: serde_json::Value,
    #[diesel(sql_type = Text)]
    state: String,
    #[diesel(sql_type = Nullable<Jsonb>)]
    result: Option<serde_json::Value>,
    #[diesel(sql_type = Nullable<Text>)]
    error: Option<String>,
    #[diesel(sql_type = Timestamptz)]
    requested_at: DateTime<Utc>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    finished_at: Option<DateTime<Utc>>,
}

/// A row this build cannot read is **dropped, not defaulted**.
///
/// The command JSON comes from a database that may be newer than this binary — a rollout runs both
/// for a few minutes. Executing a command this build parsed loosely would be acting on a guess
/// about what somebody asked for, which is the one thing an audited action must not do.
fn hydrate(raw: RawRow) -> Option<CommandRow> {
    Some(CommandRow {
        id: raw.id,
        room_id: raw.room_id,
        requested_by: raw.requested_by,
        requested_role: RoomRole::parse(&raw.requested_role)?,
        command: serde_json::from_value(raw.command).ok()?,
        state: raw.state,
        result: raw.result.and_then(|r| serde_json::from_value(r).ok()),
        error: raw.error,
        requested_at: raw.requested_at,
        finished_at: raw.finished_at,
    })
}

const COLUMNS: &str = "id, room_id, requested_by, requested_role::text AS requested_role, command, \
                       state::text AS state, result, error, requested_at, finished_at";

/// Queue a command, and wake the dispatcher.
///
/// The insert and the `NOTIFY` are one transaction so a notification can never precede the row it
/// announces — the dispatcher would look, find nothing, and the command would wait for the
/// backstop poll instead of running now.
pub async fn enqueue(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    requested_by: i64,
    requested_role: RoomRole,
    command: &RoomCommand,
) -> Result<CommandId, diesel::result::Error> {
    let id = CommandId::new();
    let body = serde_json::to_value(command).map_err(|e| {
        diesel::result::Error::SerializationError(Box::new(std::io::Error::other(e.to_string())))
    })?;

    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        async move {
            diesel::sql_query(
                "INSERT INTO room_commands
                    (id, room_id, requested_by, requested_role, command)
                 VALUES ($1, $2, $3, $4::room_role, $5)",
            )
            .bind::<SqlUuid, _>(id)
            .bind::<SqlUuid, _>(room)
            .bind::<BigInt, _>(requested_by)
            .bind::<Text, _>(requested_role.as_sql())
            .bind::<Jsonb, _>(body)
            .execute(conn)
            .await?;

            diesel::sql_query("SELECT pg_notify($1, $2)")
                .bind::<Text, _>(REQUEST_CHANNEL)
                .bind::<Text, _>(id.to_string())
                .execute(conn)
                .await?;

            Ok(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(id)
}

/// Take the oldest pending command, if one is free.
///
/// **The conditional `UPDATE` is what makes a double-run safe**: two dispatchers racing one row see
/// `state = 'pending'` once, so exactly one gets the row and the other gets nothing. That is the
/// same shape as every other mutation in the orchestrator, and it is why the leader lock is a
/// simplicity property rather than a correctness one.
pub async fn claim(
    conn: &mut AsyncPgConnection,
) -> Result<Option<CommandRow>, diesel::result::Error> {
    let rows: Vec<RawRow> = diesel::sql_query(format!(
        "UPDATE room_commands SET state = 'running', started_at = now()
          WHERE id = (
                SELECT id FROM room_commands
                 WHERE state = 'pending'
                 ORDER BY requested_at
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1)
        RETURNING {COLUMNS}"
    ))
    .load(conn)
    .await?;

    Ok(rows.into_iter().next().and_then(hydrate))
}

/// Record the outcome and wake whoever is waiting.
pub async fn finish(
    conn: &mut AsyncPgConnection,
    id: CommandId,
    state: &str,
    result: Option<&CommandOutput>,
    error: Option<&str>,
) -> Result<(), diesel::result::Error> {
    let body = result.and_then(|r| serde_json::to_value(r).ok());

    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        async move {
            diesel::sql_query(
                "UPDATE room_commands
                    SET state = $2::command_state, result = $3, error = $4, finished_at = now()
                  WHERE id = $1",
            )
            .bind::<SqlUuid, _>(id)
            .bind::<Text, _>(state)
            .bind::<Nullable<Jsonb>, _>(body)
            .bind::<Nullable<Text>, _>(error)
            .execute(conn)
            .await?;

            // In the same transaction as the write, so a waiter woken by this always finds the
            // finished row rather than the one it was already looking at.
            diesel::sql_query("SELECT pg_notify($1, $2)")
                .bind::<Text, _>(DONE_CHANNEL)
                .bind::<Text, _>(id.to_string())
                .execute(conn)
                .await?;

            Ok(())
        }
        .scope_boxed()
    })
    .await
}

pub async fn get(
    conn: &mut AsyncPgConnection,
    id: CommandId,
) -> Result<Option<CommandRow>, diesel::result::Error> {
    let rows: Vec<RawRow> =
        diesel::sql_query(format!("SELECT {COLUMNS} FROM room_commands WHERE id = $1"))
            .bind::<SqlUuid, _>(id)
            .load(conn)
            .await?;

    Ok(rows.into_iter().next().and_then(hydrate))
}

/// One room's recent commands, newest first — the console's history pane.
pub async fn recent(
    conn: &mut AsyncPgConnection,
    room: RoomId,
    limit: i64,
) -> Result<Vec<CommandRow>, diesel::result::Error> {
    let rows: Vec<RawRow> = diesel::sql_query(format!(
        "SELECT {COLUMNS} FROM room_commands
          WHERE room_id = $1 ORDER BY requested_at DESC LIMIT $2"
    ))
    .bind::<SqlUuid, _>(room)
    .bind::<BigInt, _>(limit)
    .load(conn)
    .await?;

    Ok(rows.into_iter().filter_map(hydrate).collect())
}

/// Fail commands left `running` by a dispatcher that went away.
///
/// **A `running` row is nobody's until it is stale**, because the process that claimed it is the
/// only one that will finish it — so a restart would otherwise leave commands pending forever with
/// a waiter that times out and no record of why. `older_than` must exceed anything this process
/// could legitimately still be doing; the probe's own timeout bounds that at a few seconds.
pub async fn fail_stale(
    conn: &mut AsyncPgConnection,
    older_than: std::time::Duration,
) -> Result<usize, diesel::result::Error> {
    let seconds = older_than.as_secs_f64();
    let ids: Vec<RawRow> = diesel::sql_query(format!(
        "UPDATE room_commands
            SET state = 'failed',
                error = 'the orchestrator restarted while this command was running',
                finished_at = now()
          WHERE state = 'running'
            AND started_at < now() - make_interval(secs => $1)
        RETURNING {COLUMNS}"
    ))
    .bind::<diesel::sql_types::Double, _>(seconds)
    .load(conn)
    .await?;

    // Woken individually: a waiter is keyed by command id, and there is no "everything changed"
    // notification it could act on.
    for row in &ids {
        diesel::sql_query("SELECT pg_notify($1, $2)")
            .bind::<Text, _>(DONE_CHANNEL)
            .bind::<Text, _>(row.id.to_string())
            .execute(&mut *conn)
            .await?;
    }

    Ok(ids.len())
}
