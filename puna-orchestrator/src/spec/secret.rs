//! Building a room's Kubernetes Secret.
//!
//! Every credential pahoa needs arrives as an environment variable from this Secret, attached with
//! `envFrom`, so the pod spec carries a reference and never a value. Nothing is argv: argv is
//! readable inside the container through `ps` and outside it through `kubectl get pod -o yaml`.
//!
//! ## `PAHOA_SLOT_PASSWORDS` fails closed, and that decides the shape of this module
//!
//! Pahoa's rule, settled in round two of the handoff: the variable's **presence** is what puts a
//! room in per-slot mode, and once it is in force **a slot missing from the map is refused**. Two
//! consequences, and this module exists to make both unrepresentable rather than merely avoided:
//!
//!   * **`{}` is a room nobody can join.** Per-slot mode with nobody holding a key. So the
//!     variable must be *absent* outside `per_slot` mode, not present and empty.
//!   * **A partial map locks players out.** So `per_slot` mode requires a password on every
//!     connectable slot -- spectators included, which is exactly the gap that would have been
//!     easy to leave.
//!
//! [`build`] returns [`SecretError`] rather than a Secret in either case. That is deliberate: a
//! room that fails to provision with a named reason is recoverable, and a room that comes up with
//! the wrong door open is not.
//!
//! Built ahead of its caller: `ensure_room_running` applies this at M7. The tests are the reason
//! it lands now rather than then -- the fail-closed property is worth pinning while the reasoning
//! behind it is fresh, and `expect` rather than `allow` means the attribute itself warns once
//! something starts calling `build`.
use std::collections::BTreeMap;

use puna_core::model::room::{Room, RoomSecrets, SlotAuth};
use puna_core::model::slot::Slot;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SecretError {
    /// `per_slot` mode with a slot that has no password.
    ///
    /// Under pahoa's fail-closed rule this would be a player who cannot connect, with an
    /// `InvalidPassword` that is accurate and useless. Caught here instead.
    #[error(
        "room is in per-slot password mode but {} {} no password ({slots:?}); \
         refusing to build a Secret that would lock them out",
        puna_core::text::count(*count, "slot"),
        puna_core::text::plural(*count, "has", "have"),
    )]
    IncompleteSlotPasswords { count: usize, slots: Vec<i32> },

    /// `per_slot` mode on a room with no slots at all.
    ///
    /// Would render `{}`, which pahoa reads as "per-slot mode, nobody holds a key" -- a locked
    /// room rather than an unconfigured one.
    #[error("room is in per-slot password mode but has no slots; that would lock the whole room")]
    NoSlots,

    /// `room` mode with no room password, or a password outside `room` mode.
    ///
    /// The database's `room_password_matches_mode` CHECK already forbids this, so reaching it
    /// means the row was written around the constraint.
    #[error("room-wide password does not match slot_auth = {mode}")]
    PasswordModeMismatch { mode: &'static str },
}

/// The environment a room's pod receives, as `key -> value`.
///
/// A `BTreeMap` so the ordering is deterministic: this feeds the spec hash, and a map that
/// iterated differently between ticks would make every room look changed on every sweep.
pub type SecretData = BTreeMap<String, String>;

/// Build the environment for one room.
///
/// `slots` must be every slot of the room, not a filtered subset -- the completeness check is the
/// point, and filtering before the call would defeat it.
pub fn build(
    room: &Room,
    secrets: &RoomSecrets,
    slots: &[Slot],
) -> Result<SecretData, SecretError> {
    let mut data = SecretData::new();

    // Always. Pahoa refuses to start on a token under 32 bytes, and answers 404 rather than 401
    // when none is configured -- which is how Puna diagnoses a Secret that failed to render.
    data.insert("PAHOA_ADMIN_TOKEN".to_string(), secrets.admin_token.clone());

    match room.slot_auth {
        SlotAuth::None => {
            if room.password.is_some() {
                return Err(SecretError::PasswordModeMismatch { mode: "none" });
            }
        }
        SlotAuth::Room => {
            let password = room
                .password
                .clone()
                .ok_or(SecretError::PasswordModeMismatch { mode: "room" })?;
            data.insert("PAHOA_PASSWORD".to_string(), password);
        }
        SlotAuth::PerSlot => {
            if room.password.is_some() {
                return Err(SecretError::PasswordModeMismatch { mode: "per_slot" });
            }
            if slots.is_empty() {
                return Err(SecretError::NoSlots);
            }

            // **A missing password is always the accident**, again. It briefly was not: while Puna
            // expressed a lock by withholding a slot from this map, an omission could be either a
            // mistake or a decision, and this filter had to tell them apart. pahoa shipped a native
            // `lock` verb, so the map is back to meaning one thing -- who holds a credential -- and
            // access control is not smuggled through it.
            let missing: Vec<i32> = slots
                .iter()
                .filter(|s| s.password.is_none())
                .map(|s| s.slot_number)
                .collect();
            if !missing.is_empty() {
                return Err(SecretError::IncompleteSlotPasswords {
                    count: missing.len(),
                    slots: missing,
                });
            }

            // JSON object keyed by slot number as a STRING, because JSON object keys are strings.
            // Pahoa parses exactly this shape.
            let map: BTreeMap<String, &str> = slots
                .iter()
                .filter_map(|s| Some((s.slot_number.to_string(), s.password.as_deref()?)))
                .collect();
            data.insert(
                "PAHOA_SLOT_PASSWORDS".to_string(),
                serde_json::to_string(&map).expect("a map of strings always serializes"),
            );
        }
    }

    // Independent of all three modes and rarely set: Puna's console drives the bearer-token API
    // rather than in-game `!admin`, so a room normally has no remote-admin gate at all.
    if let Some(server_password) = &secrets.server_password {
        data.insert("PAHOA_SERVER_PASSWORD".to_string(), server_password.clone());
    }

    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use puna_core::Environment;
    use puna_core::artifact::SlotKind;
    use puna_core::ids::{GenerationId, RoomId, TrackerId};
    use puna_core::model::RoomSource;
    use puna_core::model::room::{
        JournalPolicy, PatchPolicy, PrimaryPort, SpoilerPolicy, TrackerPolicy,
    };

    fn room(slot_auth: SlotAuth, password: Option<&str>) -> Room {
        Room {
            id: RoomId::new(),
            name: "test".into(),
            environment: Environment::Dev,
            generation_id: GenerationId::new(),
            source: RoomSource::Direct,
            created_by: None,
            created_at: chrono::Utc::now(),
            cloned_from: None,
            lobby_room_id: None,
            desired_state: "stopped".into(),
            slot_auth,
            password: password.map(str::to_string),
            spoiler_policy: SpoilerPolicy::Staff,
            tracker_id: TrackerId::new(),
            journal_id: puna_core::ids::JournalId::new(),
            tracker_policy: TrackerPolicy::Link,
            journal_policy: JournalPolicy::Full,
            patch_policy: PatchPolicy::Claimed,
            primary_port: PrimaryPort::Full,
            wants_filtered: true,
            state: "idle".into(),
            state_changed_at: chrono::Utc::now(),
            desired_at: chrono::Utc::now(),
            advertised_host: None,
            advertised_port: None,
            advertised_filtered_port: None,
            last_error: None,
            // Absent, and it must stay that way here: the Secret builder decides what a room is
            // STARTED with, and the room's reported rules are what it is running with. A fixture
            // that supplied them would invite a future reader to reach for them.
            enhanced_tracker: false,
            gameplay_options: None,
            probed_at: None,
        }
    }

    fn secrets(server_password: Option<&str>) -> RoomSecrets {
        RoomSecrets {
            admin_token: "a".repeat(52),
            server_password: server_password.map(str::to_string),
        }
    }

    fn slot(number: i32, password: Option<&str>, kind: SlotKind) -> Slot {
        Slot {
            room_id: RoomId::new(),
            slot_number: number,
            player_name: format!("player {number}"),
            game: "A Link to the Past".into(),
            kind,
            password: password.map(str::to_string),
            owner_id: None,
            claim_token: None,
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

    /// Three players and a spectator, which is the shape that made this rule matter.
    fn slots(password: Option<&str>) -> Vec<Slot> {
        vec![
            slot(1, password, SlotKind::Player),
            slot(2, password, SlotKind::Player),
            slot(3, password, SlotKind::Player),
            slot(4, password, SlotKind::Spectator),
        ]
    }

    #[test]
    fn a_passwordless_room_gets_only_a_token() {
        let data = build(&room(SlotAuth::None, None), &secrets(None), &slots(None)).expect("build");
        assert_eq!(data.keys().collect::<Vec<_>>(), vec!["PAHOA_ADMIN_TOKEN"]);
        // The absence is the property: an empty PAHOA_SLOT_PASSWORDS would be a locked room.
        assert!(!data.contains_key("PAHOA_SLOT_PASSWORDS"));
        assert!(!data.contains_key("PAHOA_PASSWORD"));
    }

    #[test]
    fn a_room_password_is_passed_and_no_slot_map_is() {
        let data = build(
            &room(SlotAuth::Room, Some("open-sesame")),
            &secrets(None),
            &slots(None),
        )
        .expect("build");
        assert_eq!(
            data.get("PAHOA_PASSWORD").map(String::as_str),
            Some("open-sesame")
        );
        assert!(!data.contains_key("PAHOA_SLOT_PASSWORDS"));
    }

    #[test]
    fn per_slot_renders_every_slot_including_the_spectator() {
        let data = build(
            &room(SlotAuth::PerSlot, None),
            &secrets(None),
            &slots(Some("secret")),
        )
        .expect("build");
        let raw = data.get("PAHOA_SLOT_PASSWORDS").expect("the map");
        let parsed: BTreeMap<String, String> = serde_json::from_str(raw).expect("json");

        assert_eq!(
            parsed.len(),
            4,
            "every connectable slot, spectators included"
        );
        for slot_number in ["1", "2", "3", "4"] {
            assert_eq!(parsed.get(slot_number).map(String::as_str), Some("secret"));
        }
        // Keys are strings, because JSON object keys are. Pahoa parses exactly this shape.
        assert!(raw.contains("\"4\":"), "{raw}");
        assert!(!data.contains_key("PAHOA_PASSWORD"));
    }

    /// The failure this module exists for.
    ///
    /// A map built from a player-filtered slot list leaves the spectator without a password. Under
    /// pahoa's fail-closed rule that is a spectator who cannot connect -- and before round two it
    /// would have been the one slot that could connect without one.
    #[test]
    fn a_missing_slot_password_refuses_the_build() {
        let mut slots = slots(Some("secret"));
        slots[3].password = None; // the spectator

        let err =
            build(&room(SlotAuth::PerSlot, None), &secrets(None), &slots).expect_err("must refuse");
        assert_eq!(
            err,
            SecretError::IncompleteSlotPasswords {
                count: 1,
                slots: vec![4]
            }
        );
        // The message names the slots, because "which one" is the whole question an operator has.
        assert!(err.to_string().contains("[4]"), "{err}");
    }

    /// `{}` is not an unconfigured room, it is a locked one.
    #[test]
    fn per_slot_with_no_slots_refuses_rather_than_rendering_an_empty_map() {
        let err =
            build(&room(SlotAuth::PerSlot, None), &secrets(None), &[]).expect_err("must refuse");
        assert_eq!(err, SecretError::NoSlots);
    }

    /// The database CHECK already forbids these, so reaching them means something wrote around it.
    #[test]
    fn a_mode_and_password_that_disagree_refuse_the_build() {
        assert_eq!(
            build(
                &room(SlotAuth::None, Some("stray")),
                &secrets(None),
                &slots(None)
            )
            .expect_err("none"),
            SecretError::PasswordModeMismatch { mode: "none" }
        );
        assert_eq!(
            build(&room(SlotAuth::Room, None), &secrets(None), &slots(None)).expect_err("room"),
            SecretError::PasswordModeMismatch { mode: "room" }
        );
        assert_eq!(
            build(
                &room(SlotAuth::PerSlot, Some("stray")),
                &secrets(None),
                &slots(Some("s"))
            )
            .expect_err("per_slot"),
            SecretError::PasswordModeMismatch { mode: "per_slot" }
        );
    }

    #[test]
    fn a_server_password_rides_alongside_any_mode() {
        let data = build(
            &room(SlotAuth::None, None),
            &secrets(Some("remote-admin")),
            &slots(None),
        )
        .expect("build");
        assert_eq!(
            data.get("PAHOA_SERVER_PASSWORD").map(String::as_str),
            Some("remote-admin")
        );
    }

    /// The map feeds the spec hash, so its iteration order has to be stable or every room looks
    /// changed on every tick.
    #[test]
    fn the_rendering_is_deterministic() {
        let room = room(SlotAuth::PerSlot, None);
        let slots = slots(Some("secret"));
        let a = build(&room, &secrets(None), &slots).expect("build");
        let b = build(&room, &secrets(None), &slots).expect("build");
        assert_eq!(a, b);
    }
}
