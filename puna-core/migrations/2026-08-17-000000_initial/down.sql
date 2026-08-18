-- Reverse of up.sql. Tables before types, dependents before dependencies.
--
-- This exists so `diesel migration redo` works in development and so the migration is testable
-- in both directions in CI. It is NOT an operational rollback: dropping these tables destroys
-- every port reservation, which is the one piece of state that cannot be reconstructed from
-- Kubernetes -- a torn-down room's port lives only here.

DROP TABLE IF EXISTS room_events;
DROP TABLE IF EXISTS port_reservations;
DROP TABLE IF EXISTS room_commands;
DROP TABLE IF EXISTS room_slots;
DROP TABLE IF EXISTS room_invites;

DROP TRIGGER IF EXISTS room_members_keep_an_organizer ON room_members;
DROP FUNCTION IF EXISTS forbid_removing_last_organizer();
DROP TABLE IF EXISTS room_members;

DROP TABLE IF EXISTS rooms;
DROP TABLE IF EXISTS generation_slots;
DROP TABLE IF EXISTS generations;
DROP TABLE IF EXISTS creator_allowlist;
DROP TABLE IF EXISTS settings;
DROP TABLE IF EXISTS users;

DROP TYPE IF EXISTS room_desired_state;
DROP TYPE IF EXISTS room_state;
DROP TYPE IF EXISTS slot_auth_mode;
DROP TYPE IF EXISTS command_state;
DROP TYPE IF EXISTS room_role;
DROP TYPE IF EXISTS gate_mode;
DROP TYPE IF EXISTS tracker_policy;
DROP TYPE IF EXISTS spoiler_policy;
DROP TYPE IF EXISTS room_source;
DROP TYPE IF EXISTS puna_environment;
