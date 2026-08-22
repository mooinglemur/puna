-- Slots staff have shut out of the room, without disturbing anybody else in it.
--
-- ## The lock IS the omission, and that is pahoa's rule rather than an invention here
--
-- `PAHOA_SLOT_PASSWORDS` fails closed: the variable's presence puts a room in per-slot mode, and a
-- slot missing from the map is **refused**. So Puna already had a way to bar one slot -- what it did
-- not have was a way to tell a deliberate omission from an accidental one. The Secret builder
-- refuses any partial map, precisely because a slot missing by mistake is somebody locked out of
-- their own game with nothing to explain it.
--
-- This column is that distinction. A slot with no password and no `locked_at` is still the accident
-- the builder refuses; a locked slot is left out of the map on purpose.
--
-- ## Timestamp and actor rather than a boolean
--
-- The same reasoning as `rooms.pinned_at` and `users.status_changed_by`: a lock is a decision
-- somebody made about somebody else, and the question a week later is who and when. A `locked
-- BOOLEAN` answers neither, and would have grown these two columns the first time anyone asked.
--
-- ## The password is deliberately NOT cleared
--
-- Locking omits the slot from the map and leaves `room_slots.password` exactly as it was, so
-- unlocking restores the credential the player already holds rather than minting one somebody then
-- has to deliver to them. It also means a lock is reversible without touching a secret at all.
ALTER TABLE room_slots
    ADD COLUMN locked_at TIMESTAMPTZ,
    -- Not cascading: the staff member who locked it may later be removed from the room, and losing
    -- the record of who acted is worse than a dangling reference.
    ADD COLUMN locked_by BIGINT REFERENCES users(id);

COMMENT ON COLUMN room_slots.locked_at IS
    'Set when staff barred this slot from connecting. The slot is then omitted from '
    'PAHOA_SLOT_PASSWORDS, which pahoa treats as a refusal. NULL is the ordinary case. Does not '
    'clear the slot''s password: unlocking restores the credential its holder already has.';
