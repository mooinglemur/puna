//! Importing slot ownership from an Archipelago-lobby room.
//!
//! ## What this is for
//!
//! A generation uploaded here was usually rolled by the lobby, where every YAML already carries the
//! Discord account that submitted it. Without this an organizer opens a 200-slot room and then sends
//! 200 claim links to people the lobby could have named. So: given the lobby room the seed came
//! from, claim each Puna slot for the account that owns the matching YAML.
//!
//! ## No lobby changes were needed, and that is why it reads the way it does
//!
//! `GET /api/room/<id>` already returns, per YAML, `player_name`, `discord_id` (the snowflake, which
//! is what `room_slots.owner_id` is) and a `slot_number`. It is guarded by `LoggedInSession`, and
//! the lobby's `session.rs` accepts `X-Api-Key: <its ADMIN_TOKEN>` as an admin session — so this is
//! a read of an endpoint that exists rather than a contract anybody has to implement.
//!
//! ## The join is on the NAME, and the slot number is deliberately ignored
//!
//! Both are derived, by two different programs, and the name is the one that holds. The lobby's
//! `get_ap_player_name` is a faithful port of Archipelago's `handle_name` (`Generate.py:374`) down
//! to the `.strip()[:16].strip()`, and the lobby names the files it hands the generator after its
//! own resolved names — so the generator's counter walks the same order and lands on the same
//! strings.
//!
//! Two shapes still miss, and both miss **loudly**: `%number%`/`%player%`, which Archipelago
//! converts to braces and the lobby does not, and `{player}`, where the lobby substitutes its own
//! running count rather than the slot number. Each leaves a name that matches nothing, which is a
//! slot that keeps its claim link — exactly where it was before the import ran.
//!
//! There is one case where the name is wrong rather than absent: ten or more slots sharing a
//! templated base name sort as `Ray1, Ray10, Ray2, …`, so the generator's counter shifts from the
//! ninth on. It cannot mis-assign anybody, because the lobby refuses a second person's YAML that
//! resolves to an existing name — so a `{number}` family is always one account's, and every slot it
//! shuffles has the same owner either way.
//!
//! ## A miss is not a failure
//!
//! The import claims what it matched and reports what it did not. Refusing the whole thing on one
//! unmatched name would send an organizer back to sending a hundred claim links over two edge cases,
//! and an unmatched slot is not damaged: it still has its claim token, which is precisely the state
//! it was in a moment earlier.

use std::time::Duration;

use puna_core::artifact::SlotKind;
use puna_core::ids::RoomId;
use puna_core::model::slot::Slot;

/// Where the lobby is and what Puna presents to it.
///
/// **One lobby, from the environment.** Puna does not accept a host from a request: the URL an
/// organizer pastes is read for its room id and nothing else, so a link to somebody else's lobby
/// cannot make this tier fetch from it with our token attached. That is the same rule
/// [`crate::upstream`] follows for rooms, and for the same reason — this module holds a credential.
#[derive(Debug, Clone)]
pub struct Lobby {
    /// Base URL, e.g. `https://lobby.example.com`. No trailing slash.
    pub base: String,
    /// The lobby's own `ADMIN_TOKEN`, presented as `X-Api-Key`.
    ///
    /// **Outbound**: what Puna sends to the lobby. Not to be confused with the inbound key, which is
    /// what the lobby will send to Puna when the push lands — different secret, opposite direction.
    pub token: String,
    pub timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum LobbyError {
    #[error("no lobby is configured for this deployment")]
    NotConfigured,
    #[error("that does not look like a lobby room link")]
    NotARoomLink,
    #[error("the lobby has no room with that id")]
    NoSuchRoom,
    #[error("the lobby refused the request; check the outbound token")]
    Unauthorized,
    #[error("could not reach the lobby: {0}")]
    Unreachable(String),
    #[error("the lobby answered something this build could not read: {0}")]
    Unreadable(String),
}

/// One YAML in a lobby room, reduced to the three fields an import needs.
///
/// Deliberately **not** a mirror of the lobby's `YamlInfo`: it also carries the game, a handle, a
/// patch flag and timestamps, none of which Puna has any business storing about somebody else's
/// system. Fewer fields is also fewer things to break when the lobby adds one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct LobbyYaml {
    pub player_name: String,
    pub discord_id: i64,
}

impl Lobby {
    /// Read the room id out of whatever the organizer pasted.
    ///
    /// A full URL or a bare uuid, because both are things somebody genuinely has in hand — the
    /// address bar of the lobby room they were just looking at, or an id copied out of it. The
    /// **host is discarded either way**: only the id travels, and the request goes to the
    /// configured lobby. So pasting a link to a lobby Puna does not know about fetches the same id
    /// from the lobby it does know about, which either 404s or is the room they meant.
    pub fn room_id_from(pasted: &str) -> Result<uuid::Uuid, LobbyError> {
        let trimmed = pasted.trim().trim_end_matches('/');

        // The last path segment, or the whole thing when it is already bare.
        let candidate = trimmed.rsplit('/').next().unwrap_or(trimmed);
        // A query string on a pasted URL is ordinary; strip it rather than failing on it.
        let candidate = candidate.split(['?', '#']).next().unwrap_or(candidate);

        uuid::Uuid::parse_str(candidate).map_err(|_| LobbyError::NotARoomLink)
    }

    /// Fetch a lobby room's YAML list.
    pub async fn yamls(&self, room: uuid::Uuid) -> Result<Vec<LobbyYaml>, LobbyError> {
        // **Built from the configured base and a uuid**, never from anything a request supplied as
        // text. `room` has already been through `Uuid::parse_str`, so it cannot carry a path.
        let url = format!("{}/api/room/{room}", self.base.trim_end_matches('/'));

        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| LobbyError::Unreachable(e.to_string()))?;

        let response = client
            .get(&url)
            .header("X-Api-Key", &self.token)
            .send()
            .await
            .map_err(|e| LobbyError::Unreachable(e.to_string()))?;

        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err(LobbyError::Unauthorized),
            404 => return Err(LobbyError::NoSuchRoom),
            other => {
                return Err(LobbyError::Unreachable(format!(
                    "the lobby answered {other}"
                )));
            }
        }

        // **Only the `yamls` array is parsed, and only three fields of it.** The response also
        // carries the room's URL and its live `host:port`, which are the lobby's secrets to keep.
        // Reading past what is needed is how a field nobody meant to store ends up in a log.
        #[derive(serde::Deserialize)]
        struct RoomInfo {
            yamls: Vec<LobbyYaml>,
        }

        let body: RoomInfo = response
            .json()
            .await
            .map_err(|e| LobbyError::Unreadable(e.to_string()))?;

        Ok(body.yamls)
    }
}

/// What an import would do, worked out before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// `(slot_number, discord_id)` for every slot this import will claim.
    pub claims: Vec<(i32, i64)>,
    /// Puna slots left unclaimed: the lobby named nobody for them.
    ///
    /// **Player names, not slot numbers**, because this is what an organizer is shown and a name is
    /// what they will look for in the lobby.
    pub unmatched: Vec<String>,
    /// Lobby YAMLs that named no slot in this room.
    ///
    /// Usually the sign that the wrong lobby room was associated, which is the one mistake here that
    /// looks like success — every slot unmatched and every yaml unused.
    pub unused: usize,
}

/// Work out the assignment. **Pure**, so every rule below is testable without a lobby.
///
/// Three things it will not do:
///
/// * **Touch a slot that already has an owner.** Backfill is re-runnable and must never take a slot
///   off somebody who claimed it in the meantime — including on the first run, where a player may
///   have used their claim link between the room opening and the organizer pressing the button.
/// * **Claim a spectator.** They are connectable slots and they can be claimed, but the lobby's
///   yamls are players; a spectator matching a yaml name would be a coincidence of naming.
/// * **Match case-insensitively.** Archipelago's own uniqueness rule is case-insensitive, so two
///   slots cannot differ by case alone — but the generator's output is the authority on the exact
///   string, and loosening the comparison would only ever paper over a divergence worth seeing.
pub fn plan(roster: &[Slot], yamls: &[LobbyYaml]) -> Plan {
    let mut claims = Vec::new();
    let mut unmatched = Vec::new();
    let mut used = std::collections::HashSet::new();

    for slot in roster {
        if slot.kind == SlotKind::Spectator {
            continue;
        }
        if slot.owner_id.is_some() {
            continue;
        }

        match yamls.iter().find(|y| y.player_name == slot.player_name) {
            Some(yaml) => {
                claims.push((slot.slot_number, yaml.discord_id));
                used.insert(yaml.player_name.as_str());
            }
            None => unmatched.push(slot.player_name.clone()),
        }
    }

    Plan {
        claims,
        unmatched,
        unused: yamls
            .iter()
            .filter(|y| !used.contains(y.player_name.as_str()))
            .count(),
    }
}

/// What an import actually did, for the sentence the organizer is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub claimed: usize,
    /// Planned, then not claimed, because somebody took the slot between the read and the write.
    ///
    /// Its own count rather than folded into `claimed`, because the two mean different things to an
    /// organizer: one is the lobby's answer landing, the other is a player who beat it to it — and
    /// the second is not a problem to investigate.
    pub taken_first: usize,
    pub unmatched: Vec<String>,
    pub unused: usize,
}

/// Fetch, match, and claim. **The whole import, and the only path that writes owners.**
///
/// Creation-time import and the options page's backfill are the same call. That is deliberate: the
/// creation case is this run once and automatically, so a lobby that is down at that moment costs
/// nothing — the room opens and the organizer presses the button, through code that has already
/// been exercised.
pub async fn import(
    conn: &mut diesel_async::AsyncPgConnection,
    lobby: &Lobby,
    room: RoomId,
    lobby_room: uuid::Uuid,
) -> anyhow::Result<Imported> {
    let yamls = lobby.yamls(lobby_room).await?;
    let roster = puna_core::model::slot::list(conn, room).await?;
    let plan = plan(&roster, &yamls);

    // **Rows first, and for every owner, before any slot points at one.** `room_slots.owner_id`
    // references `users`, so a slot claimed for an account that has never signed in would be a
    // foreign-key violation surfacing as a 500 on an otherwise correct import. The placeholder name
    // is what the roster renders as "never logged in" until they do.
    for (_, owner) in &plan.claims {
        puna_core::model::user::ensure_exists(conn, *owner).await?;
    }

    let claimed = puna_core::model::slot::claim_for_owners(conn, room, &plan.claims).await?;

    Ok(Imported {
        claimed,
        taken_first: plan.claims.len() - claimed,
        unmatched: plan.unmatched,
        unused: plan.unused,
    })
}

impl Imported {
    /// The sentence an organizer reads. Plain counts, and it names the leftovers.
    pub fn message(&self) -> String {
        let mut parts = vec![match self.claimed {
            0 => "No slots were claimed from the lobby".to_string(),
            1 => "Claimed 1 slot from the lobby".to_string(),
            n => format!("Claimed {n} slots from the lobby"),
        }];

        if self.taken_first > 0 {
            parts.push(format!(
                "{} had already been claimed by their player",
                self.taken_first
            ));
        }
        if !self.unmatched.is_empty() {
            // **Named, not counted, up to a point.** An organizer's next move is to find these
            // players in the lobby, and a bare number sends them to compare two lists by hand.
            let shown: Vec<&str> = self.unmatched.iter().take(5).map(String::as_str).collect();
            let rest = self.unmatched.len().saturating_sub(shown.len());
            parts.push(match rest {
                0 => format!("no lobby YAML matched {}", shown.join(", ")),
                n => format!("no lobby YAML matched {} and {n} more", shown.join(", ")),
            });
        }
        if self.unused > 0 {
            parts.push(format!(
                "{} lobby YAML(s) matched no slot here",
                self.unused
            ));
        }

        format!("{}.", parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use puna_core::ids::{RoomId, TrackerId};

    fn slot(number: i32, name: &str, owner: Option<i64>, kind: SlotKind) -> Slot {
        Slot {
            room_id: RoomId::new(),
            slot_number: number,
            player_name: name.into(),
            game: "A Link to the Past".into(),
            kind,
            password: None,
            owner_id: owner,
            claim_token: Some("a-claim-token".into()),
            claimed_at: None,
            tracker_id: TrackerId::new(),
            locked_at: None,
            locked_by: None,
        }
    }

    fn yaml(name: &str, id: i64) -> LobbyYaml {
        LobbyYaml {
            player_name: name.into(),
            discord_id: id,
        }
    }

    #[test]
    fn a_pasted_link_or_a_bare_id_both_resolve() {
        let id = "6f0e1f7e-2b3c-4d5e-8f90-a1b2c3d4e5f6";

        for pasted in [
            id,
            &format!("https://lobby.example.com/room/{id}"),
            &format!("https://lobby.example.com/room/{id}/"),
            &format!("https://lobby.example.com/room/{id}?from=discord"),
            &format!("  https://lobby.example.com/room/{id}  "),
        ] {
            assert_eq!(
                Lobby::room_id_from(pasted).expect("a room id"),
                uuid::Uuid::parse_str(id).unwrap(),
                "{pasted}"
            );
        }

        for bad in ["", "https://lobby.example.com/", "not-a-uuid", "/room/"] {
            assert!(Lobby::room_id_from(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// The ordinary case, and the two kinds of leftover an organizer needs told apart.
    #[test]
    fn the_plan_claims_matches_and_reports_both_kinds_of_leftover() {
        let roster = [
            slot(1, "Troy", None, SlotKind::Player),
            slot(2, "Alice", None, SlotKind::Player),
            slot(3, "Ray%number%", None, SlotKind::Player),
        ];
        let yamls = [yaml("Troy", 7), yaml("Alice", 8), yaml("Ray1", 9)];

        let plan = plan(&roster, &yamls);

        assert_eq!(plan.claims, vec![(1, 7), (2, 8)]);
        assert_eq!(
            plan.unmatched,
            vec!["Ray%number%".to_string()],
            "a name the generator expanded and the lobby did not"
        );
        assert_eq!(plan.unused, 1, "the lobby's Ray1 named no slot here");
    }

    /// **Re-runnable, and it must never take a slot back.** Between the room opening and an
    /// organizer pressing backfill, a player may have used their claim link — and the lobby's answer
    /// is older than that.
    #[test]
    fn a_slot_that_already_has_an_owner_is_never_touched() {
        let roster = [
            slot(1, "Troy", Some(99), SlotKind::Player),
            slot(2, "Alice", None, SlotKind::Player),
        ];
        let yamls = [yaml("Troy", 7), yaml("Alice", 8)];

        let plan = plan(&roster, &yamls);

        assert_eq!(
            plan.claims,
            vec![(2, 8)],
            "Troy is claimed and stays claimed"
        );
        assert!(
            plan.unmatched.is_empty(),
            "an owned slot is not an unmatched one"
        );
    }

    /// A spectator is claimable but is not a lobby yaml, so a name collision must not claim it.
    #[test]
    fn a_spectator_is_not_claimed_from_the_lobby() {
        let roster = [
            slot(1, "Troy", None, SlotKind::Player),
            slot(2, "Watcher", None, SlotKind::Spectator),
        ];
        let yamls = [yaml("Troy", 7), yaml("Watcher", 8)];

        let plan = plan(&roster, &yamls);

        assert_eq!(plan.claims, vec![(1, 7)]);
        assert!(
            plan.unmatched.is_empty(),
            "a spectator is neither claimed nor reported as a miss"
        );
    }

    /// The wrong lobby room associated: every slot unmatched, every yaml unused. It is the one
    /// mistake here that otherwise reads as "this seed just did not come from the lobby".
    #[test]
    fn associating_the_wrong_room_reports_every_yaml_unused() {
        let roster = [slot(1, "Troy", None, SlotKind::Player)];
        let yamls = [yaml("Someone", 7), yaml("Else", 8)];

        let plan = plan(&roster, &yamls);

        assert!(plan.claims.is_empty());
        assert_eq!(plan.unmatched, vec!["Troy".to_string()]);
        assert_eq!(plan.unused, 2);
    }
}
