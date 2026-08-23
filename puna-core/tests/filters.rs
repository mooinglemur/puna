//! Postgres-backed tests for traffic filters.
//!
//! The property worth a database is the one a type alone cannot hold: **an empty ruleset and no
//! ruleset are different states**, and they have to stay different across a write and a read. Lose
//! that and "everybody gets thinned except this one" becomes unsayable, which is the whole reason
//! pahoa separated them.

mod common;

use common::{insert_generation, insert_room, with_db};
use puna_core::ids::RoomId;
use puna_core::model::filter::{self, Direction, Effective, Kind, Rule, SlotFilter};
use puna_core::model::user;

const TROY: i64 = 100;

fn bounce(tag: &str, p: Option<f64>) -> Rule {
    Rule {
        direction: Direction::FromSlot,
        kind: Kind::Bounce,
        tag: Some(tag.into()),
        subtype: None,
        p,
    }
}

async fn a_room(conn: &mut diesel_async::AsyncPgConnection) -> RoomId {
    use diesel::sql_types::{Integer, Text, Uuid as SqlUuid};
    use diesel_async::RunQueryDsl;

    user::upsert(conn, TROY, "troy").await.expect("a user");
    let generation = insert_generation(conn).await;
    let room = insert_room(conn, generation, "running").await;

    // Slots, because `room_slot_filters` is keyed to them: a filter naming a slot that does not
    // exist is one pahoa would answer 404 to, and the foreign key says so here.
    for n in 1..=3 {
        diesel::sql_query(
            "INSERT INTO room_slots (room_id, slot_number, player_name, game, kind, tracker_id)
             VALUES ($1, $2, $3, 'Archipelago', 'player', gen_random_uuid())",
        )
        .bind::<SqlUuid, _>(room)
        .bind::<Integer, _>(n)
        .bind::<Text, _>(format!("Player{n}"))
        .execute(conn)
        .await
        .expect("slot");
    }
    room
}

/// **The three states, across a write and a read.**
///
/// `[]` and "no row" are the pair that matters: they are the only way to say "filtered by nothing
/// even though the room filters", and a nullable column or a bare `Vec` would collapse them.
#[tokio::test]
async fn an_empty_slot_ruleset_survives_as_something_other_than_no_ruleset() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        // Nothing set: every slot follows the room, and none of them has a row.
        assert_eq!(
            filter::slot_filter(&mut conn, room, 1).await.expect("read"),
            SlotFilter::Follows
        );
        assert!(
            filter::slot_filters(&mut conn, room)
                .await
                .expect("read")
                .is_empty(),
            "a slot that follows the room has no row, which is what makes the chip cheap"
        );

        let own = SlotFilter::Own(vec![bounce("TrapLink", Some(0.5))]);
        filter::set_slot_filter(&mut conn, room, 1, &own, TROY)
            .await
            .expect("write own");
        filter::set_slot_filter(&mut conn, room, 2, &SlotFilter::Exempt, TROY)
            .await
            .expect("write exempt");

        assert_eq!(
            filter::slot_filter(&mut conn, room, 1).await.expect("read"),
            own
        );
        assert_eq!(
            filter::slot_filter(&mut conn, room, 2).await.expect("read"),
            SlotFilter::Exempt,
            "an empty ruleset must not read back as no ruleset"
        );
        assert_eq!(
            filter::slot_filter(&mut conn, room, 3).await.expect("read"),
            SlotFilter::Follows
        );

        // The roster's one query: only the divergent slots, in order.
        let diverging = filter::slot_filters(&mut conn, room).await.expect("read");
        assert_eq!(
            diverging.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![1, 2],
            "slot 3 follows the room, so it is not in the list and gets no chip"
        );

        // Back to following: the row goes, and the state is the one it started in.
        filter::set_slot_filter(&mut conn, room, 1, &SlotFilter::Follows, TROY)
            .await
            .expect("clear");
        assert_eq!(
            filter::slot_filter(&mut conn, room, 1).await.expect("read"),
            SlotFilter::Follows
        );
        assert_eq!(
            filter::slot_filters(&mut conn, room)
                .await
                .expect("read")
                .len(),
            1,
            "only the exempt slot is left diverging"
        );
    })
    .await;
}

/// The room's ruleset, and what the roster warning reads from.
#[tokio::test]
async fn a_room_filter_reports_exactly_the_slots_it_will_not_reach() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        let room = a_room(&mut conn).await;

        assert_eq!(
            filter::room_filter(&mut conn, room).await.expect("read"),
            None,
            "no row means the room does not filter"
        );

        let thin = vec![bounce("DeathLink", Some(0.75))];
        filter::set_room_filter(&mut conn, room, &thin, TROY)
            .await
            .expect("write");
        assert_eq!(
            filter::room_filter(&mut conn, room).await.expect("read"),
            Some(thin.clone())
        );

        // Slot 2 opts out entirely; slot 3 replaces the room's rules with its own. Both are slots
        // the room's filter no longer describes, in opposite directions.
        filter::set_slot_filter(&mut conn, room, 2, &SlotFilter::Exempt, TROY)
            .await
            .expect("exempt");
        filter::set_slot_filter(
            &mut conn,
            room,
            3,
            &SlotFilter::Own(vec![bounce("TrapLink", None)]),
            TROY,
        )
        .await
        .expect("own");

        let missed = filter::slot_filters(&mut conn, room).await.expect("read");
        assert_eq!(
            missed.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![2, 3],
            "editing the room's filter reaches neither of these, and the warning says so"
        );

        // **The effective set, which is Puna's whole job here.** Slot 1 gets the room's thinning;
        // slot 3 does not, because its own rules REPLACED them rather than adding to them.
        let room_rules = filter::room_filter(&mut conn, room)
            .await
            .expect("read")
            .unwrap_or_default();
        let follows = Effective::of(
            &room_rules,
            &filter::slot_filter(&mut conn, room, 1).await.expect("read"),
        );
        assert_eq!(follows.rules, thin);
        assert!(follows.from_room);

        let owns = Effective::of(
            &room_rules,
            &filter::slot_filter(&mut conn, room, 3).await.expect("read"),
        );
        assert_eq!(owns.rules.len(), 1);
        assert_eq!(owns.rules[0].tag.as_deref(), Some("TrapLink"));
        assert!(
            !owns
                .rules
                .iter()
                .any(|r| r.tag.as_deref() == Some("DeathLink")),
            "a slot with its own rules is NOT also thinned by the room's"
        );

        // Clearing the room's filter removes the row rather than storing an empty one: with nothing
        // above it to inherit from, the two are the same thing.
        filter::clear_room_filter(&mut conn, room)
            .await
            .expect("clear");
        assert_eq!(
            filter::room_filter(&mut conn, room).await.expect("read"),
            None
        );
        filter::set_room_filter(&mut conn, room, &[], TROY)
            .await
            .expect("empty");
        assert_eq!(
            filter::room_filter(&mut conn, room).await.expect("read"),
            None,
            "an empty room ruleset is no ruleset, unlike a slot's"
        );
    })
    .await;
}
