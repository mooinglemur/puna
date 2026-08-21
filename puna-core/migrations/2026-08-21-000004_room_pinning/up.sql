-- Rooms an administrator has exempted from the idle reaper.
--
-- Timestamps and an actor rather than a boolean, for the same reason `users.status_changed_by`
-- exists: "this room is pinned" is a decision somebody made, and six months later the useful
-- question is who and when. A `pinned BOOLEAN` answers neither and would have grown these two
-- columns the first time anybody asked.
--
-- NULL is the ordinary case, so the reaper's predicate is `pinned_at IS NULL` and an unpinned room
-- costs nothing to represent.
ALTER TABLE rooms
    ADD COLUMN pinned_at TIMESTAMPTZ,
    -- Not cascading: the admin who pinned it may be removed, and losing the record of who acted is
    -- worse than a dangling reference.
    ADD COLUMN pinned_by BIGINT REFERENCES users(id);

COMMENT ON COLUMN rooms.pinned_at IS
    'Set when an administrator exempted this room from the idle reaper. NULL means reapable. Does '
    'not stop anything else: a pinned room still stops, closes, redeploys and deletes on request.';

-- The reaper scans running rooms for one candidate per tick, and a pinned room is a small minority.
CREATE INDEX rooms_reapable_idx ON rooms (last_activity_at)
    WHERE pinned_at IS NULL AND state = 'running';
