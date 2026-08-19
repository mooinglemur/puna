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
use puna_core::Environment;
use puna_core::artifact::SlotKind;
use puna_core::ids::{GenerationId, RoomId};
use puna_core::model::member::{self, RoomRole};
use puna_core::model::room::{
    self, DesiredState, NewRoom, Relationship, SlotAuth, SpoilerPolicy, TrackerPolicy,
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
        assert_eq!(stored.desired_state, "stopped", "a new room starts stopped");
        assert_eq!(stored.state, "provisioning");
        // Not a race, so the permissive defaults.
        assert_eq!(stored.spoiler_policy, SpoilerPolicy::AdminOnly);
        assert_eq!(stored.tracker_policy, TrackerPolicy::Link);
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
        assert_eq!(stored.spoiler_policy, SpoilerPolicy::Never);
        assert_eq!(stored.tracker_policy, TrackerPolicy::Members);
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
