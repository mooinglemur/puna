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
    /// The lobby room was made by somebody who has no standing in this room.
    ///
    /// **This is what stops the import being a way to read a stranger's lobby room.** Without it,
    /// anyone who can open a room here could point it at any lobby room id and pull that room's
    /// player names and Discord accounts into their own roster.
    ///
    /// The message deliberately names nobody: the reader may not be entitled to know who created
    /// that lobby room, and if they are, they already do.
    #[error(
        "the person who created that lobby room is not an organizer of this room. Add them as an \
         organizer, or ask them to run the import."
    )]
    AuthorIsNotAnOrganizer,
}

/// A lobby room, reduced to what an import needs: who made it, and who owns each YAML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LobbyRoom {
    /// The Discord account that created the room in the lobby.
    ///
    /// **`-1` is the lobby's sentinel for "nobody"**, not a user id — `db/room.rs` uses it where the
    /// column is null and the API flattens `Option<i64>` to a bare number on the way out. Treated as
    /// absent everywhere here, which matters because it would otherwise be a perfectly valid
    /// argument to `user::ensure_exists`.
    pub author_id: i64,
    pub yamls: Vec<LobbyYaml>,
}

impl LobbyRoom {
    /// The author, or `None` where the lobby recorded nobody.
    pub fn author(&self) -> Option<i64> {
        (self.author_id >= 0).then_some(self.author_id)
    }
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

    /// Fetch a lobby room: its author, and its YAML list.
    pub async fn room(&self, room: uuid::Uuid) -> Result<LobbyRoom, LobbyError> {
        // **Built from the configured base and a uuid**, never from anything a request supplied as
        // text. `room` has already been through `Uuid::parse_str`, so it cannot carry a path.
        let url = format!("{}/api/room/{room}", self.base.trim_end_matches('/'));

        // --- REDIRECTS ARE NOT FOLLOWED, AND THAT IS A CREDENTIAL DECISION ------------------------
        //
        // reqwest follows up to ten by default, and it strips only the headers it knows are
        // sensitive -- `Authorization`, `Cookie`, `Proxy-Authorization`, `WWW-Authenticate`. A
        // custom `X-Api-Key` is not on that list, so it is re-sent to whatever host the chain
        // reaches. This token is the lobby's own ADMIN_TOKEN, which grants full admin there.
        //
        // Not hypothetical, and not specific to one environment -- **both lobbies do this, and a
        // WRONG key is treated exactly like no key at all.** Verified 2026-08-28 against both:
        // `/api/room/<id>` answers `303` to `/auth/login`, which answers `303` to
        // `https://discord.com/oauth2/authorize`. So an unsynced token walked our lobby admin token
        // out to discord.com, which the web tier's NetworkPolicy already permits it to reach for
        // OAuth -- there was not even a connection refusal to stop it.
        //
        // Following also destroyed the diagnosis, which is the half that was already observed: the
        // lobby never gets to say "refused", Discord answers `200` with HTML, `.json()` fails, and
        // the organizer is told the lobby returned something unreadable.
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
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
            // A redirect on this endpoint is the lobby sending an unauthenticated caller to log in,
            // so it means the same thing a `401` means and is reported as such. Reading it as a
            // transport fault would point an organizer at the lobby being down when the answer is
            // that PUNA_LOBBY_OUTBOUND_TOKEN does not match the lobby's ADMIN_TOKEN.
            401 | 403 | 301..=308 => return Err(LobbyError::Unauthorized),
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
            author_id: i64,
            yamls: Vec<LobbyYaml>,
        }

        let body: RoomInfo = response
            .json()
            .await
            .map_err(|e| LobbyError::Unreadable(e.to_string()))?;

        Ok(LobbyRoom {
            author_id: body.author_id,
            yamls: body.yamls,
        })
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
    /// Slots the lobby named that already had an owner, so there was nothing to do.
    ///
    /// **Its own bucket, because folding it into `unused` said something untrue.** A yaml whose slot
    /// is already claimed matched perfectly; reporting it as "matched no slot here" told an organizer
    /// to go looking for a mismatch that does not exist — and in the ordinary case, a room where
    /// people have been claiming their own slots, it described *most* of the roster that way.
    pub already_claimed: usize,
    /// Lobby YAMLs that named no slot in this room.
    ///
    /// Usually the sign that the wrong lobby room was associated, which is the one mistake here that
    /// looks like success — every slot unmatched and every yaml unused. That signal only works if
    /// this bucket means what it says, which is why `already_claimed` is separate.
    pub unused: usize,
}

/// Work out the assignment. **Pure**, so every rule below is testable without a lobby.
///
/// Two things it will not do:
///
/// * **Touch a slot that already has an owner.** Backfill is re-runnable and must never take a slot
///   off somebody who claimed it in the meantime — including on the first run, where a player may
///   have used their claim link between the room opening and the organizer pressing the button.
///   That slot is counted under `already_claimed`, not treated as if the lobby had never named it.
/// * **Match case-insensitively.** Archipelago's own uniqueness rule is case-insensitive, so two
///   slots cannot differ by case alone — but the generator's output is the authority on the exact
///   string, and loosening the comparison would only ever paper over a divergence worth seeing.
///
/// **Spectators are claimed like anybody else**, which reverses an earlier rule here. The argument
/// for skipping them was that the lobby's yamls are players, so a spectator matching one would be a
/// coincidence of naming — and that is simply not how the two systems work. A spectator slot exists
/// because somebody submitted a yaml for it, the lobby names that account, and everything downstream
/// already treats a spectator as an ordinary connectable slot: it takes an owner, a claim link, a
/// per-slot password and a tracker id like any other. Skipping it left the one slot the organizer
/// most wanted filled as the only one still holding a claim link.
///
/// **A yaml is `used` if it matched a slot at all**, claimed or already owned. Marking only the
/// claimed ones is what made a fully-claimed room report every yaml as matching nothing.
pub fn plan(roster: &[Slot], yamls: &[LobbyYaml]) -> Plan {
    let mut claims = Vec::new();
    let mut unmatched = Vec::new();
    let mut already_claimed = 0;
    let mut used = std::collections::HashSet::new();

    for slot in roster {
        match yamls.iter().find(|y| y.player_name == slot.player_name) {
            Some(yaml) => {
                // Marked used before the ownership branch, deliberately: the question this answers
                // is "did the lobby name a slot in this room", and it did either way.
                used.insert(yaml.player_name.as_str());

                if slot.owner_id.is_some() {
                    already_claimed += 1;
                } else {
                    claims.push((slot.slot_number, yaml.discord_id));
                }
            }
            // A slot nobody has claimed and the lobby cannot name. A slot that is already owned and
            // matches nothing is not reported at all — there is nothing for an organizer to do about
            // a slot that is already where it needs to be.
            None if slot.owner_id.is_none() => unmatched.push(slot.player_name.clone()),
            None => {}
        }
    }

    Plan {
        claims,
        unmatched,
        already_claimed,
        unused: yamls
            .iter()
            .filter(|y| !used.contains(y.player_name.as_str()))
            .count(),
    }
}

/// May this import proceed?
///
/// **Pure, because the check itself cannot be reached in a test.** Everything around it needs a live
/// lobby answering an HTTP request, so the rule lives here where a truth table can hold it and
/// `import` is the only caller.
///
/// Three inputs and one decision:
///
/// * **A site admin passes regardless.** They can already read every room here and, holding the
///   outbound token, every room there — so the gate would withhold nothing from them.
/// * **No author fails.** The lobby records `-1` where a room has none, and `author()` has already
///   turned that into `None`; an absent author is nobody to have standing.
/// * **Otherwise the author must be an ORGANIZER**, not merely a member. A helper is trusted to run
///   this room, not to decide which lobby room it is bound to — and binding is what hands a
///   stranger's player list to this roster.
fn may_import(
    is_admin: bool,
    author: Option<i64>,
    author_role: Option<puna_core::model::member::RoomRole>,
) -> bool {
    use puna_core::model::member::RoomRole;

    if is_admin {
        return true;
    }
    author.is_some() && author_role.is_some_and(|role| role >= RoomRole::Organizer)
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
    /// Slots the lobby named that already had an owner before this ran. See [`Plan`].
    pub already_claimed: usize,
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
    is_admin: bool,
) -> anyhow::Result<Imported> {
    let fetched = lobby.room(lobby_room).await?;

    // **The lobby room's author must be an organizer here.**
    //
    // Otherwise the import is a way to read somebody else's lobby room: paste any room id and its
    // players' names and Discord accounts arrive in your roster. Tying the two rooms together
    // requires standing in both, and the lobby's author is the only identity its API offers to
    // check against.
    //
    // **Inside `import` rather than in the two routes**, so neither can forget it and a third
    // caller inherits it. A site admin bypasses it entirely, which is the one exception: they can
    // already read every room here and every room there.
    let author_role = match fetched.author() {
        Some(author) => puna_core::model::member::role_of(conn, room, author).await?,
        None => None,
    };
    if !may_import(is_admin, fetched.author(), author_role) {
        return Err(LobbyError::AuthorIsNotAnOrganizer.into());
    }

    let roster = puna_core::model::slot::list(conn, room).await?;
    let plan = plan(&roster, &fetched.yamls);

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
        already_claimed: plan.already_claimed,
        unused: plan.unused,
    })
}

impl Imported {
    /// The sentence an organizer reads. Plain counts, and it names the leftovers.
    ///
    /// **Every clause has to be true of the room, not just of this run.** The first version read
    /// "No slots were claimed from the lobby; 4 lobby YAML(s) matched no slot here" about a room
    /// where all four matched and three were already claimed — so the two facts it stated were the
    /// two an organizer would act on, and both were wrong. The clauses below are ordered by what
    /// somebody wants to know: what changed, what was already fine, and what still needs a person.
    pub fn message(&self) -> String {
        let mut parts = vec![match self.claimed {
            0 => "No slots were claimed from the lobby".to_string(),
            1 => "1 slot was claimed from the lobby".to_string(),
            n => format!("{n} slots were claimed from the lobby"),
        }];

        // Not a problem, and said plainly so it does not read as one. On a re-run, or on a room
        // where people have been using their claim links, this is most of the roster.
        if self.already_claimed > 0 {
            parts.push(match self.already_claimed {
                1 => "1 matching slot already had a claim".to_string(),
                n => format!("{n} matching slots already had claims"),
            });
        }
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
            parts.push(match self.unused {
                1 => "1 lobby YAML matched no slot here".to_string(),
                n => format!("{n} lobby YAMLs matched no slot here"),
            });
        }

        format!("{}.", parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `SlotKind` is a fixture concern now, not a rule: `plan` stopped branching on it when
    // spectators became claimable, and the tests are the only thing that still needs to build one.
    use puna_core::artifact::SlotKind;
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

    /// **The gate that stops the import being a way to read somebody else's lobby room.**
    ///
    /// Without it, anyone who can open a room here could paste any lobby room id and pull that
    /// room's player names and Discord accounts into their own roster. Binding two rooms together
    /// should require standing in both, and the lobby's `author_id` is the only identity its API
    /// offers to check against.
    ///
    /// Note what this means at CREATION time: a new room's only organizer is whoever created it, so
    /// the rule reduces to "you must be the lobby room's author, or an admin". A colleague opening
    /// the room adds the author as an organizer first and then backfills from the options page,
    /// which is the flow the refusal message names.
    #[test]
    fn only_an_organizer_of_both_rooms_may_bind_them() {
        use puna_core::model::member::RoomRole;

        assert!(may_import(false, Some(7), Some(RoomRole::Organizer)));

        assert!(
            !may_import(false, Some(7), Some(RoomRole::Helper)),
            "a helper is trusted to run this room, not to decide which lobby room it is bound to"
        );
        assert!(
            !may_import(false, Some(7), None),
            "the lobby room's author has no standing here at all"
        );

        // The lobby writes -1 where a room has no author; `author()` turns that into None, and an
        // absent author is nobody to have standing.
        assert!(!may_import(false, None, None));
        assert!(
            !may_import(false, None, Some(RoomRole::Organizer)),
            "a role with no author behind it must not pass"
        );

        // A site admin already reads every room on both sides, so the gate withholds nothing.
        for (author, role) in [
            (None, None),
            (Some(7), None),
            (Some(7), Some(RoomRole::Helper)),
        ] {
            assert!(may_import(true, author, role), "an admin is not gated");
        }
    }

    /// `-1` is the lobby's "no author", not a user id -- and it would be a perfectly valid argument
    /// to `user::ensure_exists`, which is what makes reading it as one worth preventing here.
    #[test]
    fn the_lobbys_no_author_sentinel_is_not_a_user() {
        let room = |author_id| LobbyRoom {
            author_id,
            yamls: Vec::new(),
        };

        assert_eq!(room(7).author(), Some(7));
        assert_eq!(room(-1).author(), None);
        assert_eq!(room(0).author(), Some(0), "only negatives are the sentinel");
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

    /// A spectator is an ordinary connectable slot everywhere else in Puna, and the lobby knows who
    /// submitted its yaml, so there is nothing to withhold.
    #[test]
    fn a_spectator_is_claimed_like_anybody_else() {
        let roster = [
            slot(1, "Troy", None, SlotKind::Player),
            slot(2, "Watcher", None, SlotKind::Spectator),
        ];
        let yamls = [yaml("Troy", 7), yaml("Watcher", 8)];

        let plan = plan(&roster, &yamls);

        assert_eq!(
            plan.claims,
            vec![(1, 7), (2, 8)],
            "a spectator slot exists because somebody submitted a yaml for it, and the lobby names \
             the account that did"
        );
        assert!(plan.unmatched.is_empty());
        assert_eq!(plan.unused, 0);
    }

    /// The reported case, end to end: four slots, all four named by the lobby, three already
    /// claimed, and the fourth a spectator.
    ///
    /// It produced *"No slots were claimed from the lobby; 4 lobby YAML(s) matched no slot here"* —
    /// both clauses false, and the one slot that needed claiming was the one deliberately skipped.
    #[test]
    fn a_mostly_claimed_room_claims_the_rest_and_says_so_truthfully() {
        let roster = [
            slot(1, "Troy", Some(7), SlotKind::Player),
            slot(2, "Ray", Some(8), SlotKind::Player),
            slot(3, "Mira", Some(9), SlotKind::Player),
            slot(4, "Watcher", None, SlotKind::Spectator),
        ];
        let yamls = [
            yaml("Troy", 7),
            yaml("Ray", 8),
            yaml("Mira", 9),
            yaml("Watcher", 10),
        ];

        let plan = plan(&roster, &yamls);

        assert_eq!(plan.claims, vec![(4, 10)]);
        assert_eq!(plan.already_claimed, 3);
        assert_eq!(
            plan.unused, 0,
            "every yaml named a slot here; none of them matched nothing"
        );
        assert!(plan.unmatched.is_empty());

        let imported = Imported {
            claimed: 1,
            taken_first: 0,
            unmatched: plan.unmatched,
            already_claimed: plan.already_claimed,
            unused: plan.unused,
        };
        assert_eq!(
            imported.message(),
            "1 slot was claimed from the lobby; 3 matching slots already had claims."
        );
    }

    /// The signal `unused` exists for, still intact: a genuinely unrelated lobby room reports every
    /// yaml as matching nothing. It only means that if an already-claimed slot does NOT land here.
    #[test]
    fn an_already_claimed_slot_is_never_reported_as_matching_nothing() {
        let roster = [slot(1, "Troy", Some(7), SlotKind::Player)];

        let claimed_elsewhere = plan(&roster, &[yaml("Troy", 7)]);
        assert_eq!(claimed_elsewhere.unused, 0);
        assert_eq!(claimed_elsewhere.already_claimed, 1);

        let wrong_room = plan(&roster, &[yaml("Somebody", 7)]);
        assert_eq!(wrong_room.unused, 1);
        assert_eq!(wrong_room.already_claimed, 0);
        assert!(
            wrong_room.unmatched.is_empty(),
            "an owned slot the lobby cannot name needs nothing from an organizer"
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

    /// **A redirect is a refusal, and it must not be followed.**
    ///
    /// Both lobbies answer `/api/room/<id>` with `303` to `/auth/login` when the `X-Api-Key` is
    /// wrong or absent — a bad key is treated exactly like no key — and that login redirects on to
    /// `https://discord.com/oauth2/authorize`. reqwest follows redirects by default and strips only
    /// the headers it knows are sensitive, which does not include a custom `X-Api-Key`, so the
    /// lobby's own ADMIN_TOKEN was being re-sent along the chain.
    ///
    /// Asserted at the transport rather than as a source lint, because both halves of the failure
    /// are reachable here: without `Policy::none()` the client follows to the second endpoint,
    /// parses its HTML as JSON, and reports `Unreadable` — so an unsynced credential presented as
    /// the lobby returning something broken.
    ///
    /// `std::net` on a thread rather than `tokio::net`, deliberately: the workspace's tokio does not
    /// declare the `net` feature, and depending on another crate enabling it is the same
    /// feature-unification trap the rustls provider already cost this project once.
    #[tokio::test]
    async fn a_redirect_is_reported_as_a_refusal_and_never_followed() {
        use std::io::{Read, Write};

        // What startup does by way of the database pool. Already-installed is success, since the
        // whole binary's tests share a process.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));

        // Two responses: the refusal, then what a follower would land on. The second exists so the
        // test fails LOUDLY rather than by timing out when the policy is removed.
        let server = std::thread::spawn(move || {
            for body in ["303 See Other", "200 OK"] {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = if body.starts_with("303") {
                    "HTTP/1.1 303 See Other\r\nLocation: /auth/login\r\nContent-Length: 0\r\n\r\n"
                        .to_string()
                } else {
                    let html = "<!doctype html><html>sign in</html>";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{html}",
                        html.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let lobby = Lobby {
            base,
            token: "not-the-lobbys-admin-token".into(),
            timeout: Duration::from_secs(5),
        };

        let error = lobby
            .room(uuid::Uuid::nil())
            .await
            .expect_err("a 303 is not a room");

        assert!(
            matches!(error, LobbyError::Unauthorized),
            "a redirect must read as a refused credential, got {error:?}"
        );

        drop(server);
    }
}
