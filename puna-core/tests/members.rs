//! Postgres-backed tests for the room role ladder, membership and invites.
//!
//! Authorization, so asserted rather than reasoned about. Two properties here are enforced by the
//! database rather than by Rust and would otherwise never be exercised at all: the last-organizer
//! trigger, and the conditional `UPDATE` that makes an invite's last use unraceable.

mod common;

use common::{insert_generation, insert_room, with_db};
use puna_core::model::member::{self, MemberError, RoomRole};
use puna_core::model::user;

const OWNER: i64 = 10;
const HELPER: i64 = 20;
const OTHER: i64 = 30;
const STRANGER: i64 = 40;

/// A room with `OWNER` as its sole organizer, which is the state room creation leaves behind.
async fn room_with_owner(conn: &mut diesel_async::AsyncPgConnection) -> puna_core::ids::RoomId {
    for id in [OWNER, HELPER, OTHER, STRANGER] {
        user::ensure_exists(conn, id).await.expect("user");
    }
    let generation = insert_generation(conn).await;
    let room = insert_room(conn, generation, "idle").await;
    member::set_role(conn, room, OWNER, RoomRole::Organizer, None)
        .await
        .expect("first organizer");
    room
}

#[tokio::test]
async fn roles_resolve_and_strangers_hold_nothing() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = room_with_owner(&mut conn).await;

        member::set_role(&mut conn, room, HELPER, RoomRole::Helper, Some(OWNER))
            .await
            .expect("add helper");

        assert_eq!(
            member::role_of(&mut conn, room, OWNER)
                .await
                .expect("owner"),
            Some(RoomRole::Organizer)
        );
        assert_eq!(
            member::role_of(&mut conn, room, HELPER)
                .await
                .expect("helper"),
            Some(RoomRole::Helper)
        );
        // `role_of` answers what the roster says, nothing more. A global admin's bypass lives in
        // the web tier so that the roster page does not quietly omit the people who can act.
        assert_eq!(
            member::role_of(&mut conn, room, STRANGER)
                .await
                .expect("stranger"),
            None
        );

        let listed = member::list(&mut conn, room).await.expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].role, RoomRole::Organizer, "organizers first");
        assert_eq!(listed[0].user_id, OWNER);
        assert_eq!(listed[1].added_by, Some(OWNER));
    })
    .await;
}

/// The invariant a room cannot recover from on its own.
#[tokio::test]
async fn removing_the_last_organizer_is_refused() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = room_with_owner(&mut conn).await;
        member::set_role(&mut conn, room, HELPER, RoomRole::Helper, Some(OWNER))
            .await
            .expect("add helper");

        // A helper is not a second organizer, so the room still has exactly one.
        let err = member::remove(&mut conn, room, OWNER)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, MemberError::LastOrganizer),
            "the trigger must surface as a rule, not a database fault: {err:?}"
        );

        // The demotion path trips the same trigger, and is the one easy to forget.
        let err = member::set_role(&mut conn, room, OWNER, RoomRole::Helper, Some(OWNER))
            .await
            .expect_err("must refuse");
        assert!(matches!(err, MemberError::LastOrganizer), "{err:?}");

        assert_eq!(
            member::role_of(&mut conn, room, OWNER)
                .await
                .expect("still"),
            Some(RoomRole::Organizer),
            "a refused demotion must not have applied"
        );

        // With a second organizer, both operations are ordinary.
        member::set_role(&mut conn, room, OTHER, RoomRole::Organizer, Some(OWNER))
            .await
            .expect("promote");
        assert!(
            member::remove(&mut conn, room, OWNER)
                .await
                .expect("remove")
        );
        assert_eq!(
            member::role_of(&mut conn, room, OWNER).await.expect("gone"),
            None
        );
    })
    .await;
}

/// Deleting the room takes its members with it, which is not an orphaning.
///
/// Worth its own test because the trigger has to distinguish the two, and getting it wrong makes
/// rooms undeletable -- a failure that would only show up at the end of M5.
#[tokio::test]
async fn deleting_a_room_does_not_trip_the_organizer_trigger() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = room_with_owner(&mut conn).await;

        diesel_async::RunQueryDsl::execute(
            diesel::sql_query("DELETE FROM rooms WHERE id = $1")
                .bind::<diesel::sql_types::Uuid, _>(room),
            &mut conn,
        )
        .await
        .expect("a room with one organizer must still be deletable");

        assert_eq!(
            member::role_of(&mut conn, room, OWNER).await.expect("gone"),
            None
        );
    })
    .await;
}

#[tokio::test]
async fn an_invite_grants_its_role_and_can_be_revoked() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = room_with_owner(&mut conn).await;

        let token = member::create_invite(&mut conn, room, RoomRole::Helper, OWNER, None, None)
            .await
            .expect("mint");

        let listed = member::list_invites(&mut conn, room).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].role, RoomRole::Helper);
        assert_eq!(listed[0].uses_remaining, None, "unlimited by default");

        let (granted_room, role) = member::redeem_invite(&mut conn, &token, HELPER)
            .await
            .expect("redeem");
        assert_eq!(granted_room, room);
        assert_eq!(role, RoomRole::Helper);
        assert_eq!(
            member::role_of(&mut conn, room, HELPER)
                .await
                .expect("held"),
            Some(RoomRole::Helper)
        );

        assert!(
            member::revoke_invite(&mut conn, room, &token)
                .await
                .expect("revoke")
        );
        let err = member::redeem_invite(&mut conn, &token, OTHER)
            .await
            .expect_err("revoked");
        assert!(matches!(err, MemberError::NoSuchInvite), "{err:?}");
    })
    .await;
}

/// A link is an offer of access, not an instruction to take some away.
#[tokio::test]
async fn redeeming_a_lesser_invite_does_not_demote() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = room_with_owner(&mut conn).await;
        member::set_role(&mut conn, room, OTHER, RoomRole::Organizer, Some(OWNER))
            .await
            .expect("second organizer");

        let token = member::create_invite(&mut conn, room, RoomRole::Helper, OWNER, None, None)
            .await
            .expect("mint");
        let (_, role) = member::redeem_invite(&mut conn, &token, OTHER)
            .await
            .expect("redeem");

        assert_eq!(
            role,
            RoomRole::Organizer,
            "an organizer following a helper link stays an organizer"
        );
        assert_eq!(
            member::role_of(&mut conn, room, OTHER).await.expect("held"),
            Some(RoomRole::Organizer)
        );
    })
    .await;
}

#[tokio::test]
async fn an_expired_or_spent_invite_is_refused_distinguishably() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = room_with_owner(&mut conn).await;

        let expired = member::create_invite(
            &mut conn,
            room,
            RoomRole::Helper,
            OWNER,
            Some(chrono::Utc::now() - chrono::Duration::hours(1)),
            None,
        )
        .await
        .expect("mint");
        let err = member::redeem_invite(&mut conn, &expired, HELPER)
            .await
            .expect_err("expired");
        // Spent, not missing: the link was real, so the message should say it is used up rather
        // than send someone hunting for a typo.
        assert!(matches!(err, MemberError::InviteSpent), "{err:?}");

        let once = member::create_invite(&mut conn, room, RoomRole::Helper, OWNER, None, Some(1))
            .await
            .expect("mint");
        member::redeem_invite(&mut conn, &once, HELPER)
            .await
            .expect("first use");
        let err = member::redeem_invite(&mut conn, &once, OTHER)
            .await
            .expect_err("second use");
        assert!(matches!(err, MemberError::InviteSpent), "{err:?}");

        assert_eq!(
            member::role_of(&mut conn, room, OTHER)
                .await
                .expect("other"),
            None,
            "a spent invite must grant nothing"
        );

        let err = member::redeem_invite(&mut conn, "not-a-real-token", HELPER)
            .await
            .expect_err("missing");
        assert!(matches!(err, MemberError::NoSuchInvite), "{err:?}");
    })
    .await;
}

/// Two people race the last use of a link. Exactly one may win.
///
/// This is what the conditional `UPDATE ... WHERE uses_remaining > 0` buys: a `SELECT` followed by
/// an `UPDATE` would let both readers see one use remaining and both proceed.
#[tokio::test]
async fn concurrent_redemptions_cannot_overspend_an_invite() {
    with_db(|pool| async move {
        let mut setup = pool.get().await.expect("connection");
        let room = room_with_owner(&mut setup).await;

        const CLAIMANTS: i64 = 16;
        for id in 100..100 + CLAIMANTS {
            user::ensure_exists(&mut setup, id).await.expect("user");
        }

        // One use, sixteen claimants.
        let token = member::create_invite(&mut setup, room, RoomRole::Helper, OWNER, None, Some(1))
            .await
            .expect("mint");
        drop(setup);

        let mut tasks = Vec::new();
        for id in 100..100 + CLAIMANTS {
            let pool = pool.clone();
            let token = token.clone();
            tasks.push(tokio::spawn(async move {
                let mut conn = pool.get().await.expect("connection");
                member::redeem_invite(&mut conn, &token, id).await.is_ok()
            }));
        }

        let mut winners = 0;
        for task in tasks {
            if task.await.expect("join") {
                winners += 1;
            }
        }
        assert_eq!(winners, 1, "exactly one redemption may succeed");

        let mut conn = pool.get().await.expect("connection");
        let members = member::list(&mut conn, room).await.expect("list");
        assert_eq!(
            members.len(),
            2,
            "the owner plus exactly one redeemer: {members:#?}"
        );
    })
    .await;
}
