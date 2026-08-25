//! Postgres-backed tests for the room-creation gates.
//!
//! This is an authorization decision, so it is asserted as a full matrix rather than sampled: a
//! gate that is wrong in one cell of `mode x caller x source` is a gate that lets the wrong person
//! create rooms, and nothing about the wrong answer looks like a failure at the time.
//!
//! The forward-compatibility case is the one worth reading. `gate_mode` is a Postgres enum, so a
//! value this build does not recognize cannot be inserted by accident -- it can only arrive from a
//! database migrated ahead of the binary, which is exactly what a rollout does for a few minutes.
//! `a_gate_mode_this_build_does_not_know_is_refused` provokes that state deliberately.

mod common;

use common::with_db;
use puna_core::model::settings::{self, Decision, GateMode, Grant, Refusal};
use puna_core::model::{RoomSource, user};

const ADMIN: i64 = 100;
const ALLOWLISTED: i64 = 200;
const NOBODY: i64 = 300;

const SOURCES: [RoomSource; 2] = [RoomSource::Direct, RoomSource::Lobby];

/// Every caller the matrix distinguishes, and whether they hold global admin.
const CALLERS: [(&str, i64, bool); 3] = [
    ("admin", ADMIN, true),
    ("allowlisted", ALLOWLISTED, false),
    ("nobody", NOBODY, false),
];

fn expected(mode: GateMode, caller: &str) -> Decision {
    match (mode, caller) {
        // An admin bypasses every gate, in every mode. This is the whole of the initial testing
        // posture: both gates ship `disabled`, so a fresh deployment is admin-only by default.
        (_, "admin") => Decision::Allowed(Grant::Admin),

        (GateMode::Disabled, _) => Decision::Refused(Refusal::Disabled),
        (GateMode::Open, _) => Decision::Allowed(Grant::Open),

        (GateMode::Allowlist, "allowlisted") => Decision::Allowed(Grant::Allowlisted),
        (GateMode::Allowlist, _) => Decision::Refused(Refusal::NotAllowlisted),
    }
}

#[tokio::test]
async fn the_full_gate_matrix() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        for id in [ADMIN, ALLOWLISTED, NOBODY] {
            user::ensure_exists(&mut conn, id).await.expect("user");
        }
        settings::allow(&mut conn, ALLOWLISTED, Some("test fixture"), ADMIN)
            .await
            .expect("allowlist");

        for source in SOURCES {
            for mode in GateMode::ALL {
                settings::set_mode(&mut conn, source.settings_key(), mode, ADMIN)
                    .await
                    .expect("set gate");

                for (name, user_id, is_admin) in CALLERS {
                    let got = settings::evaluate(&mut conn, source, user_id, is_admin)
                        .await
                        .expect("evaluate");
                    assert_eq!(
                        got,
                        expected(mode, name),
                        "source={source:?} mode={mode:?} caller={name}"
                    );
                }
            }
        }
    })
    .await;
}

/// The posture a fresh deployment lands in, asserted directly rather than inferred from the
/// migration: admin-only, both sources, with no configuration step.
#[tokio::test]
async fn a_fresh_database_is_admin_only() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");

        for source in SOURCES {
            assert_eq!(
                settings::mode(&mut conn, source.settings_key())
                    .await
                    .expect("mode"),
                GateMode::Disabled,
                "{source:?} should ship disabled"
            );
            assert!(
                settings::evaluate(&mut conn, source, ADMIN, true)
                    .await
                    .expect("admin")
                    .is_allowed()
            );
            assert!(
                !settings::evaluate(&mut conn, source, NOBODY, false)
                    .await
                    .expect("nobody")
                    .is_allowed()
            );
        }
    })
    .await;
}

/// Two switches, not one. Disabling direct uploads must not disable the lobby's pipeline.
#[tokio::test]
async fn the_two_sources_are_independent() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        user::ensure_exists(&mut conn, ADMIN).await.expect("user");

        settings::set_mode(
            &mut conn,
            RoomSource::Direct.settings_key(),
            GateMode::Open,
            ADMIN,
        )
        .await
        .expect("open direct");

        assert!(
            settings::evaluate(&mut conn, RoomSource::Direct, NOBODY, false)
                .await
                .expect("direct")
                .is_allowed()
        );
        assert!(
            !settings::evaluate(&mut conn, RoomSource::Lobby, NOBODY, false)
                .await
                .expect("lobby")
                .is_allowed(),
            "opening direct must not open the lobby push"
        );
    })
    .await;
}

/// A gate that cannot be read must permit nothing.
///
/// The migration seeds both rows, so this state means one was deleted by hand -- and the tempting
/// reading, "no gate configured, so nothing is gated", would turn a deleted row into open room
/// creation.
#[tokio::test]
async fn a_missing_settings_row_is_refused() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        user::ensure_exists(&mut conn, ADMIN).await.expect("user");

        // Open it first, so the test cannot pass merely because the default is `disabled`.
        settings::set_mode(
            &mut conn,
            RoomSource::Direct.settings_key(),
            GateMode::Open,
            ADMIN,
        )
        .await
        .expect("open");
        assert!(
            settings::evaluate(&mut conn, RoomSource::Direct, NOBODY, false)
                .await
                .expect("open")
                .is_allowed()
        );

        diesel_async::RunQueryDsl::execute(
            diesel::sql_query("DELETE FROM settings WHERE key = 'room_creation.direct'"),
            &mut conn,
        )
        .await
        .expect("delete the gate");

        assert_eq!(
            settings::evaluate(&mut conn, RoomSource::Direct, NOBODY, false)
                .await
                .expect("missing"),
            Decision::Refused(Refusal::Disabled)
        );

        // ...and an admin can still act, which is the point of short-circuiting before the read:
        // the person who would repair this must not be locked out by it.
        assert!(
            settings::evaluate(&mut conn, RoomSource::Direct, ADMIN, true)
                .await
                .expect("admin")
                .is_allowed()
        );

        // The upsert in `set_mode` is what lets /admin/gates repair it without psql.
        settings::set_mode(
            &mut conn,
            RoomSource::Direct.settings_key(),
            GateMode::Open,
            ADMIN,
        )
        .await
        .expect("repair");
        assert!(
            settings::evaluate(&mut conn, RoomSource::Direct, NOBODY, false)
                .await
                .expect("repaired")
                .is_allowed()
        );
    })
    .await;
}

/// A database migrated ahead of the binary -- which every rollout produces for a few minutes.
///
/// The old binary meets a `gate_mode` value it has never heard of. It must refuse, not guess.
#[tokio::test]
async fn a_gate_mode_this_build_does_not_know_is_refused() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");

        // Postgres will not let this value be typed by accident, so provoke it the only way it
        // can really happen: add it to the enum, exactly as a future migration would.
        for statement in [
            "ALTER TYPE gate_mode ADD VALUE 'invite_only'",
            "UPDATE settings SET mode = 'invite_only' WHERE key = 'room_creation.direct'",
        ] {
            diesel_async::RunQueryDsl::execute(diesel::sql_query(statement), &mut conn)
                .await
                .unwrap_or_else(|e| panic!("{statement}: {e}"));
        }

        assert_eq!(
            settings::mode(&mut conn, "room_creation.direct")
                .await
                .expect("mode"),
            GateMode::Disabled,
            "an unknown mode must read as disabled, never as open"
        );
        assert_eq!(
            settings::evaluate(&mut conn, RoomSource::Direct, NOBODY, false)
                .await
                .expect("evaluate"),
            Decision::Refused(Refusal::Disabled)
        );
    })
    .await;
}

#[tokio::test]
async fn allowlist_entries_can_be_added_listed_and_revoked() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        user::ensure_exists(&mut conn, ADMIN).await.expect("user");
        settings::set_mode(
            &mut conn,
            RoomSource::Direct.settings_key(),
            GateMode::Allowlist,
            ADMIN,
        )
        .await
        .expect("allowlist mode");

        // Deliberately never registered as a user: an administrator must be able to authorize a
        // Discord id before its owner has ever logged in, so `creator_allowlist` has no FK.
        assert!(
            !settings::is_allowlisted(&mut conn, ALLOWLISTED)
                .await
                .expect("before")
        );
        settings::allow(&mut conn, ALLOWLISTED, Some("asked in #general"), ADMIN)
            .await
            .expect("allow");
        assert!(
            settings::is_allowlisted(&mut conn, ALLOWLISTED)
                .await
                .expect("after")
        );

        let listed = settings::allowlist(&mut conn).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].user_id, ALLOWLISTED);
        assert_eq!(listed[0].note.as_deref(), Some("asked in #general"));
        assert_eq!(listed[0].added_by, Some(ADMIN));

        assert_eq!(
            settings::evaluate(&mut conn, RoomSource::Direct, ALLOWLISTED, false)
                .await
                .expect("evaluate"),
            Decision::Allowed(Grant::Allowlisted)
        );

        assert!(
            settings::revoke(&mut conn, ALLOWLISTED)
                .await
                .expect("revoke")
        );
        assert!(
            !settings::revoke(&mut conn, ALLOWLISTED)
                .await
                .expect("revoke again"),
            "revoking twice should report that nothing was removed"
        );
        assert_eq!(
            settings::evaluate(&mut conn, RoomSource::Direct, ALLOWLISTED, false)
                .await
                .expect("evaluate"),
            Decision::Refused(Refusal::NotAllowlisted)
        );
    })
    .await;
}

#[tokio::test]
async fn setting_a_gate_records_who_did_it() {
    with_db(|pool| async move {
        let mut conn = pool.get().await.expect("connection");
        user::ensure_exists(&mut conn, ADMIN).await.expect("user");

        settings::set_mode(
            &mut conn,
            RoomSource::Lobby.settings_key(),
            GateMode::Allowlist,
            ADMIN,
        )
        .await
        .expect("set");

        let gates = settings::all(&mut conn).await.expect("all");
        let lobby = gates
            .iter()
            .find(|g| g.key == RoomSource::Lobby.settings_key())
            .expect("lobby gate");
        assert_eq!(lobby.mode, GateMode::Allowlist);
        assert_eq!(lobby.updated_by, Some(ADMIN));

        // Both seeded gates are listed, so /admin/gates cannot silently omit one.
        assert_eq!(gates.len(), 2, "{gates:#?}");
    })
    .await;
}
