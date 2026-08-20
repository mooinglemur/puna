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
//! **`rotate_password` is not a command**, though §6's table lists it beside these. It is
//! `POST /admin/v1/slots/<n>/password`, a different endpoint with different semantics — it `404`s
//! outside per-slot mode, and it changes only the running room while the Secret stays authoritative.
//! Modelling it as a ninth variant here would put a credential in the audit trail and imply it goes
//! through the same dispatcher.
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
            Self::Hint { .. } => "hint",
            Self::Kick { .. } => "kick",
        }
    }

    /// **The capability table.** The one authority on who may run what.
    ///
    /// The split is "does this change the room or another player's game". A helper may talk, hint
    /// and count down; moving somebody else's items, kicking them, or acting on their behalf is an
    /// organizer's.
    pub fn required_role(&self) -> RoomRole {
        match self {
            // Reads and speech. Nothing here changes a player's game.
            Self::Status | Self::Say { .. } | Self::Countdown { .. } => RoomRole::Helper,
            // `hint` is a helper's despite costing points, because that is the support action the
            // tier exists for: a player stuck on a lost item asks, and a helper answers.
            Self::Hint { .. } => RoomRole::Helper,
            // Each of these reaches into somebody's game or ends their session.
            Self::Release { .. }
            | Self::Collect { .. }
            | Self::SendItem { .. }
            | Self::Kick { .. } => RoomRole::Organizer,
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
            Self::Status | Self::Say { .. } | Self::Countdown { .. } => None,
            Self::Release { slot }
            | Self::Collect { slot }
            | Self::SendItem { slot, .. }
            | Self::Hint { slot, .. }
            | Self::Kick { slot, .. } => Some(*slot),
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

    fn every_command() -> Vec<RoomCommand> {
        vec![
            RoomCommand::Status,
            RoomCommand::Say { text: "hi".into() },
            RoomCommand::Countdown { seconds: 10 },
            RoomCommand::Release { slot: 3 },
            RoomCommand::Collect { slot: 3 },
            RoomCommand::SendItem {
                slot: 3,
                item: "Bow".into(),
            },
            RoomCommand::Hint {
                slot: 3,
                item: "Progressive Sword".into(),
                force: false,
            },
            RoomCommand::Kick {
                slot: 3,
                reason: Some("afk".into()),
            },
        ]
    }

    /// **The wire format is pahoa's, and a mismatch is a `400` nothing on this side can anticipate.**
    /// Transcribed from `pahoa-net/src/http/command.rs`, so this is the test that fails if either
    /// side renames a field.
    #[test]
    fn commands_serialize_to_pahoas_wire_shape() {
        let rendered: Vec<serde_json::Value> = every_command()
            .iter()
            .map(|c| serde_json::to_value(c).expect("serializes"))
            .collect();

        assert_eq!(rendered[0], serde_json::json!({"command": "status"}));
        assert_eq!(
            rendered[1],
            serde_json::json!({"command": "say", "text": "hi"})
        );
        assert_eq!(
            rendered[2],
            serde_json::json!({"command": "countdown", "seconds": 10})
        );
        assert_eq!(
            rendered[3],
            serde_json::json!({"command": "release", "slot": 3})
        );
        assert_eq!(
            rendered[4],
            serde_json::json!({"command": "collect", "slot": 3})
        );
        assert_eq!(
            rendered[5],
            serde_json::json!({"command": "send_item", "slot": 3, "item": "Bow"})
        );
        assert_eq!(
            rendered[6],
            serde_json::json!({
                "command": "hint", "slot": 3, "item": "Progressive Sword", "force": false
            })
        );
        assert_eq!(
            rendered[7],
            serde_json::json!({"command": "kick", "slot": 3, "reason": "afk"})
        );

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

        for (command, expected) in [
            (RoomCommand::Status, Helper),
            (
                RoomCommand::Say {
                    text: String::new(),
                },
                Helper,
            ),
            (RoomCommand::Countdown { seconds: 1 }, Helper),
            (
                RoomCommand::Hint {
                    slot: 1,
                    item: String::new(),
                    force: false,
                },
                Helper,
            ),
            (RoomCommand::Release { slot: 1 }, Organizer),
            (RoomCommand::Collect { slot: 1 }, Organizer),
            (
                RoomCommand::SendItem {
                    slot: 1,
                    item: String::new(),
                },
                Organizer,
            ),
            (
                RoomCommand::Kick {
                    slot: 1,
                    reason: None,
                },
                Organizer,
            ),
        ] {
            assert_eq!(
                command.required_role(),
                expected,
                "{} changed tier",
                command.name()
            );
        }

        // The ladder is `Ord`, so every check is `role >= required` -- an organizer may do
        // everything a helper may.
        assert!(Organizer >= Helper);
        assert!(
            every_command()
                .iter()
                .all(|c| Organizer >= c.required_role())
        );
    }

    /// A targeted command that lost its target would act on nobody, or on pahoa's reserved slot 0.
    #[test]
    fn every_targeted_command_carries_its_target() {
        for command in every_command() {
            match command {
                RoomCommand::Status | RoomCommand::Say { .. } | RoomCommand::Countdown { .. } => {
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
