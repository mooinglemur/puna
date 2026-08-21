-- Postgres cannot remove a value from an enum, so the type is rebuilt without it.
--
-- **Closed rooms become stopped, which loses the gate rather than the room.** That is the honest
-- mapping: `stopped` is what closed means to everything except the start route, so a rolled-back
-- deployment leaves every room torn down exactly as it was -- and reopens the ones an organizer had
-- closed. Nothing else could be true, since the column would no longer have a value to say it in.
--
-- The rename-then-recreate dance is the standard one, and the `USING ...::text::...` cast is what
-- carries the surviving values across two types that share no identity.

ALTER TABLE rooms ALTER COLUMN desired_state DROP DEFAULT;

UPDATE rooms SET desired_state = 'stopped' WHERE desired_state = 'closed';

ALTER TYPE room_desired_state RENAME TO room_desired_state_old;
CREATE TYPE room_desired_state AS ENUM ('running', 'stopped', 'deleted');

ALTER TABLE rooms
    ALTER COLUMN desired_state TYPE room_desired_state
    USING desired_state::text::room_desired_state;

ALTER TABLE rooms ALTER COLUMN desired_state SET DEFAULT 'stopped';

DROP TYPE room_desired_state_old;
