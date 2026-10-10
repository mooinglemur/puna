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
//! the lobby's `session.rs` accepts `X-Api-Key: <its ADMIN_TOKEN>` as an admin session, so this is
//! a read of an endpoint that exists rather than a contract anybody has to implement.
//!
//! ## The join is on the NAME, and the slot number is deliberately ignored
//!
//! Both are derived, by two different programs, and the name is the one that holds.
//!
//! **The lobby sends the raw yaml name and the generator's is cut to sixteen characters**, so the
//! two agree for every name that fits and for no name that does not. `/api/room/<id>` serves
//! `yamls.player_name`, which is the `name:` field exactly as submitted; the lobby does have an
//! Archipelago-resolved name (`get_ap_player_name`, a faithful port of `handle_name` down to the
//! `.strip()[:16].strip()`), but it computes that for its own room page and does not put it on the
//! wire. A player submitting `betterthanyou_Pupupu` is `betterthanyou_Pu` in the seed, and this
//! read it as a name the lobby had never heard of.
//!
//! So the match runs in passes: on the strings as they stand, then on whatever is left over against
//! [`ap_name`], which is the generator's own cut. Every pass takes a yaml **only where it is the one
//! candidate**: two names cut to one string is a question for a person rather than a coin toss. It
//! should never arise, because the lobby refuses a yaml whose resolved name collides with one
//! already in the room, but [`plan`] is pure and matches whatever list it is handed.
//!
//! ## Two kinds of name the lobby never held, and both are recoverable
//!
//! A slot's name in the seed is not always a name the lobby ever saw, and neither case is exotic:
//! a 275-slot room reported against this carried six of them.
//!
//! **A `triggers` block can rename the slot.** A yaml whose `game` is a weighted list of several
//! games usually carries one trigger per game that renames the slot to say which rolled, because
//! that is how everybody tells which game a slot is. `roll_triggers` runs *before* `ret.name` is
//! read (`Generate.py`, lines 547 and 613) and `update_weights` filters no keys, so the name in the
//! seed is whichever the roll chose. [`trigger_names`] reads the candidates out of the yaml, which
//! costs one extra request per unmatched yaml and none at all for a room whose names line up.
//!
//! **A `{number}` or `{player}` template is substituted.** [`resolved_name`] is the whole of
//! `handle_name`, so these are reconstructed rather than guessed at: `{player}` is the generator's
//! player number, and a slot's number IS that player number, so the question asked is "would this
//! yaml, generated as THIS slot, have been called this?". The name counter behind `{number}` needs
//! the yamls in the generator's order, which is the one thing `slot_number` is read for.
//!
//! **Both are decided by name equality, never by position**, and that is what makes them safe to
//! claim on. A room whose yamls were edited between the lobby download and an offline generation
//! produces names that do not match, and those slots stay unclaimed exactly as they do today.
//! Nothing is ever assigned because it was nearby.
//!
//! What still misses: a yaml edited after it was downloaded, a trigger condition this cannot
//! evaluate without the generator, and any name two yamls could both carry. Each leaves a slot
//! holding its claim link, which is where it was before the import ran.
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
/// [`crate::upstream`] follows for rooms, and for the same reason: this module holds a credential.
#[derive(Debug, Clone)]
pub struct Lobby {
    /// Base URL, e.g. `https://lobby.example.com`. No trailing slash.
    pub base: String,
    /// The lobby's own `ADMIN_TOKEN`, presented as `X-Api-Key`.
    ///
    /// **Outbound**: what Puna sends to the lobby. Not to be confused with the inbound key, which is
    /// what the lobby will send to Puna when the push lands: different secret, opposite direction.
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
    /// **`-1` is the lobby's sentinel for "nobody"**, not a user id: `db/room.rs` uses it where the
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
    /// The lobby's id for this yaml, which is how its CONTENT is fetched.
    ///
    /// Needed only for the yamls that matched nothing by name, so a room where every name lines up
    /// costs no extra request at all. See [`Lobby::yaml_content`].
    pub id: uuid::Uuid,
    /// Which slot the lobby expects this yaml to become, one-based.
    ///
    /// **A prediction, and a principled one.** The lobby's `get_slots` names each yaml
    /// `<sanitized_name>.yaml`, deduplicates collisions with a `_N` suffix, and sorts the result
    /// case-insensitively; Archipelago assigns player numbers by sorting its weights cache on the
    /// case-folded file path (`Generate.py:158`) and walking each file's documents in order, and
    /// the lobby stores one row per document. So the two orders agree whenever the generator was
    /// handed the files the lobby produced, which it was: that is what the lobby's download is for.
    ///
    /// **It is not authority, and nothing here treats it as such.** An organizer who added or
    /// removed a yaml between downloading and generating offline has shifted every number past the
    /// edit, and the lobby cannot know. So this is used for exactly one thing: ordering the name
    /// counter in [`resolved_name`], which needs to know how many same-named yamls came first. A
    /// claim is never made on position. See the module docs.
    pub slot_number: i32,
    /// Every other name this yaml could have been given, from its `triggers`.
    ///
    /// **Not from the API**, which is why it is `skip`ped rather than deserialized: it comes from a
    /// second request per yaml, made only for the ones that matched nothing, and is empty for every
    /// yaml on the ordinary path. [`plan`] treats an empty list as "no alternatives known", which is
    /// also exactly what a yaml with no triggers has.
    #[serde(skip)]
    pub alternate_names: Vec<String>,
}

impl Lobby {
    /// Read the room id out of whatever the organizer pasted.
    ///
    /// A full URL or a bare uuid, because both are things somebody genuinely has in hand: the
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
        // sensitive: `Authorization`, `Cookie`, `Proxy-Authorization`, `WWW-Authenticate`. A
        // custom `X-Api-Key` is not on that list, so it is re-sent to whatever host the chain
        // reaches. This token is the lobby's own ADMIN_TOKEN, which grants full admin there.
        //
        // Not hypothetical, and not specific to one environment: **both lobbies do this, and a
        // WRONG key is treated exactly like no key at all.** Verified 2026-08-28 against both:
        // `/api/room/<id>` answers `303` to `/auth/login`, which answers `303` to
        // `https://discord.com/oauth2/authorize`. So an unsynced token walked our lobby admin token
        // out to discord.com, which the web tier's NetworkPolicy already permits it to reach for
        // OAuth, and there was not even a connection refusal to stop it.
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

    /// One yaml's text, for reading its `triggers`.
    ///
    /// **Fetched per yaml and only for the ones that matched nothing**, which is what keeps this
    /// affordable: a 275-slot room whose names all line up makes zero of these requests, and the
    /// rooms that need any need one or two. Fetching the set up front would be 275 requests to
    /// answer a question about three of them.
    ///
    /// Same client policy as [`Lobby::room`], and for the same reason: this carries the lobby's
    /// admin token, redirects are not followed, and a redirect means "log in" rather than "moved".
    /// The route it calls happens to have no session guard on the lobby side, so the token is not
    /// strictly needed here, but sending it is what makes this work the day that changes.
    ///
    /// Both ids have been through `Uuid::parse_str`, so neither can carry a path segment.
    pub async fn yaml_content(
        &self,
        room: uuid::Uuid,
        yaml: uuid::Uuid,
    ) -> Result<String, LobbyError> {
        let url = format!(
            "{}/api/room/{room}/download/{yaml}",
            self.base.trim_end_matches('/')
        );

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
            401 | 403 | 301..=308 => return Err(LobbyError::Unauthorized),
            404 => return Err(LobbyError::NoSuchRoom),
            other => {
                return Err(LobbyError::Unreachable(format!(
                    "the lobby answered {other}"
                )));
            }
        }

        response
            .text()
            .await
            .map_err(|e| LobbyError::Unreadable(e.to_string()))
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
    /// to go looking for a mismatch that does not exist, and in the ordinary case, a room where
    /// people have been claiming their own slots, it described *most* of the roster that way.
    pub already_claimed: usize,
    /// Lobby YAMLs that named no slot in this room.
    ///
    /// Usually the sign that the wrong lobby room was associated, which is the one mistake here that
    /// looks like success: every slot unmatched and every yaml unused. That signal only works if
    /// this bucket means what it says, which is why `already_claimed` is separate.
    ///
    /// **By id rather than a count, so a caller can act on it.** `import` reads this to decide
    /// which yamls are worth a second request for their `triggers`: a yaml that named a slot is
    /// spent and has nothing left to tell anybody. The count an organizer is shown is
    /// [`Plan::unused`], which is this length, so the two cannot disagree.
    pub unused_ids: Vec<uuid::Uuid>,
}

impl Plan {
    /// How many lobby YAMLs named no slot here.
    pub fn unused(&self) -> usize {
        self.unused_ids.len()
    }
}

/// What the generator would have called a yaml, once it cut the name to size.
///
/// Archipelago's `handle_name` ends `new_name.strip()[:16].strip()` (`Generate.py:387`), and the
/// second strip is not redundant: the slice can leave a trailing space that the first one had no
/// reason to touch, and a client that mishandles it is the comment upstream gives for doing it.
/// Transcribed rather than approximated, because this decides who owns a slot.
///
/// **The cut and nothing else**, which is all this is for: the name as it stands, sized as the
/// generator would size it. [`resolved_name`] is the whole of `handle_name` and does the
/// substitutions too, for the pass that has a player number to offer; this one is the second pass's
/// comparison, where there is no position to reason from yet.
pub fn ap_name(submitted: &str) -> String {
    resolved_name(submitted, None)
}

/// The substitutions `handle_name` performs, which [`ap_name`] deliberately skips.
///
/// `player` is the generator's player number and `number` is its name counter: how many yamls with
/// this same name (case-insensitively) have been seen up to and including this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamePosition {
    pub player: i32,
    pub number: usize,
}

/// A submitted name, as the generator would have spelled it.
///
/// **A port of Archipelago's `handle_name`, and faithful down to the two `.strip()` calls.** Read
/// off `Generate.py` rather than inferred from outputs: `%%` splits first so an escaped percent
/// cannot be read as a token, `%number%`/`%player%` become their brace forms, the braces are
/// formatted, and only then is the result trimmed, cut to sixteen **characters**, and trimmed
/// again.
///
/// `{NUMBER}` and `{PLAYER}` are the upper-case variants, which render as **nothing at all** at 1
/// rather than as "1": `Connor_Miner_HT{NUMBER}` is `Connor_Miner_HT` for the only player with that
/// name, and `Connor_Miner_HT2` for the second. That asymmetry is upstream's and it is the reason
/// these names can be reconstructed at all.
///
/// `position` is `None` where the caller has no player number to offer, which leaves every brace
/// token standing and makes this the cut and nothing else: [`ap_name`]'s behavior, unchanged.
///
/// **Sixteen CHARACTERS, not bytes.** Python slices code points and Rust makes the difference easy
/// to get wrong in the direction that panics. Unobservable through the lobby, which refuses a
/// non-ASCII name outright, but Archipelago produced the string being matched against, so
/// Archipelago's rule is the one to hold whatever reaches this from where.
pub fn resolved_name(submitted: &str, position: Option<NamePosition>) -> String {
    let substituted = match position {
        None => submitted.to_string(),
        Some(NamePosition { player, number }) => {
            // `%%` first, exactly as upstream does: it splits on the escape, rewrites the tokens
            // within each piece, and rejoins on a single `%`. Doing the token pass first would turn
            // `%%number%%` into something with a live token in it.
            let rewritten: Vec<String> = submitted
                .split("%%")
                .map(|piece| {
                    piece
                        .replace("%number%", "{number}")
                        .replace("%player%", "{player}")
                })
                .collect();
            let joined = rewritten.join("%");

            // **Only these four, and an unknown brace token is left alone.** Python's formatter is
            // wrapped in a `SafeFormatter` upstream precisely so an unrecognized field survives
            // rather than raising, so a name carrying `{whatever}` reaches the seed with the braces
            // still in it, and a reconstruction that dropped them would match nothing.
            joined
                .replace("{number}", &number.to_string())
                .replace(
                    "{NUMBER}",
                    &if number > 1 {
                        number.to_string()
                    } else {
                        String::new()
                    },
                )
                .replace("{player}", &player.to_string())
                .replace(
                    "{PLAYER}",
                    &if player > 1 {
                        player.to_string()
                    } else {
                        String::new()
                    },
                )
        }
    };

    substituted
        .trim()
        .chars()
        .take(16)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Every name a yaml's `triggers` block can set, in the order they appear.
///
/// ## Why this is needed at all
///
/// A yaml whose `game` is a weighted list of several games commonly carries a trigger per game that
/// renames the slot to match whichever rolled, because a name tells everybody which game a slot is:
///
/// ```yaml
/// game:
///   Super Mario 64: 10
///   Refunct: 10
/// name: chzit
/// triggers:
///   - option_name: game
///     option_result: Super Mario 64
///     options:
///       '':
///         name: chzit64
/// ```
///
/// The lobby sends `chzit`, the seed holds `chzit64`, and no amount of cutting turns one into the
/// other. `roll_triggers` runs **before** `ret.name` is read (`Generate.py`, lines 547 and 613) and
/// `update_weights` filters no keys, so this is upstream behavior rather than a quirk of one room.
///
/// ## The shape, read off `roll_triggers` rather than off samples
///
/// `options` is a map of **option category** to a map of options, and the category is applied with
/// `if category_name:`, so the empty string means the root. `name` is a root option, so only the
/// root category's `name` is the player's; a `name` under a game's category is that game's own
/// option of that name and is none of this function's business.
///
/// A value reaches `get_choice`, so it may be a bare string, a weighted map, or a list. **All of
/// them are collected**: this answers "which names could this yaml produce", and a weighted choice
/// between two names can produce either.
///
/// Triggers nest, through a trigger whose options set `triggers` again, so this recurses.
///
/// ## What it does NOT do
///
/// It does not evaluate anything. No `option_result` is compared, no percentage is rolled, no
/// weight is read. Every reachable name comes back and [`plan`] requires a slot to be named by
/// exactly one yaml, which is a stronger test than guessing which branch fired and is the only one
/// available without the whole generator.
pub fn trigger_names(content: &str) -> Vec<String> {
    let Ok(doc) = serde_saphyr::from_str::<serde_json::Value>(content) else {
        // An unreadable yaml yields no candidates, which leaves its slot exactly where it already
        // was: holding a claim link. The lobby parses these files with this same crate, so getting
        // here means the content changed under us rather than that Puna is stricter.
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_trigger_names(&doc, &mut out, 0);
    out
}

/// Walk one document's `triggers`, collecting every root `name` a trigger could set.
///
/// `depth` bounds the recursion rather than trusting the document: triggers nest legitimately, and
/// a hand-written yaml that nests them absurdly deep is a file this should decline to follow rather
/// than one it should blow the stack on. Four is past anything a person writes on purpose.
fn collect_trigger_names(node: &serde_json::Value, out: &mut Vec<String>, depth: usize) {
    const MAX_DEPTH: usize = 4;
    if depth > MAX_DEPTH {
        return;
    }
    let Some(triggers) = node.get("triggers").and_then(|t| t.as_array()) else {
        return;
    };
    for trigger in triggers {
        let Some(categories) = trigger.get("options").and_then(|o| o.as_object()) else {
            continue;
        };
        for (category, options) in categories {
            // The root, and only the root: see the note above on `if category_name:`.
            if !category.is_empty() {
                continue;
            }
            if let Some(name) = options.get("name") {
                push_choices(name, out);
            }
            // A trigger that installs further triggers. Rolled by the same loop upstream, so a name
            // behind two conditions is as reachable as one behind one.
            collect_trigger_names(options, out, depth + 1);
        }
    }
}

/// Every string a `get_choice` value could yield: itself, a list's items, or a weighted map's keys.
fn push_choices(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                push_choices(item, out);
            }
        }
        // A weighted choice: the KEYS are the candidates and the values are their weights. A weight
        // of zero is still collected, deliberately: this reports what a yaml could be called, and
        // `plan`'s uniqueness rule is what decides whether that is enough to claim a slot.
        serde_json::Value::Object(weights) => out.extend(weights.keys().cloned()),
        _ => {}
    }
}

/// Work out the assignment. **Pure**, so every rule below is testable without a lobby.
///
/// Four passes, in order of how directly each one knows the answer: the name as it stands, the name
/// cut to size by [`ap_name`], a name a `triggers` block could set, and a `{number}`/`{player}`
/// template reconstructed by [`resolved_name`]. See the module docs for why the lobby's name and the
/// generator's diverge, and why every pass takes a yaml only where it is the sole candidate.
///
/// **The order is the precedence and it matters.** Each pass offers more candidate names than the
/// one before, so running a later one first would prefer weaker evidence: a trigger name that
/// *could* be a slot's would take a slot whose exact name already settled it. A used yaml is out of
/// the running, which is what enforces that.
///
/// Two things it will not do:
///
/// * **Touch a slot that already has an owner.** Backfill is re-runnable and must never take a slot
///   off somebody who claimed it in the meantime, including on the first run, where a player may
///   have used their claim link between the room opening and the organizer pressing the button.
///   That slot is counted under `already_claimed`, not treated as if the lobby had never named it.
/// * **Match case-insensitively.** Archipelago's own uniqueness rule is case-insensitive, so two
///   slots cannot differ by case alone, but the generator's output is the authority on the exact
///   string, and loosening the comparison would only ever paper over a divergence worth seeing.
///
/// **Spectators are claimed like anybody else**, which reverses an earlier rule here. The argument
/// for skipping them was that the lobby's yamls are players, so a spectator matching one would be a
/// coincidence of naming, and that is simply not how the two systems work. A spectator slot exists
/// because somebody submitted a yaml for it, the lobby names that account, and everything downstream
/// already treats a spectator as an ordinary connectable slot: it takes an owner, a claim link, a
/// per-slot password and a tracker id like any other. Skipping it left the one slot the organizer
/// most wanted filled as the only one still holding a claim link.
///
/// **A yaml is `used` if it matched a slot at all**, claimed or already owned. Marking only the
/// claimed ones is what made a fully-claimed room report every yaml as matching nothing.
pub fn plan(roster: &[Slot], yamls: &[LobbyYaml]) -> Plan {
    // **Every exact match is settled before a single cut name is considered.** The two passes
    // cannot compete for one yaml (a yaml that matches a slot exactly is at most sixteen
    // characters, so cutting it changes nothing and it can only ever name that same slot), but the
    // ordering says so structurally rather than by that argument, and it is the argument that would
    // stop holding if the cut ever gained a substitution step.
    let mut matched: Vec<Option<usize>> = roster
        .iter()
        .map(|slot| yamls.iter().position(|y| y.player_name == slot.player_name))
        .collect();

    // Held by NAME rather than by index, which is how `unused` has always been counted: two yamls
    // spelled identically both named the slot that one of them matched, and reporting the second as
    // having named nothing would send an organizer looking for a mismatch.
    let mut used: std::collections::HashSet<&str> = matched
        .iter()
        .flatten()
        .map(|&i| yamls[i].player_name.as_str())
        .collect();

    // The second pass: the generator cut these names and the lobby did not. Order-independent,
    // since a yaml cuts to exactly one string and no two slots share a name, so a candidate here
    // belongs to one slot or to none, and which slot asks first cannot change the answer.
    for (slot, matched) in roster.iter().zip(matched.iter_mut()) {
        if matched.is_some() {
            continue;
        }
        let mut candidates = yamls
            .iter()
            .enumerate()
            .filter(|(_, y)| !used.contains(y.player_name.as_str()))
            .filter(|(_, y)| ap_name(&y.player_name) == slot.player_name);

        let Some((i, yaml)) = candidates.next() else {
            continue;
        };
        // **Two names cutting to one is left for a person.** Claiming either would be a guess about
        // which account a slot belongs to, and a wrong guess hands somebody else's world away,
        // where leaving it costs one claim link, which is what this slot has anyway.
        if candidates.next().is_some() {
            continue;
        }
        *matched = Some(i);
        used.insert(yaml.player_name.as_str());
    }

    // **The name counter, in the generator's own order.**
    //
    // `handle_name` counts how many yamls carrying this same name (lower-cased) it has already
    // seen, and substitutes that count. So reconstructing a templated name needs the yamls walked
    // in the order the generator walked them, which is what `slot_number` predicts. Computed once
    // here rather than per candidate: it is a property of the whole list, not of a pairing.
    //
    // Ordered by `(slot_number, index)` so the tie is the list's own order rather than whatever the
    // sort happened to do. A lobby that somehow reported duplicate slot numbers degrades to list
    // order, which is the same answer the old code gave by never looking at all.
    let mut by_position: Vec<usize> = (0..yamls.len()).collect();
    by_position.sort_by_key(|&i| (yamls[i].slot_number, i));
    let mut counter: std::collections::HashMap<String, usize> = Default::default();
    let mut name_number = vec![1usize; yamls.len()];
    for &i in &by_position {
        let key = yamls[i].player_name.to_lowercase();
        let seen = counter.entry(key).or_insert(0);
        *seen += 1;
        name_number[i] = *seen;
    }

    // **The third and fourth passes, which exist because two kinds of name cannot survive the
    // first two at all.**
    //
    // A `triggers` block can rename a slot after the lobby has reported its name, and a `{number}`
    // or `{player}` template reaches the seed as whatever the generator substituted. Both leave a
    // slot whose name the lobby never held, and both are recoverable: the trigger's names are in
    // the yaml, and the template's substitution is reproducible from the slot number itself, which
    // IS the player number the generator used.
    //
    // **Both are matched by name equality, not by position**, which is the whole reason they are
    // safe. The slot number is an input to the substitution rather than a claim in its own right,
    // so a room whose yamls were edited before generation produces a name that does not match and
    // the slot stays unclaimed, exactly as it does today. Nothing is assigned because it was
    // nearby.
    //
    // One loop over both sources, rather than two passes: a yaml offers a set of candidate names
    // and the rule is the same for every one of them, which keeps "named by exactly one yaml" a
    // single test instead of two that could disagree about precedence.
    //
    // **Spent by INDEX here, where the earlier passes go by name, and the difference is load
    // bearing.** `used` is keyed on the name because two yamls spelled identically cannot be told
    // apart, so the second naming nothing is noise rather than a mismatch worth reporting. A
    // TEMPLATE breaks that: two yamls both called `Troy{number}` are two players whose names
    // resolve differently, `Troy1` and `Troy2`, so collapsing them by name claims the first slot
    // and silently drops the second. Found by the test below rather than by reading.
    //
    // Keying on the index cannot reintroduce the thing `used` guards against: two yamls that
    // resolve to the SAME name are two candidates for one slot, and the uniqueness check already
    // refuses that. What reaches a claim here is a name exactly one yaml could carry.
    let mut spent: std::collections::HashSet<usize> = matched.iter().flatten().copied().collect();

    for (slot, matched) in roster.iter().zip(matched.iter_mut()) {
        if matched.is_some() {
            continue;
        }
        let names_this = |yaml: &LobbyYaml, i: usize| {
            // The trigger names, each cut as the generator cuts every name.
            if yaml
                .alternate_names
                .iter()
                .any(|alt| ap_name(alt) == slot.player_name)
            {
                return true;
            }
            // The template, substituted with this slot's own number. `{player}` is the player
            // number and a slot's number is that player number, so this asks "would this yaml,
            // generated AS this slot, have been called this?" and nothing weaker.
            resolved_name(
                &yaml.player_name,
                Some(NamePosition {
                    player: slot.slot_number,
                    number: name_number[i],
                }),
            ) == slot.player_name
        };

        let mut candidates = yamls
            .iter()
            .enumerate()
            .filter(|(i, _)| !spent.contains(i))
            .filter(|(i, y)| names_this(y, *i));

        let Some((i, yaml)) = candidates.next() else {
            continue;
        };
        // **Two yamls naming one slot is left for a person**, the same rule the cut pass follows and
        // for the same reason: a wrong guess hands somebody else's world away, and leaving it costs
        // one claim link, which is what the slot already has.
        if candidates.next().is_some() {
            continue;
        }
        *matched = Some(i);
        spent.insert(i);
        used.insert(yaml.player_name.as_str());
    }

    let mut claims = Vec::new();
    let mut unmatched = Vec::new();
    let mut already_claimed = 0;

    for (slot, matched) in roster.iter().zip(&matched) {
        match matched {
            // Counted whichever pass found it: a cut name is as much a match as an exact one, and
            // an organizer's next move is the same either way. What the two passes must not share
            // is the ownership branch below.
            Some(i) => {
                if slot.owner_id.is_some() {
                    already_claimed += 1;
                } else {
                    claims.push((slot.slot_number, yamls[*i].discord_id));
                }
            }
            // A slot nobody has claimed and the lobby cannot name. A slot that is already owned and
            // matches nothing is not reported at all: there is nothing for an organizer to do about
            // a slot that is already where it needs to be.
            None if slot.owner_id.is_none() => unmatched.push(slot.player_name.clone()),
            None => {}
        }
    }

    Plan {
        claims,
        unmatched,
        already_claimed,
        unused_ids: yamls
            .iter()
            .filter(|y| !used.contains(y.player_name.as_str()))
            .map(|y| y.id)
            .collect(),
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
///   outbound token, every room there, so the gate would withhold nothing from them.
/// * **No author fails.** The lobby records `-1` where a room has none, and `author()` has already
///   turned that into `None`; an absent author is nobody to have standing.
/// * **Otherwise the author must be an ORGANIZER**, not merely a member. A helper is trusted to run
///   this room, not to decide which lobby room it is bound to, and binding is what hands a
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
    /// organizer: one is the lobby's answer landing, the other is a player who beat it to it, and
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
/// nothing: the room opens and the organizer presses the button, through code that has already
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

    // **Planned once on names alone, and only then is any yaml content fetched.**
    //
    // The first plan is the whole answer for a room whose names line up, which is most of them, and
    // it is what identifies the handful worth asking about: a yaml that named a slot needs no
    // `triggers` read, because it has already been spent. So the cost of this feature on an
    // ordinary room is one extra `plan` call over a list already in memory.
    let mut yamls = fetched.yamls;
    let first = plan(&roster, &yamls);

    // Both sides must have leftovers for a rename to join anything up: no unclaimed slot to give,
    // or no yaml left to give it to, and there is nothing to find either way.
    if !first.unused_ids.is_empty() && !first.unmatched.is_empty() {
        let wanted: Vec<usize> = (0..yamls.len())
            .filter(|&i| first.unused_ids.contains(&yamls[i].id))
            .collect();

        for i in wanted {
            match lobby.yaml_content(lobby_room, yamls[i].id).await {
                Ok(content) => yamls[i].alternate_names = trigger_names(&content),
                // **Best effort, per yaml.** A yaml whose content cannot be read leaves its slot
                // where the first plan left it, which is holding a claim link. Failing the whole
                // import over it would turn a feature that recovers extra slots into a new way for
                // the import to fail, on rooms where it used to work.
                Err(e) => tracing::warn!(
                    room = %room,
                    lobby_room = %lobby_room,
                    yaml = %yamls[i].id,
                    error = %e,
                    "could not read a lobby yaml's triggers; its slot keeps its claim link"
                ),
            }
        }
    }

    // Replanned over the same roster with the alternatives filled in. Deliberately a second call
    // to the same pure function rather than a patch over the first result: one definition of the
    // rules, and the second run is a superset of the first by construction, since every pass it
    // adds only looks at yamls the earlier passes left unused.
    let plan = plan(&roster, &yamls);

    // **Rows first, and for every owner, before any slot points at one.** `room_slots.owner_id`
    // references `users`, so a slot claimed for an account that has never signed in would be a
    // foreign-key violation surfacing as a 500 on an otherwise correct import. The placeholder name
    // is what the roster renders as "never logged in" until they do.
    for (_, owner) in &plan.claims {
        puna_core::model::user::ensure_exists(conn, *owner).await?;
    }

    let claimed = puna_core::model::slot::claim_for_owners(conn, room, &plan.claims).await?;

    let unused = plan.unused();
    Ok(Imported {
        claimed,
        taken_first: plan.claims.len() - claimed,
        unmatched: plan.unmatched,
        already_claimed: plan.already_claimed,
        unused,
    })
}

impl Imported {
    /// The sentence an organizer reads. Plain counts, and it names the leftovers.
    ///
    /// **Every clause has to be true of the room, not just of this run.** The first version read
    /// "No slots were claimed from the lobby; 4 lobby YAML(s) matched no slot here" about a room
    /// where all four matched and three were already claimed, so the two facts it stated were the
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
            password_hidden: false,
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

    /// A lobby yaml with no alternative names and no position of its own.
    ///
    /// **`slot_number: 0` for every one of these, deliberately.** `plan` sorts by
    /// `(slot_number, index)` purely to order the name counter, so an unset position degrades to
    /// the list's own order, which is what a fixture that does not mention positions means. The
    /// tests that care about the counter use [`yaml_at`].
    ///
    /// The id is derived from the Discord id rather than random, so a failure names the same yaml
    /// on every run.
    fn yaml(name: &str, id: i64) -> LobbyYaml {
        LobbyYaml {
            player_name: name.into(),
            discord_id: id,
            id: uuid::Uuid::from_u128(id as u128),
            slot_number: 0,
            alternate_names: Vec::new(),
        }
    }

    /// The same, at a stated position in the generator's order.
    fn yaml_at(name: &str, id: i64, slot_number: i32) -> LobbyYaml {
        LobbyYaml {
            slot_number,
            ..yaml(name, id)
        }
    }

    /// The same, carrying the names its `triggers` could set.
    fn yaml_with_alts(name: &str, id: i64, alternates: &[&str]) -> LobbyYaml {
        LobbyYaml {
            alternate_names: alternates.iter().map(|s| s.to_string()).collect(),
            ..yaml(name, id)
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

    /// `-1` is the lobby's "no author", not a user id, and it would be a perfectly valid argument
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
        assert_eq!(plan.unused(), 1, "the lobby's Ray1 named no slot here");
    }

    /// Archipelago's own cut, transcribed. The second strip is the interesting one: it exists
    /// because the slice can expose a trailing space the first strip had no reason to touch.
    #[test]
    fn a_name_is_cut_the_way_the_generator_cuts_it() {
        assert_eq!(ap_name("Troy"), "Troy", "a name that fits is left alone");
        assert_eq!(ap_name("betterthanyou_Pupupu"), "betterthanyou_Pu");
        assert_eq!(ap_name("  Troy  "), "Troy");

        assert_eq!(
            ap_name("sixteencharacter"),
            "sixteencharacter",
            "exactly sixteen is not cut"
        );
        assert_eq!(
            ap_name("Troy the Second X"),
            "Troy the Second",
            "the slice ends on a space, which only the second strip removes"
        );

        // Sixteen CHARACTERS. Byte slicing would panic here rather than answer, which is the
        // failure mode worth pinning even though the lobby refuses a non-ASCII name.
        assert_eq!(ap_name(&"é".repeat(20)), "é".repeat(16));

        assert_eq!(
            ap_name("Ray{number}"),
            "Ray{number}",
            "no substitution: that is the generator's counter, not ours"
        );
    }

    /// **The reported case.** A 65-slot room imported from the prod lobby claimed 63 and reported
    /// *"no lobby YAML matched betterthanyou_Pu, betterthanyou_SM"*: two players whose yaml names
    /// ran past sixteen characters, which the lobby sends whole and the generator had already cut.
    #[test]
    fn a_name_the_generator_cut_still_matches_its_yaml() {
        let roster = [
            slot(1, "Troy", None, SlotKind::Player),
            slot(2, "betterthanyou_Pu", None, SlotKind::Player),
            slot(3, "betterthanyou_SM", None, SlotKind::Player),
        ];
        let yamls = [
            yaml("Troy", 7),
            yaml("betterthanyou_Pupupu", 8),
            yaml("betterthanyou_SMB3", 9),
        ];

        let plan = plan(&roster, &yamls);

        assert_eq!(plan.claims, vec![(1, 7), (2, 8), (3, 9)]);
        assert!(plan.unmatched.is_empty());
        assert_eq!(
            plan.unused(),
            0,
            "a yaml that named a slot under its cut name named a slot"
        );
    }

    /// **Two names cutting to one string is a question for a person.** Claiming either would be a
    /// guess about whose world a slot is, where leaving it costs the one claim link it already has.
    #[test]
    fn an_ambiguous_cut_claims_nobody() {
        let roster = [slot(1, "betterthanyou_Pu", None, SlotKind::Player)];
        let yamls = [
            yaml("betterthanyou_Pupupu", 7),
            yaml("betterthanyou_Punch", 8),
        ];
        // The fixture is the case, not two names that simply miss.
        for y in &yamls {
            assert_eq!(ap_name(&y.player_name), roster[0].player_name);
        }

        let plan = plan(&roster, &yamls);

        assert!(plan.claims.is_empty());
        assert_eq!(plan.unmatched, vec!["betterthanyou_Pu".to_string()]);
        assert_eq!(plan.unused(), 2, "neither of them named this slot");
    }

    /// **A name spelled in full beats one that only matches after cutting**, whichever order the
    /// lobby listed them in.
    ///
    /// The shape has to be contrived, and that is worth knowing rather than hiding: for a yaml to
    /// cut down to some *other* slot's name it must be padded, since anything short enough to match
    /// a slot exactly is short enough to survive the cut unchanged. So the two passes cannot
    /// genuinely compete for one yaml, and their separation is structure rather than a fix, which
    /// is exactly why the preference is pinned here instead of resting on that argument.
    #[test]
    fn a_name_spelled_in_full_beats_one_that_only_matches_cut() {
        let roster = [slot(1, "Ray", None, SlotKind::Player)];
        // Listed so the cut candidate is seen first: it is sixteen characters of `Ray` and padding
        // before the trailing strip, and `Ray` afterwards.
        let yamls = [yaml("Ray             xx", 7), yaml("Ray", 8)];
        assert_eq!(
            ap_name(&yamls[0].player_name),
            "Ray",
            "the fixture is the case"
        );

        let plan = plan(&roster, &yamls);

        assert_eq!(plan.claims, vec![(1, 8)], "the yaml that spells it wins");
        assert_eq!(plan.unused(), 1);
    }

    /// **Re-runnable, and it must never take a slot back.** Between the room opening and an
    /// organizer pressing backfill, a player may have used their claim link, and the lobby's answer
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
        assert_eq!(plan.unused(), 0);
    }

    /// The reported case, end to end: four slots, all four named by the lobby, three already
    /// claimed, and the fourth a spectator.
    ///
    /// It produced *"No slots were claimed from the lobby; 4 lobby YAML(s) matched no slot here"*:
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
            plan.unused(),
            0,
            "every yaml named a slot here; none of them matched nothing"
        );
        assert!(plan.unmatched.is_empty());

        let unused = plan.unused();
        let imported = Imported {
            claimed: 1,
            taken_first: 0,
            unmatched: plan.unmatched,
            already_claimed: plan.already_claimed,
            unused,
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
        assert_eq!(claimed_elsewhere.unused(), 0);
        assert_eq!(claimed_elsewhere.already_claimed, 1);

        let wrong_room = plan(&roster, &[yaml("Somebody", 7)]);
        assert_eq!(wrong_room.unused(), 1);
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
        assert_eq!(plan.unused(), 2);
    }

    /// **The trigger block from a real Random yaml, verbatim.**
    ///
    /// `chzit`'s submission to a live 275-slot room, which is the shape this whole pass exists for:
    /// a weighted `game` and one trigger per game renaming the slot to say which one rolled. The
    /// lobby reports `chzit`; the seed holds `chzit64` or `chzitRefunct`, decided at roll time.
    ///
    /// Read off a file the lobby accepted rather than written to suit the parser, because the
    /// indentation and the `''` category key are exactly where a hand-written fixture would differ
    /// from reality and pass anyway.
    #[test]
    fn trigger_names_reads_the_names_a_random_game_can_take() {
        let content = r#"
game:
  Super Mario 64: 10
  Refunct: 10
name: chzit
description: Generated on https://ap-lobby.ionium.us/options/sm64ex
Super Mario 64:
  progression_balancing: normal
  death_link: true
triggers:
  - option_name: game
    option_result: Super Mario 64
    options:
      '':
        name: chzit64
  - option_name: game
    option_result: Refunct
    options:
      '':
        name: chzitRefunct
Refunct:
  progression_balancing: normal
  amount_of_grass: 120
"#;
        assert_eq!(
            trigger_names(content),
            vec!["chzit64".to_string(), "chzitRefunct".to_string()],
            "the names behind a Random game were not read out of its triggers"
        );

        // A yaml with no triggers at all is the ordinary case and must be cheap and silent.
        assert!(trigger_names("name: Troy\ngame: Refunct\n").is_empty());

        // **Only the ROOT category's `name`.** A `name` under a game's own section is that game's
        // option of that name: `roll_triggers` applies a category with `if category_name:`, so the
        // empty string is the root and anything else is a game. Reading a game's option as a
        // player name would invent a candidate that no slot can ever be called.
        let game_scoped = r#"
name: Troy
game: Refunct
triggers:
  - option_name: game
    option_result: Refunct
    options:
      Refunct:
        name: not-a-player-name
"#;
        assert!(
            trigger_names(game_scoped).is_empty(),
            "a game's own `name` option was read as a player name"
        );

        // A weighted choice between names: `get_choice` can pick either, so both are candidates.
        let weighted = r#"
name: Troy
game: Refunct
triggers:
  - option_name: game
    option_result: Refunct
    options:
      '':
        name:
          TroyA: 1
          TroyB: 1
"#;
        let mut names = trigger_names(weighted);
        names.sort();
        assert_eq!(names, vec!["TroyA".to_string(), "TroyB".to_string()]);

        // Unreadable content yields nothing rather than failing: the slot keeps its claim link,
        // which is where it already was.
        assert!(trigger_names("\tnot: [valid").is_empty());
    }

    /// **A Random game's slot is claimed from its trigger names, and only when one yaml claims it.**
    ///
    /// The three real yamls from the room this was reported against: `chzit` renames to one of two
    /// names, `WIL57GD-Rando` to one of five, `AriesRando` to one of three. Whichever rolled, the
    /// seed's slot carries a name the lobby never sent.
    #[test]
    fn a_renamed_random_slot_is_claimed_from_its_trigger_names() {
        let roster = [
            slot(1, "chzitRefunct", None, SlotKind::Player),
            slot(2, "WIL57GD-SMS", None, SlotKind::Player),
            slot(3, "Serterd", None, SlotKind::Player),
        ];
        let yamls = [
            yaml_with_alts("chzit", 11, &["chzit64", "chzitRefunct"]),
            yaml_with_alts(
                "WIL57GD-Rando",
                22,
                &[
                    "WIL57GD-J&D",
                    "WIL57GD-P2",
                    "WIL57GD-RE",
                    "WIL57GD-SA2",
                    "WIL57GD-SMS",
                ],
            ),
            yaml("Serterd", 33),
        ];

        let plan = plan(&roster, &yamls);

        assert_eq!(
            plan.claims,
            vec![(1, 11), (2, 22), (3, 33)],
            "a Random game's slot was not matched to the yaml that could be called that"
        );
        assert!(plan.unmatched.is_empty());
        assert_eq!(plan.unused(), 0);
    }

    /// **Two yamls that could both be called one name is left for a person.**
    ///
    /// The same rule the cut pass follows, and the reason the pass does not try to work out which
    /// trigger actually fired: it cannot, without the generator, and a wrong guess hands somebody
    /// else's world away. Leaving it costs one claim link, which the slot already has.
    #[test]
    fn two_yamls_that_could_take_one_name_claim_nothing() {
        let roster = [slot(1, "SharedName", None, SlotKind::Player)];
        let yamls = [
            yaml_with_alts("First", 11, &["SharedName"]),
            yaml_with_alts("Second", 22, &["SharedName", "SomethingElse"]),
        ];

        let plan = plan(&roster, &yamls);

        assert!(
            plan.claims.is_empty(),
            "an ambiguous rename was guessed at rather than left alone"
        );
        assert_eq!(plan.unmatched, vec!["SharedName".to_string()]);
        assert_eq!(plan.unused(), 2);
    }

    /// **A `{NUMBER}` template is reconstructed, not guessed at positionally.**
    ///
    /// Three of these were in the same room: `Connor_Miner_HT{NUMBER}`, `PurpleRefunctAny{NUMBER}`,
    /// `FirefoxGrass{NUMBER}`. Upstream's asymmetry is what makes them recoverable: `{NUMBER}`
    /// renders as **nothing** for the first yaml of that name and as the count for later ones, so
    /// the sole `Connor_Miner_HT{NUMBER}` in a room is simply `Connor_Miner_HT`.
    ///
    /// Matched by name equality like every other pass. The slot number is an input to the
    /// substitution, never a claim: `{player}` is the generator's player number and a slot's number
    /// IS that player number, so this asks "would this yaml, generated as THIS slot, have been
    /// called this?" and accepts nothing weaker.
    #[test]
    fn a_templated_name_is_reconstructed_from_the_slot_it_would_be() {
        // The lone `{NUMBER}`: counter 1, so it renders away entirely.
        let lone = plan(
            &[slot(4, "Connor_Miner_HT", None, SlotKind::Player)],
            &[yaml_at("Connor_Miner_HT{NUMBER}", 11, 4)],
        );
        assert_eq!(
            lone.claims,
            vec![(4, 11)],
            "a lone {{NUMBER}} did not resolve"
        );

        // Two of one name: the counter walks the generator's order, which `slot_number` predicts,
        // so the first becomes `Troy1` and the second `Troy2`. Each names exactly one slot.
        let counted = plan(
            &[
                slot(1, "Troy1", None, SlotKind::Player),
                slot(2, "Troy2", None, SlotKind::Player),
            ],
            &[
                yaml_at("Troy{number}", 11, 1),
                yaml_at("Troy{number}", 22, 2),
            ],
        );
        assert_eq!(
            counted.claims,
            vec![(1, 11), (2, 22)],
            "the name counter did not follow the generator's order"
        );

        // `{player}` is the player number, so it resolves against the slot it would have been and
        // against no other. Asserted in both directions, since a pass that ignored the number
        // would claim the wrong slot rather than none.
        let positioned = plan(
            &[
                slot(7, "Ray7", None, SlotKind::Player),
                slot(8, "Ray8", None, SlotKind::Player),
            ],
            &[yaml_at("Ray{player}", 11, 7)],
        );
        assert_eq!(
            positioned.claims,
            vec![(7, 11)],
            "a {{player}} template did not resolve to the slot whose number it is"
        );
        assert_eq!(positioned.unmatched, vec!["Ray8".to_string()]);
    }

    /// **The substitutions, against `handle_name` as written.**
    ///
    /// Transcribed from `Generate.py:376` rather than inferred, because this decides who owns a
    /// slot. The `%%` escape is split FIRST so an escaped percent cannot be read as a token, and
    /// the cut happens last, after substitution, which is what makes a long template resolve to
    /// something that fits.
    #[test]
    fn resolved_name_matches_the_generators_own_substitution() {
        let at = |player, number| Some(NamePosition { player, number });

        // The upper-case forms vanish at 1 and appear past it. This is the asymmetry the
        // `{NUMBER}` recovery rests on.
        assert_eq!(resolved_name("Troy{NUMBER}", at(1, 1)), "Troy");
        assert_eq!(resolved_name("Troy{NUMBER}", at(1, 2)), "Troy2");
        assert_eq!(resolved_name("Troy{PLAYER}", at(1, 1)), "Troy");
        assert_eq!(resolved_name("Troy{PLAYER}", at(2, 1)), "Troy2");

        // The lower-case forms always render.
        assert_eq!(resolved_name("Troy{number}", at(1, 1)), "Troy1");
        assert_eq!(resolved_name("Troy{player}", at(5, 1)), "Troy5");

        // Archipelago's percent spellings, which become the brace forms before formatting.
        assert_eq!(resolved_name("Troy%number%", at(1, 3)), "Troy3");
        assert_eq!(resolved_name("Troy%player%", at(9, 1)), "Troy9");

        // **The escape, which is why the split comes first.** `%%` is a literal percent, so the
        // token inside it must survive as text rather than being substituted.
        assert_eq!(resolved_name("a%%number%%b", at(1, 1)), "a%number%b");

        // An unknown token is left standing, because upstream formats through a `SafeFormatter`
        // that does not raise on one. A reconstruction that dropped it would match nothing.
        assert_eq!(resolved_name("Troy{whatever}", at(1, 1)), "Troy{whatever}");

        // The cut is LAST: substitute, then trim, then sixteen characters, then trim again.
        assert_eq!(
            resolved_name("aaaaaaaaaaaaaaa{number}", at(1, 12)),
            "aaaaaaaaaaaaaaa1"
        );
        assert_eq!(resolved_name("  Troy{number}  ", at(1, 1)), "Troy1");

        // And with no position offered this is the cut and nothing else, which is `ap_name`.
        assert_eq!(resolved_name("Troy{NUMBER}", None), "Troy{NUMBER}");
        assert_eq!(ap_name("betterthanyou_Pupupu"), "betterthanyou_Pu");
    }

    /// **A name the lobby already matched is never reconsidered by a later pass.**
    ///
    /// The passes run in order and a used yaml is out of the running, which is what stops a trigger
    /// name or a template from stealing a slot that an exact name already settled. Worth pinning
    /// because the later passes generate MORE candidates than the earlier ones, so a reordering
    /// would not fail loudly: it would quietly prefer the weaker evidence.
    #[test]
    fn an_exact_name_outranks_a_rename_that_could_also_claim_it() {
        let roster = [
            slot(1, "Troy", None, SlotKind::Player),
            slot(2, "Other", None, SlotKind::Player),
        ];
        // The second yaml could be called `Troy` by a trigger, and the first simply IS `Troy`.
        let yamls = [
            yaml("Troy", 11),
            yaml_with_alts("Other", 22, &["Troy", "Other"]),
        ];

        let plan = plan(&roster, &yamls);

        assert_eq!(
            plan.claims,
            vec![(1, 11), (2, 22)],
            "a trigger name took a slot that an exact name had already matched"
        );
    }

    /// **A redirect is a refusal, and it must not be followed.**
    ///
    /// Both lobbies answer `/api/room/<id>` with `303` to `/auth/login` when the `X-Api-Key` is
    /// wrong or absent (a bad key is treated exactly like no key), and that login redirects on to
    /// `https://discord.com/oauth2/authorize`. reqwest follows redirects by default and strips only
    /// the headers it knows are sensitive, which does not include a custom `X-Api-Key`, so the
    /// lobby's own ADMIN_TOKEN was being re-sent along the chain.
    ///
    /// Asserted at the transport rather than as a source lint, because both halves of the failure
    /// are reachable here: without `Policy::none()` the client follows to the second endpoint,
    /// parses its HTML as JSON, and reports `Unreadable`, so an unsynced credential presented as
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
