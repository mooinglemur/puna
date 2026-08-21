-- When a slot in this room last registered a genuinely NEW location check.
--
-- The reference server's own idle signal (`MultiServer.py:2671-2682`), which pahoa mirrors per slot
-- and reports room-wide as `activity.last_check_at` (its P23). This is what the reaper measures,
-- where `last_activity_at` beside it moves on any packet at all and therefore stays fresh in a room
-- where everybody is chatting and nobody is playing.
--
-- NULL means no slot has ever checked anything -- a real answer, not a gap: a room whose organizer
-- is still getting people connected has that shape, and it is measured from the room's start
-- instead. Never read as a check at the epoch.
ALTER TABLE rooms
    ADD COLUMN last_check_at TIMESTAMPTZ;

COMMENT ON COLUMN rooms.last_check_at IS
    'Room-wide newest NEW-location-check time, from pahoa activity.last_check_at. Persisted by '
    'pahoa across a room restart, so a room stopped for days reports days of check-idle on its '
    'return -- which is why the reaper floors this at how long the room has been up.';
