-- Reverting drops the locks themselves, which is the honest behavior: without the column there is
-- no way to express one, and a room reverted to the previous schema lets every locked slot back in
-- at its next start. Worth knowing before running this against an environment with live rooms.
ALTER TABLE room_slots
    DROP COLUMN locked_by,
    DROP COLUMN locked_at;
