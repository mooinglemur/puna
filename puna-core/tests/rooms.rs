//! Postgres-backed tests for room creation, the password modes, claims and cloning.
//!
//! Two families of property here, and they fail differently:
//!
//!   * **Authorization** -- who may see a slot's credentials, and what a clone carries over.
//!     A wrong answer hands one player another's password, and nothing at the time looks wrong.
//!   * **Credential completeness** -- every `per_slot` room has a password on every slot. Under
//!     pahoa's fail-closed rule a gap locks a player out, which is loud, but an *empty* map locks
//!     the room, which is worse and is one line away.

mod common;

use std::collections::HashSet;

use common::with_db;
use diesel_async::RunQueryDsl;
use puna_core::Environment;
use puna_core::artifact::SlotKind;
use puna_core::ids::{GenerationId, RoomId};
use puna_core::model::member::{self, RoomRole};
use puna_core::model::room::{
    self, DesiredState, JournalPolicy, NewRoom, Relationship, SlotAuth, SpoilerPolicy,
    TrackerPolicy,
};
use puna_core::model::{slot, user};

const OWNER: i64 = 1;
const HELPER: i64 = 2;
const PLAYER: i64 = 3;
const STRANGER: i64 = 4;

/// A generation with three player slots and one spectator, so every test exercises the case that
/// nearly went wrong: a spectator is connectable and therefore needs a credential.
async fn seed_generation(
    conn: &mut diesel_async::AsyncPgConnection,
    race_mode: bool,
) -> GenerationId {
    use diesel::sql_types::{Bool, Integer, Text, Uuid as SqlUuid};
    use diesel_async::RunQueryDsl;

    let id = GenerationId::new();
    diesel::sql_query(
        "INSERT INTO generations (id, sha256, size_bytes, seed_name, slots, locations, race_mode)
         VALUES ($1, decode(md5(random()::text), 'hex'), 1, 'seed', 4, 100, $2)",
    )
    .bind::<SqlUuid, _>(id)
    .bind::<Bool, _>(race_mode)
    .execute(conn)
    .await
    .expect("generation");

    for (number, name, game, kind) in [
        (1, "Troy", "A Link to the Past", "player"),
        (2, "Kai", "Super Metroid", "player"),
        (3, "Sam", "Factorio", "player"),
        (4, "Spectatrawr", "Archipelago", "spectator"),
    ] {
        diesel::sql_query(
            "INSERT INTO generation_slots (generation_id, slot_number, player_name, game, kind)
             VALUES ($1, $2, $3, $4, $5::slot_kind)",
        )
        .bind::<SqlUuid, _>(id)
        .bind::<Integer, _>(number)
        .bind::<Text, _>(name)
        .bind::<Text, _>(game)
        .bind::<Text, _>(kind)
        .execute(conn)
        .await
        .expect("generation slot");
    }
    id
}

async fn users(conn: &mut diesel_async::AsyncPgConnection) {
    for id in [OWNER, HELPER, PLAYER, STRANGER] {
        user::ensure_exists(conn, id).await.expect("user");
    }
}

#[tokio::test]
async fn creating_a_room_populates_slots_membership_and_credentials() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "test room", generation, OWNER),
        )
        .await
        .expect("create");

        // The uploader is the first organizer -- an ordinary roster row, not a special case.
        assert_eq!(
            member::role_of(&mut conn, id, OWNER).await.expect("role"),
            Some(RoomRole::Organizer)
        );

        let slots = slot::list(&mut conn, id).await.expect("slots");
        assert_eq!(slots.len(), 4, "spectators are slots too");
        assert_eq!(
            slots.iter().filter(|s| s.is_spectator()).count(),
            1,
            "the spectator must survive the copy from generation_slots"
        );

        // A claim link in EVERY mode, including `none`: claiming gates the patch download and puts
        // the room on a player's landing page whether or not there is a password.
        assert!(slots.iter().all(|s| s.claim_token.is_some()));
        assert!(slots.iter().all(|s| s.owner_id.is_none()));
        assert!(
            slots.iter().all(|s| s.password.is_none()),
            "`none` mode must not mint slot passwords"
        );

        // Every token and tracker id distinct: reusing one would let a single link claim two slots.
        let tokens: HashSet<_> = slots.iter().filter_map(|s| s.claim_token.clone()).collect();
        assert_eq!(tokens.len(), 4);
        let trackers: HashSet<_> = slots.iter().map(|s| s.tracker_id).collect();
        assert_eq!(trackers.len(), 4);

        let stored = room::get(&mut conn, id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(stored.slot_auth, SlotAuth::None);
        assert_eq!(stored.password, None);
        // **A new room is created running, as the reference implementation does.** An organizer
        // preparing one days early does not share the link yet, and Puna offers Stop and Close —
        // controls upstream has none of — so the unusual case is one click away while the ordinary
        // one is no clicks at all.
        assert_eq!(stored.desired_state, "running", "a new room starts running");
        assert_eq!(stored.state, "provisioning");
        // Not a race, so the permissive defaults.
        assert_eq!(stored.spoiler_policy, SpoilerPolicy::Staff);
        assert_eq!(stored.tracker_policy, TrackerPolicy::Link);
        // **Open, where the other two are not.** Those guard what the seed knows and a player has
        // not earned; this guards what the room's own participants said to each other, and the
        // feed link reaches exactly as far as the room link does.
        assert_eq!(stored.journal_policy, JournalPolicy::Full);
    })
    .await;
}

/// A race seed defaults to the settings whose leak cannot be taken back.
#[tokio::test]
async fn a_race_seed_defaults_to_the_closed_policies() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, true).await;

        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "race", generation, OWNER),
        )
        .await
        .expect("create");

        let stored = room::get(&mut conn, id)
            .await
            .expect("get")
            .expect("present");
        // **The spoiler does NOT vary with the seed, and no longer does.** It used to be `never`
        // for a race — nobody at all, the organizer included — which is a real choice and a bad
        // one to be handed silently: the person it locks out is the one who would need the file to
        // settle an argument. Every room now starts staff-only, and the options page offers all
        // four settings including `never`.
        assert_eq!(stored.spoiler_policy, SpoilerPolicy::Staff);
        assert_eq!(stored.tracker_policy, TrackerPolicy::Members);
        // **Not `Disabled`, and the difference matters.** A race's history is a live scoreboard —
        // who found what, in order — so the chat and hints come out; the item feed stays, because
        // it is the same information the room already broadcasts to every unfiltered client and a
        // racer's own client is showing it to them anyway.
        assert_eq!(stored.journal_policy, JournalPolicy::Feed);
    })
    .await;
}

/// The setting an organizer changes, and the one thing it must not do.
///
/// **A journal policy is not a restart**, unlike every other control beside it in that section:
/// nothing here reaches pahoa, moves the spec hash, or queues a redeploy. Getting that wrong would
/// disconnect a room full of people to change a gate that lives entirely in the web tier — and it
/// would look like the setting working, because the gate would also change.
#[tokio::test]
async fn changing_the_journal_policy_changes_nothing_about_the_room() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "async", generation, OWNER),
        )
        .await
        .expect("create");

        for policy in [
            JournalPolicy::Disabled,
            JournalPolicy::Feed,
            JournalPolicy::Full,
        ] {
            room::set_journal_policy(&mut conn, id, policy)
                .await
                .expect("set");
            let stored = room::get(&mut conn, id)
                .await
                .expect("get")
                .expect("present");
            assert_eq!(stored.journal_policy, policy);
            assert!(
                !common::redeploy_requested(&mut conn, id).await,
                "{} queued a restart to change a gate the room cannot see",
                policy.as_sql()
            );
        }
    })
    .await;
}

/// A clone carries the source room's answer rather than re-deriving it from the seed.
///
/// The seed is not a race, so a re-derivation would silently reopen a room whose organizer had
/// closed it — and a clone is usually the *same group playing again*, which is exactly when the
/// decision they already made should carry.
#[tokio::test]
async fn a_clone_keeps_the_journal_policy_it_was_cloned_from() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let source = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "async", generation, OWNER),
        )
        .await
        .expect("create");
        room::set_journal_policy(&mut conn, source, JournalPolicy::Disabled)
            .await
            .expect("set");

        let clone = room::clone_room(&mut conn, source, "async 2".into(), OWNER, true)
            .await
            .expect("clone");
        let stored = room::get(&mut conn, clone)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(
            stored.journal_policy,
            JournalPolicy::Disabled,
            "the clone reopened a history its organizer had closed"
        );
    })
    .await;
}

/// All six transitions, with the completeness property asserted at every step.
#[tokio::test]
async fn every_slot_auth_transition_lands_consistent() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        for from in SlotAuth::ALL {
            for to in SlotAuth::ALL {
                if from == to {
                    continue;
                }

                let mut new = NewRoom::direct(Environment::Dev, "modes", generation, OWNER);
                new.slot_auth = from;
                let id = room::create(&mut conn, &new).await.expect("create");

                assert_mode(&mut conn, id, from).await;
                room::set_slot_auth(&mut conn, id, to)
                    .await
                    .expect("switch");
                assert_mode(&mut conn, id, to).await;
            }
        }
    })
    .await;
}

/// The room row, the slot rows and the mode all agree.
async fn assert_mode(conn: &mut diesel_async::AsyncPgConnection, id: RoomId, mode: SlotAuth) {
    let stored = room::get(conn, id).await.expect("get").expect("present");
    assert_eq!(stored.slot_auth, mode);

    // The `room_password_matches_mode` CHECK already forbids disagreement, so this asserts the
    // code puts the room on the right side of it rather than that the constraint works.
    assert_eq!(
        stored.password.is_some(),
        mode == SlotAuth::Room,
        "a room-wide password exists exactly in `room` mode ({mode:?})"
    );

    let entries = slot::passwords(conn, id).await.expect("passwords");
    assert_eq!(entries.len(), 4, "every slot listed, {mode:?}");

    match mode {
        SlotAuth::PerSlot => {
            // COMPLETE, not merely non-empty. Under pahoa's fail-closed rule a slot missing from
            // the map is refused, so a gap here is a player who cannot join -- and the spectator
            // is the slot most likely to be the gap.
            assert!(
                entries.iter().all(|(_, p)| p.is_some()),
                "per_slot must give every slot a password, spectators included: {entries:?}"
            );
            let distinct: HashSet<_> = entries.iter().filter_map(|(_, p)| p.clone()).collect();
            assert_eq!(distinct.len(), 4, "each slot's password must be its own");
        }
        SlotAuth::None | SlotAuth::Room => {
            // Every one NULL, so the caller renders no PAHOA_SLOT_PASSWORDS key at all. An empty
            // JSON object would be per-slot mode with nobody holding a key: a locked room.
            assert!(
                entries.iter().all(|(_, p)| p.is_none()),
                "leaving per_slot must clear every slot password: {entries:?}"
            );
        }
    }
}

/// **Describing a link must never spend it**, which is the property the whole landing page rests
/// on.
///
/// A claim token is single-use and a chat client fetches a link the moment it is pasted, before
/// anybody has clicked — so a read path that went through `claim` would leave the recipient with a
/// link that had already worked for a crawler. The same is true of any prefetch holding a session,
/// which is how the old `GET`-that-redeemed could be spent by a browser being helpful.
///
/// Asserted by reading twice and then claiming, because the failure is invisible from the read
/// itself: a consuming lookup returns exactly the same offer the first time.
#[tokio::test]
async fn describing_a_claim_link_does_not_spend_it() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;
        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "Friday async", generation, OWNER),
        )
        .await
        .expect("create");

        let slots = slot::list(&mut conn, id).await.expect("slots");
        let token = slots[0].claim_token.clone().expect("token");

        for _ in 0..2 {
            let offer = slot::offered_by_claim_token(&mut conn, &token)
                .await
                .expect("read")
                .expect("an unspent link describes itself");
            assert_eq!(offer.room_id, id);
            assert_eq!(offer.room_name, "Friday async");
            assert_eq!(offer.slot_number, slots[0].slot_number);
            assert_eq!(offer.player_name, slots[0].player_name);
        }

        // Still claimable afterwards, by somebody who is not the crawler.
        slot::claim(&mut conn, &token, PLAYER).await.expect("claim");

        // And a spent link describes nothing, so the page cannot offer what it cannot deliver.
        assert!(
            slot::offered_by_claim_token(&mut conn, &token)
                .await
                .expect("read")
                .is_none(),
            "a spent claim link still describes itself, so the page would offer a dead control"
        );
    })
    .await;
}

/// The same property for an invite, where the cost of getting it wrong is one use rather than the
/// only one — an organizer who minted a three-use link would find it two.
#[tokio::test]
async fn describing_an_invite_does_not_spend_a_use() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;
        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "Friday async", generation, OWNER),
        )
        .await
        .expect("create");

        let token = member::create_invite(&mut conn, id, RoomRole::Helper, OWNER, None, Some(1))
            .await
            .expect("invite");

        for _ in 0..3 {
            let offer = member::offered_by_invite_token(&mut conn, &token)
                .await
                .expect("read")
                .expect("an unspent invite describes itself");
            assert_eq!(offer.room_id, id);
            assert_eq!(offer.room_name, "Friday async");
            assert_eq!(offer.role, RoomRole::Helper);
        }

        // The single use it was minted with is still there.
        member::redeem_invite(&mut conn, &token, PLAYER)
            .await
            .expect("redeem");
        assert!(
            member::offered_by_invite_token(&mut conn, &token)
                .await
                .expect("read")
                .is_none(),
            "a spent invite still describes itself"
        );
    })
    .await;
}

#[tokio::test]
async fn a_claim_link_is_single_use_and_grants_only_that_slot() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;
        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "claims", generation, OWNER),
        )
        .await
        .expect("create");

        let slots = slot::list(&mut conn, id).await.expect("slots");
        let token = slots[0].claim_token.clone().expect("token");

        let claimed = slot::claim(&mut conn, &token, PLAYER).await.expect("claim");
        assert_eq!(claimed.owner_id, Some(PLAYER));
        assert!(claimed.claim_token.is_none(), "the link is spent");

        // A second use finds nothing: the token was nulled in the same statement that set the
        // owner, so there is no window between checking and consuming it.
        assert!(slot::claim(&mut conn, &token, STRANGER).await.is_err());
        let after = slot::get(&mut conn, id, claimed.slot_number)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(after.owner_id, Some(PLAYER), "the first claimant keeps it");

        // Releasing hands it back with a FRESH link, because the old one may have been shared
        // with whoever is being replaced.
        let reissued = slot::release(&mut conn, id, claimed.slot_number)
            .await
            .expect("release");
        assert_ne!(reissued, token);
        assert!(slot::claim(&mut conn, &token, STRANGER).await.is_err());
        assert!(slot::claim(&mut conn, &reissued, STRANGER).await.is_ok());
    })
    .await;
}

/// Two people follow one claim link. Exactly one may win.
#[tokio::test]
async fn concurrent_claims_cannot_both_succeed() {
    with_db(|pool| async move {
        let mut setup = pool.get().await.expect("connection");
        users(&mut setup).await;
        let generation = seed_generation(&mut setup, false).await;
        let id = room::create(
            &mut setup,
            &NewRoom::direct(Environment::Dev, "race", generation, OWNER),
        )
        .await
        .expect("create");

        const CLAIMANTS: i64 = 16;
        for user_id in 100..100 + CLAIMANTS {
            user::ensure_exists(&mut setup, user_id)
                .await
                .expect("user");
        }
        let token = slot::list(&mut setup, id).await.expect("slots")[0]
            .claim_token
            .clone()
            .expect("token");
        drop(setup);

        let mut tasks = Vec::new();
        for user_id in 100..100 + CLAIMANTS {
            let pool = pool.clone();
            let token = token.clone();
            tasks.push(tokio::spawn(async move {
                let mut conn = pool.get().await.expect("connection");
                slot::claim(&mut conn, &token, user_id).await.is_ok()
            }));
        }

        let mut winners = 0;
        for task in tasks {
            if task.await.expect("join") {
                winners += 1;
            }
        }
        assert_eq!(winners, 1, "a claim link is single-use under concurrency");
    })
    .await;
}

#[tokio::test]
async fn a_clone_carries_the_people_and_none_of_the_credentials() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let mut new = NewRoom::direct(Environment::Dev, "original", generation, OWNER);
        new.slot_auth = SlotAuth::PerSlot;
        let source = room::create(&mut conn, &new).await.expect("create");

        member::set_role(&mut conn, source, HELPER, RoomRole::Helper, Some(OWNER))
            .await
            .expect("helper");
        let source_slots = slot::list(&mut conn, source).await.expect("slots");
        slot::claim(
            &mut conn,
            source_slots[0].claim_token.as_deref().expect("token"),
            PLAYER,
        )
        .await
        .expect("claim");

        let clone = room::clone_room(&mut conn, source, "clone".into(), OWNER, true)
            .await
            .expect("clone");

        // The roster carries over, so the same organizers and helpers keep working.
        assert_eq!(
            member::role_of(&mut conn, clone, HELPER)
                .await
                .expect("role"),
            Some(RoomRole::Helper)
        );
        assert_eq!(
            member::role_of(&mut conn, clone, OWNER)
                .await
                .expect("role"),
            Some(RoomRole::Organizer)
        );

        let clone_slots = slot::list(&mut conn, clone).await.expect("slots");
        assert_eq!(clone_slots.len(), source_slots.len());

        // Owners carry over -- the same people keep their slots without re-claiming...
        assert_eq!(clone_slots[0].owner_id, Some(PLAYER));
        assert!(
            clone_slots[0].claim_token.is_none(),
            "an owned slot needs no claim link"
        );

        // ...and no credential does. This is the property the clone exists to have.
        let source_secrets: HashSet<_> = source_slots
            .iter()
            .filter_map(|s| s.password.clone())
            .chain(source_slots.iter().filter_map(|s| s.claim_token.clone()))
            .collect();
        for entry in &clone_slots {
            if let Some(password) = &entry.password {
                assert!(!source_secrets.contains(password), "password reused");
            }
            if let Some(token) = &entry.claim_token {
                assert!(!source_secrets.contains(token), "claim token reused");
            }
        }
        assert!(
            clone_slots.iter().all(|s| s.password.is_some()),
            "the clone inherits per_slot mode, so every slot needs its own password"
        );

        let cloned = room::get(&mut conn, clone)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(cloned.cloned_from, Some(source));
        assert_eq!(cloned.generation_id, generation);

        // The source is untouched: same slots, same owners, same secrets.
        let after = slot::list(&mut conn, source).await.expect("slots");
        assert_eq!(after[0].owner_id, Some(PLAYER));
        assert_eq!(after[0].password, source_slots[0].password);

        let siblings = room::siblings(&mut conn, source, generation)
            .await
            .expect("siblings");
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0].id, clone);
    })
    .await;
}

/// The other clone shape: a changing roster, so nobody keeps a slot by default.
#[tokio::test]
async fn a_clone_can_start_with_unclaimed_slots() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;
        let source = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "original", generation, OWNER),
        )
        .await
        .expect("create");

        let token = slot::list(&mut conn, source).await.expect("slots")[0]
            .claim_token
            .clone()
            .expect("token");
        slot::claim(&mut conn, &token, PLAYER).await.expect("claim");

        let clone = room::clone_room(&mut conn, source, "fresh".into(), OWNER, false)
            .await
            .expect("clone");

        let slots = slot::list(&mut conn, clone).await.expect("slots");
        assert!(slots.iter().all(|s| s.owner_id.is_none()));
        assert!(slots.iter().all(|s| s.claim_token.is_some()));
    })
    .await;
}

#[tokio::test]
async fn my_rooms_finds_both_ways_a_room_can_be_yours() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let staffed = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "staffed", generation, OWNER),
        )
        .await
        .expect("create");
        let played = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "played", generation, OWNER),
        )
        .await
        .expect("create");

        let token = slot::list(&mut conn, played).await.expect("slots")[0]
            .claim_token
            .clone()
            .expect("token");
        slot::claim(&mut conn, &token, PLAYER).await.expect("claim");
        member::set_role(&mut conn, staffed, PLAYER, RoomRole::Helper, Some(OWNER))
            .await
            .expect("helper");

        let mine = room::mine(&mut conn, PLAYER).await.expect("mine");
        assert_eq!(mine.len(), 2, "{mine:#?}");

        let staffed_entry = mine.iter().find(|m| m.room.id == staffed).expect("staffed");
        assert_eq!(
            staffed_entry.relationship,
            Relationship::Staff(RoomRole::Helper)
        );
        let played_entry = mine.iter().find(|m| m.room.id == played).expect("played");
        assert_eq!(played_entry.relationship, Relationship::Player);

        // Staff beats player when both apply: an organizer who also plays wants the staff view.
        member::set_role(&mut conn, played, PLAYER, RoomRole::Organizer, Some(OWNER))
            .await
            .expect("promote");
        let mine = room::mine(&mut conn, PLAYER).await.expect("mine");
        let played_entry = mine.iter().find(|m| m.room.id == played).expect("played");
        assert_eq!(
            played_entry.relationship,
            Relationship::Staff(RoomRole::Organizer)
        );

        // A stranger holds nothing, and rooms you merely visited never appear.
        assert!(
            room::mine(&mut conn, STRANGER)
                .await
                .expect("mine")
                .is_empty()
        );
    })
    .await;
}

#[tokio::test]
async fn requesting_a_state_is_idempotent() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;
        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "lifecycle", generation, OWNER),
        )
        .await
        .expect("create");

        // Created running, so the room has to be stopped before "start it" is a change at all.
        assert!(
            room::request_state(&mut conn, id, DesiredState::Stopped)
                .await
                .expect("stop")
        );
        assert!(
            room::request_state(&mut conn, id, DesiredState::Running)
                .await
                .expect("start")
        );
        // A second request updates zero rows, which is what makes "requested twice while starting"
        // need no special case in the orchestrator.
        assert!(
            !room::request_state(&mut conn, id, DesiredState::Running)
                .await
                .expect("start again")
        );

        let stored = room::get(&mut conn, id)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(stored.desired_state, "running");
        // The observed half is untouched: the web tier asked, the orchestrator has not acted.
        assert_eq!(stored.state, "provisioning");
        assert_eq!(stored.advertised_port, None);
    })
    .await;
}

/// The spectator, end to end: it is a slot, it is claimable, and in `per_slot` it holds a
/// credential like anyone else.
#[tokio::test]
async fn a_spectator_is_a_first_class_slot() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let mut new = NewRoom::direct(Environment::Dev, "with a spectator", generation, OWNER);
        new.slot_auth = SlotAuth::PerSlot;
        let id = room::create(&mut conn, &new).await.expect("create");

        let spectator = slot::list(&mut conn, id)
            .await
            .expect("slots")
            .into_iter()
            .find(|s| s.kind == SlotKind::Spectator)
            .expect("the spectator survived");

        assert!(
            spectator.password.is_some(),
            "a spectator connects, so it needs a credential like anyone else"
        );
        let token = spectator.claim_token.clone().expect("claimable");
        let claimed = slot::claim(&mut conn, &token, PLAYER).await.expect("claim");
        assert_eq!(claimed.owner_id, Some(PLAYER));

        // And it is subject to the same access rule as any other slot.
        assert!(slot::may_access(&claimed, Some(PLAYER), None, false));
        assert!(!slot::may_access(&claimed, Some(STRANGER), None, false));
    })
    .await;
}

/// **`open` widens the patch and nothing else.**
///
/// This is the reference implementation's behavior — archipelago.gg serves every slot's patch to
/// anyone holding the room's URL — and it is the whole trade the two policies express: `claimed`
/// costs a player a sign-in and a claim, and pays them back by embedding the credential so the
/// client connects on its own.
///
/// **The half that must not move is the password route.** Both take a slot guard, they sit two
/// lines apart in the roster, and widening them together would turn "patches are public, as
/// upstream" into "every slot's password is public" — with nothing failing, on a page that is
/// already public. So the two rules are separate functions and this asserts they disagree.
#[tokio::test]
async fn an_open_patch_policy_widens_the_patch_and_never_the_password() {
    use puna_core::model::room::PatchPolicy;

    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;
        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "async", generation, OWNER),
        )
        .await
        .expect("create");

        let slots = slot::list(&mut conn, id).await.expect("slots");
        let token = slots[0].claim_token.clone().expect("token");
        let held = slot::claim(&mut conn, &token, PLAYER).await.expect("claim");

        // A stranger: no slot here, no role, not an admin.
        assert!(
            !slot::may_download_patch(PatchPolicy::Claimed, &held, Some(STRANGER), None, false),
            "a claimed room served a patch to somebody with no claim on it"
        );
        assert!(
            slot::may_download_patch(PatchPolicy::Open, &held, Some(STRANGER), None, false),
            "an open room refused a patch, which is the whole behavior the policy exists for"
        );
        // Not even signed in, which is the case that makes it upstream's behavior rather than a
        // slightly looser version of Puna's.
        assert!(slot::may_download_patch(
            PatchPolicy::Open,
            &held,
            None,
            None,
            false
        ));

        // The password is untouched by either policy: it goes through `may_access`, which knows
        // nothing about patches.
        assert!(
            !slot::may_access(&held, Some(STRANGER), None, false),
            "the patch policy reached the password rule"
        );
        assert!(slot::may_access(&held, Some(PLAYER), None, false));
    })
    .await;
}

/// A rename changes the label and nothing else.
///
/// **The point of the test is the second half.** Every other control on the room page that looks
/// like a setting is a restart, and this one is not — object names are `mw-<room id>`, so
/// `rooms.name` reaches no manifest and no spec hash. A rename that moved `spec_hash` would bounce
/// the room and disconnect everybody for a cosmetic edit, and nothing at the time would say why.
#[tokio::test]
async fn renaming_a_room_touches_nothing_but_the_name() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let new = NewRoom::direct(Environment::Dev, "friday night", generation, OWNER);
        let id = room::create(&mut conn, &new).await.expect("create");
        let before = room::get(&mut conn, id)
            .await
            .expect("read")
            .expect("exists");

        // Pretend the room has started, so there is a hash for a rename to disturb. `Room` does not
        // project `spec_hash` -- it is orchestrator-owned -- so this reads the column itself, which
        // is what the planner compares and therefore the thing that must not move.
        diesel::sql_query(
            "UPDATE rooms SET spec_hash = 'hash-1', redeploy_requested_at = NULL WHERE id = $1",
        )
        .bind::<diesel::sql_types::Uuid, _>(id)
        .execute(&mut conn)
        .await
        .expect("seed a spec hash");

        room::rename(&mut conn, id, "saturday morning")
            .await
            .expect("rename");
        let after = room::get(&mut conn, id)
            .await
            .expect("read")
            .expect("exists");

        assert_eq!(after.name, "saturday morning");
        assert_eq!(
            after.desired_state, before.desired_state,
            "and must not change what the room is meant to be doing"
        );
        assert_eq!(after.slot_auth, before.slot_auth);

        #[derive(diesel::QueryableByName)]
        struct Spec {
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
            spec_hash: Option<String>,
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>)]
            redeploy_requested_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let spec: Vec<Spec> =
            diesel::sql_query("SELECT spec_hash, redeploy_requested_at FROM rooms WHERE id = $1")
                .bind::<diesel::sql_types::Uuid, _>(id)
                .load(&mut conn)
                .await
                .expect("read the spec columns");
        let spec = spec.into_iter().next().expect("the room is still there");

        assert_eq!(
            spec.spec_hash.as_deref(),
            Some("hash-1"),
            "a rename must not move the spec hash -- that would restart the room"
        );
        assert_eq!(
            spec.redeploy_requested_at, None,
            "and must not queue a restart either"
        );
    })
    .await;
}

/// One definition of a usable room name, for create, clone and rename alike.
///
/// Three callers had two answers between them and no length rule at all, which is how a name one
/// path accepts becomes one another path cannot store.
#[test]
fn a_room_name_is_trimmed_and_bounded() {
    use puna_core::model::room::{MAX_NAME_CHARS, NameError, validate_name};

    assert_eq!(
        validate_name("  friday night  ").as_deref(),
        Ok("friday night")
    );
    assert_eq!(validate_name(""), Err(NameError::Empty));
    assert_eq!(validate_name("   \t\n "), Err(NameError::Empty));

    // Counted in CHARACTERS, so the limit does not depend on the alphabet somebody names their
    // room in: this is well under the cap in chars and well over it in bytes.
    let cyrillic = "я".repeat(MAX_NAME_CHARS);
    assert!(
        cyrillic.len() > MAX_NAME_CHARS,
        "the byte length would fail a byte-based cap"
    );
    assert!(validate_name(&cyrillic).is_ok());

    assert_eq!(
        validate_name(&"x".repeat(MAX_NAME_CHARS + 1)),
        Err(NameError::TooLong)
    );
    // Trimmed BEFORE measuring, or padding a name to the cap would refuse it.
    assert!(validate_name(&format!("   {}   ", "x".repeat(MAX_NAME_CHARS))).is_ok());
}

/// `/admin/rooms` splits the fleet on `desired_state`, and this is the query that does it.
///
/// **The predicate is interpolated SQL**, which nothing else here is — a scope that silently
/// matched everything would put every stopped room back on a page built to keep them off it, and
/// the page would look completely normal. So each scope is asserted for what it returns *and* for
/// what it leaves out.
#[tokio::test]
async fn the_fleet_overview_scopes_on_what_was_asked_for() {
    use puna_core::model::fleet::{self, Scope};

    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let mut ids = Vec::new();
        for (name, desired) in [
            ("up", "running"),
            ("stopped", "stopped"),
            ("closed", "closed"),
        ] {
            let id = room::create(
                &mut conn,
                &NewRoom::direct(Environment::Dev, name, generation, OWNER),
            )
            .await
            .expect("create");
            diesel::sql_query(
                "UPDATE rooms SET desired_state = $2::room_desired_state WHERE id = $1",
            )
            .bind::<diesel::sql_types::Uuid, _>(id)
            .bind::<diesel::sql_types::Text, _>(desired)
            .execute(&mut conn)
            .await
            .expect("set desired state");
            ids.push((name, id));
        }

        let names = |o: &fleet::Overview| {
            let mut seen: Vec<String> = o.rooms.iter().map(|r| r.name.clone()).collect();
            seen.sort();
            seen
        };

        let active = fleet::overview(&mut conn, Environment::Dev, Scope::Active)
            .await
            .expect("active");
        assert_eq!(names(&active), vec!["up".to_string()]);

        let resting = fleet::overview(&mut conn, Environment::Dev, Scope::Resting)
            .await
            .expect("resting");
        assert_eq!(
            names(&resting),
            vec!["closed".to_string(), "stopped".into()]
        );

        let all = fleet::overview(&mut conn, Environment::Dev, Scope::All)
            .await
            .expect("all");
        assert_eq!(names(&all).len(), 3);

        // The count is what the collapsed heading reports, so it must be right whichever scope
        // was loaded -- including the one that deliberately loaded none of them.
        for overview in [&active, &resting, &all] {
            assert_eq!(
                overview.resting, 2,
                "the resting count is scope-independent"
            );
        }

        // The creator comes back through the LEFT JOIN. An inner join would have dropped every row
        // with no `created_by`, which on an admin page reads as the room not existing.
        assert_eq!(active.rooms[0].created_by, Some(OWNER));
        assert!(active.rooms[0].created_by_name.is_some());

        diesel::sql_query("UPDATE rooms SET created_by = NULL WHERE id = $1")
            .bind::<diesel::sql_types::Uuid, _>(ids[0].1)
            .execute(&mut conn)
            .await
            .expect("orphan the room");
        let orphaned = fleet::overview(&mut conn, Environment::Dev, Scope::Active)
            .await
            .expect("active");
        assert_eq!(
            names(&orphaned),
            vec!["up".to_string()],
            "a room with no creator is still a room"
        );
        assert_eq!(orphaned.rooms[0].created_by, None);
    })
    .await;
}

/// Account standing: what each rung withholds, and that nothing is destroyed by any of them.
///
/// **The second half is the load-bearing one.** A ban is a statement about a person; the rooms they
/// opened and the slots they hold are other people's games. A sanction that quietly emptied those
/// would punish everybody in the room, and it would do it silently — nothing about a missing slot
/// owner says *why*.
#[tokio::test]
async fn a_sanction_withholds_and_never_deletes() {
    use puna_core::model::user::{self, UserStatus};

    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "theirs", generation, OWNER),
        )
        .await
        .expect("create");

        // They hold a slot in it, as somebody sanctioned mid-async normally would.
        let token = slot::list(&mut conn, id).await.expect("slots")[0]
            .claim_token
            .clone()
            .expect("claimable");
        slot::claim(&mut conn, &token, OWNER).await.expect("claim");

        // The ladder, and each rung is strictly more than the last.
        assert!(UserStatus::Active.may_create() && UserStatus::Active.may_act());
        assert!(!UserStatus::Restricted.may_create());
        assert!(
            UserStatus::Restricted.may_act(),
            "restricted still plays -- that is the entire point of the middle rung"
        );
        assert!(!UserStatus::Banned.may_create() && !UserStatus::Banned.may_act());

        for status in [UserStatus::Restricted, UserStatus::Banned] {
            user::set_status(&mut conn, OWNER, status, Some("spam"), HELPER)
                .await
                .expect("set status");

            let (read, note) = user::status_of(&mut conn, OWNER)
                .await
                .expect("read")
                .expect("the user exists");
            assert_eq!(read, status);
            assert_eq!(note.as_deref(), Some("spam"));

            // Nothing of theirs moved.
            assert!(
                room::get(&mut conn, id).await.expect("read").is_some(),
                "{status:?} deleted a room"
            );
            assert_eq!(
                slot::list(&mut conn, id).await.expect("slots")[0].owner_id,
                Some(OWNER),
                "{status:?} took a slot away from its holder"
            );
            assert_eq!(
                member::role_of(&mut conn, id, OWNER).await.expect("role"),
                Some(RoomRole::Organizer),
                "{status:?} removed them from their own room's roster"
            );
        }

        // Restoring clears the note: it explained a sanction that no longer applies, and leaving it
        // would show a reason beside an account that is fine.
        user::set_status(&mut conn, OWNER, UserStatus::Active, None, HELPER)
            .await
            .expect("restore");
        let (read, note) = user::status_of(&mut conn, OWNER)
            .await
            .expect("read")
            .expect("exists");
        assert_eq!(read, UserStatus::Active);
        assert_eq!(note, None, "a restored account still showed a reason");
    })
    .await;
}

/// The admin listing names everybody, with the numbers a sanction decision needs.
#[tokio::test]
async fn the_user_listing_reports_standing_and_what_it_would_touch() {
    use puna_core::model::user::{self, UserStatus};

    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;
        room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "one", generation, OWNER),
        )
        .await
        .expect("create");

        user::set_status(&mut conn, PLAYER, UserStatus::Banned, Some("why"), OWNER)
            .await
            .expect("ban");

        let listed = user::list(&mut conn).await.expect("list");
        let owner = listed.iter().find(|u| u.id == OWNER).expect("the owner");
        let player = listed.iter().find(|u| u.id == PLAYER).expect("the player");

        assert_eq!(owner.status(), UserStatus::Active);
        assert_eq!(owner.rooms_created, 1, "rooms they opened");

        assert_eq!(player.status(), UserStatus::Banned);
        assert_eq!(player.status_note.as_deref(), Some("why"));
        // Who did it, resolved through the self-join rather than left as a bare id.
        assert!(
            player
                .changed_by_name
                .as_deref()
                .is_some_and(|n| !n.is_empty()),
            "the listing does not say who applied the sanction"
        );

        // And after a restore the actor survives while the reason does not: "restored by X" is
        // worth keeping, the reason for a sanction that no longer applies is not.
        user::set_status(&mut conn, PLAYER, UserStatus::Active, None, OWNER)
            .await
            .expect("restore");
        let listed = user::list(&mut conn).await.expect("list");
        let player = listed.iter().find(|u| u.id == PLAYER).expect("the player");
        assert_eq!(player.status(), UserStatus::Active);
        assert_eq!(
            player.status_note, None,
            "a restored account still shows a reason"
        );
        assert!(
            player.changed_by_name.is_some(),
            "a restored account no longer says who restored it"
        );
        assert_eq!(player.rooms_created, 0);
    })
    .await;
}

/// **Locking a slot withholds it from the room without touching its credential.**
///
/// The two are deliberately independent: the lock is expressed as an omission from
/// `PAHOA_SLOT_PASSWORDS`, and the password stays in the row so unlocking restores the credential
/// its holder already has rather than minting one somebody then has to deliver.
#[tokio::test]
async fn locking_a_slot_withholds_it_without_disturbing_its_password() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;

        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "lockable", generation, OWNER),
        )
        .await
        .expect("create");
        room::set_slot_auth(&mut conn, id, SlotAuth::PerSlot)
            .await
            .expect("per-slot mode");

        let before = slot::get(&mut conn, id, 1)
            .await
            .expect("read")
            .expect("slot");
        assert!(!before.is_locked(), "a fresh slot is not locked");
        assert!(before.password.is_some(), "per-slot mode mints a password");

        assert!(
            slot::set_locked(&mut conn, id, 1, true, OWNER)
                .await
                .expect("lock"),
            "the first lock must report that the row moved"
        );

        let locked = slot::get(&mut conn, id, 1)
            .await
            .expect("read")
            .expect("slot");
        assert!(locked.is_locked());
        assert_eq!(locked.locked_by, Some(OWNER));
        assert_eq!(
            locked.password, before.password,
            "locking must not touch the credential -- unlocking restores what the holder has"
        );

        // Nobody else is disturbed: this is one slot's door, not the room's.
        let others = slot::list(&mut conn, id).await.expect("list");
        assert_eq!(
            others.iter().filter(|s| s.is_locked()).count(),
            1,
            "locking one slot locked another"
        );

        // **Locking an already-locked slot keeps the ORIGINAL timestamp and actor.** Rewriting them
        // to whoever pressed last would lose the answer to "who decided this", which is the whole
        // reason these are a timestamp and an id rather than a boolean.
        assert!(
            !slot::set_locked(&mut conn, id, 1, true, PLAYER)
                .await
                .expect("lock again"),
            "a repeat lock reported a change, so callers would rewrite the Secret for nothing"
        );
        let again = slot::get(&mut conn, id, 1)
            .await
            .expect("read")
            .expect("slot");
        assert_eq!(again.locked_at, locked.locked_at);
        assert_eq!(again.locked_by, Some(OWNER), "the actor was overwritten");

        // And unlocking gives the slot back its original credential, unchanged.
        assert!(
            slot::set_locked(&mut conn, id, 1, false, OWNER)
                .await
                .expect("unlock")
        );
        let unlocked = slot::get(&mut conn, id, 1)
            .await
            .expect("read")
            .expect("slot");
        assert!(!unlocked.is_locked());
        assert_eq!(unlocked.locked_by, None);
        assert_eq!(unlocked.password, before.password);

        // A no-op unlock reports nothing moved, for the same reason a repeat lock does.
        assert!(
            !slot::set_locked(&mut conn, id, 1, false, OWNER)
                .await
                .expect("unlock again")
        );
    })
    .await;
}

/// **The lobby import never takes a slot off somebody who already holds it.**
///
/// The guard is `owner_id IS NULL` **inside the UPDATE**, not a filter the caller applies, and the
/// difference is a real window rather than a style preference: the plan is computed from a roster
/// read a moment earlier and from a lobby answer that is older still, so a player who used their
/// claim link in between must win. Deciding it in the statement makes the check and the write one
/// operation.
///
/// Nothing above this layer can see the difference — a caller-side filter passes every unit test in
/// `lobby::plan` and loses the race only under load, where the symptom is a player who claimed a
/// slot and then did not have it.
#[tokio::test]
async fn importing_owners_never_overwrites_a_slot_somebody_already_claimed() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        users(&mut conn).await;
        let generation = seed_generation(&mut conn, false).await;
        let id = room::create(
            &mut conn,
            &NewRoom::direct(Environment::Dev, "Friday async", generation, OWNER),
        )
        .await
        .expect("create");

        let slots = slot::list(&mut conn, id).await.expect("slots");
        assert!(slots.len() >= 2, "this fixture needs two slots");

        // The player got there first, through their claim link.
        let token = slots[0].claim_token.clone().expect("token");
        slot::claim(&mut conn, &token, PLAYER).await.expect("claim");

        // The import runs anyway, naming somebody else for that same slot.
        let claimed = slot::claim_for_owners(
            &mut conn,
            id,
            &[(slots[0].slot_number, OWNER), (slots[1].slot_number, OWNER)],
        )
        .await
        .expect("import");

        assert_eq!(claimed, 1, "only the unowned slot is claimed");

        let after = slot::list(&mut conn, id).await.expect("slots");
        assert_eq!(
            after[0].owner_id,
            Some(PLAYER),
            "the player who claimed it keeps it"
        );
        assert_eq!(after[1].owner_id, Some(OWNER), "the free slot was imported");

        // And the imported slot's link is spent, exactly as an ordinary claim spends it: leaving it
        // live would let whoever it was sent to take a slot that now belongs to somebody.
        assert!(
            after[1].claim_token.is_none(),
            "an imported slot keeps a working claim link"
        );
        assert!(
            after[1].claimed_at.is_some(),
            "an imported slot has no claim time"
        );
    })
    .await;
}
