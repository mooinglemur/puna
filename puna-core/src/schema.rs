// @generated automatically by Diesel CLI.

pub mod sql_types {
    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "command_state"))]
    pub struct CommandState;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "gate_mode"))]
    pub struct GateMode;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "puna_environment"))]
    pub struct PunaEnvironment;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "room_desired_state"))]
    pub struct RoomDesiredState;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "room_role"))]
    pub struct RoomRole;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "room_source"))]
    pub struct RoomSource;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "room_state"))]
    pub struct RoomState;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "slot_auth_mode"))]
    pub struct SlotAuthMode;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "slot_kind"))]
    pub struct SlotKind;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "spoiler_policy"))]
    pub struct SpoilerPolicy;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "tracker_policy"))]
    pub struct TrackerPolicy;

    #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "user_status"))]
    pub struct UserStatus;
}

diesel::table! {
    creator_allowlist (user_id) {
        user_id -> Int8,
        note -> Nullable<Text>,
        added_by -> Nullable<Int8>,
        added_at -> Timestamptz,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::PunaEnvironment;

    fleet (environment) {
        environment -> PunaEnvironment,
        pahoa_image -> Text,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    generation_game_names (generation_id, game) {
        generation_id -> Uuid,
        game -> Text,
        item_names -> Jsonb,
        location_names -> Jsonb,
    }
}

diesel::table! {
    generation_slot_locations (generation_id, slot_number) {
        generation_id -> Uuid,
        slot_number -> Int4,
        location_ids -> Array<Nullable<Int8>>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::SlotKind;

    generation_slots (generation_id, slot_number) {
        generation_id -> Uuid,
        slot_number -> Int4,
        player_name -> Text,
        game -> Text,
        kind -> SlotKind,
        patch_member -> Nullable<Text>,
        patch_size_bytes -> Nullable<Int8>,
    }
}

diesel::table! {
    generation_uploads (generation_id, user_id) {
        generation_id -> Uuid,
        user_id -> Int8,
        uploaded_at -> Timestamptz,
    }
}

diesel::table! {
    generations (id) {
        id -> Uuid,
        sha256 -> Bytea,
        size_bytes -> Int8,
        seed_name -> Text,
        slots -> Int4,
        locations -> Int8,
        games -> Array<Nullable<Text>>,
        race_mode -> Bool,
        spoiler_member -> Nullable<Text>,
        min_server_version -> Nullable<Text>,
        first_ingested_by -> Nullable<Int8>,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::PunaEnvironment;

    port_ranges (environment) {
        environment -> PunaEnvironment,
        base_low -> Int4,
        base_high -> Int4,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::PunaEnvironment;

    port_reservations (environment, base_port) {
        environment -> PunaEnvironment,
        base_port -> Int4,
        room_id -> Nullable<Uuid>,
        last_activity -> Timestamptz,
        quarantined_until -> Nullable<Timestamptz>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::RoomRole;
    use super::sql_types::CommandState;

    room_commands (id) {
        id -> Uuid,
        room_id -> Uuid,
        requested_by -> Int8,
        requested_role -> RoomRole,
        command -> Jsonb,
        state -> CommandState,
        result -> Nullable<Jsonb>,
        error -> Nullable<Text>,
        requested_at -> Timestamptz,
        started_at -> Nullable<Timestamptz>,
        finished_at -> Nullable<Timestamptz>,
        batch_id -> Nullable<Uuid>,
    }
}

diesel::table! {
    room_events (id) {
        id -> Int8,
        room_id -> Uuid,
        at -> Timestamptz,
        actor -> Text,
        kind -> Text,
        detail -> Jsonb,
    }
}

diesel::table! {
    room_filters (room_id) {
        room_id -> Uuid,
        rules -> Jsonb,
        set_by -> Nullable<Int8>,
        set_at -> Timestamptz,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::RoomRole;

    room_invites (token) {
        token -> Text,
        room_id -> Uuid,
        role -> RoomRole,
        created_by -> Int8,
        created_at -> Timestamptz,
        expires_at -> Nullable<Timestamptz>,
        uses_remaining -> Nullable<Int4>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::RoomRole;

    room_members (room_id, user_id) {
        room_id -> Uuid,
        user_id -> Int8,
        role -> RoomRole,
        added_by -> Nullable<Int8>,
        added_at -> Timestamptz,
    }
}

diesel::table! {
    room_slot_filters (room_id, slot_number) {
        room_id -> Uuid,
        slot_number -> Int4,
        rules -> Jsonb,
        set_by -> Nullable<Int8>,
        set_at -> Timestamptz,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::SlotKind;

    room_slots (room_id, slot_number) {
        room_id -> Uuid,
        slot_number -> Int4,
        player_name -> Text,
        game -> Text,
        kind -> SlotKind,
        password -> Nullable<Text>,
        owner_id -> Nullable<Int8>,
        claim_token -> Nullable<Text>,
        claimed_at -> Nullable<Timestamptz>,
        tracker_id -> Uuid,
        locked_at -> Nullable<Timestamptz>,
        locked_by -> Nullable<Int8>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::PunaEnvironment;
    use super::sql_types::RoomSource;
    use super::sql_types::RoomDesiredState;
    use super::sql_types::SpoilerPolicy;
    use super::sql_types::TrackerPolicy;
    use super::sql_types::SlotAuthMode;
    use super::sql_types::RoomState;

    rooms (id) {
        id -> Uuid,
        lock_key -> Int4,
        environment -> PunaEnvironment,
        name -> Text,
        generation_id -> Uuid,
        source -> RoomSource,
        lobby_room_id -> Nullable<Uuid>,
        lobby_job_id -> Nullable<Text>,
        idempotency_key -> Nullable<Text>,
        cloned_from -> Nullable<Uuid>,
        created_by -> Nullable<Int8>,
        created_at -> Timestamptz,
        desired_state -> RoomDesiredState,
        desired_at -> Timestamptz,
        wants_filtered -> Bool,
        spoiler_policy -> SpoilerPolicy,
        tracker_id -> Uuid,
        tracker_policy -> TrackerPolicy,
        slot_auth -> SlotAuthMode,
        password -> Nullable<Text>,
        server_password -> Nullable<Text>,
        use_embedded_options -> Bool,
        save_interval_secs -> Int4,
        state -> RoomState,
        state_changed_at -> Timestamptz,
        provisioned_at -> Nullable<Timestamptz>,
        secret_synced_at -> Nullable<Timestamptz>,
        deployment_uid -> Nullable<Text>,
        spec_hash -> Nullable<Text>,
        advertised_host -> Nullable<Text>,
        advertised_port -> Nullable<Int4>,
        advertised_filtered_port -> Nullable<Int4>,
        started_at -> Nullable<Timestamptz>,
        stopped_at -> Nullable<Timestamptz>,
        last_error -> Nullable<Text>,
        failure_count -> Int4,
        retry_after -> Nullable<Timestamptz>,
        not_ready_sweeps -> Int4,
        admin_token -> Text,
        admin_token_rotated_at -> Timestamptz,
        clients_connected -> Nullable<Int4>,
        last_activity_at -> Nullable<Timestamptz>,
        probed_at -> Nullable<Timestamptz>,
        probe_kind -> Nullable<Text>,
        last_tracker_doc -> Nullable<Jsonb>,
        last_tracker_at -> Nullable<Timestamptz>,
        running_image -> Nullable<Text>,
        deployment_created_at -> Nullable<Timestamptz>,
        process_started_at -> Nullable<Timestamptz>,
        desired_spec_hash -> Nullable<Text>,
        redeploy_requested_at -> Nullable<Timestamptz>,
        pinned_at -> Nullable<Timestamptz>,
        pinned_by -> Nullable<Int8>,
        last_check_at -> Nullable<Timestamptz>,
        journal_id -> Uuid,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::GateMode;

    settings (key) {
        key -> Text,
        mode -> GateMode,
        detail -> Jsonb,
        updated_at -> Timestamptz,
        updated_by -> Nullable<Int8>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::UserStatus;

    users (id) {
        id -> Int8,
        username -> Text,
        first_seen_at -> Timestamptz,
        last_seen_at -> Timestamptz,
        status -> UserStatus,
        status_note -> Nullable<Text>,
        status_changed_at -> Nullable<Timestamptz>,
        status_changed_by -> Nullable<Int8>,
    }
}

diesel::joinable!(creator_allowlist -> users (added_by));
diesel::joinable!(generation_game_names -> generations (generation_id));
diesel::joinable!(generation_slot_locations -> generations (generation_id));
diesel::joinable!(generation_slots -> generations (generation_id));
diesel::joinable!(generation_uploads -> generations (generation_id));
diesel::joinable!(generation_uploads -> users (user_id));
diesel::joinable!(generations -> users (first_ingested_by));
diesel::joinable!(port_reservations -> rooms (room_id));
diesel::joinable!(room_commands -> rooms (room_id));
diesel::joinable!(room_commands -> users (requested_by));
diesel::joinable!(room_events -> rooms (room_id));
diesel::joinable!(room_filters -> rooms (room_id));
diesel::joinable!(room_filters -> users (set_by));
diesel::joinable!(room_invites -> rooms (room_id));
diesel::joinable!(room_invites -> users (created_by));
diesel::joinable!(room_members -> rooms (room_id));
diesel::joinable!(room_slot_filters -> users (set_by));
diesel::joinable!(room_slots -> rooms (room_id));
diesel::joinable!(rooms -> generations (generation_id));
diesel::joinable!(settings -> users (updated_by));

diesel::allow_tables_to_appear_in_same_query!(
    creator_allowlist,
    fleet,
    generation_game_names,
    generation_slot_locations,
    generation_slots,
    generation_uploads,
    generations,
    port_ranges,
    port_reservations,
    room_commands,
    room_events,
    room_filters,
    room_invites,
    room_members,
    room_slot_filters,
    room_slots,
    rooms,
    settings,
    users,
);
