-- The enhanced tracker: annotations a room's own participants write on its tracker page.
--
-- Emulating the parts of Cheese Trackers that are worth having, without the integration that was
-- rejected -- it scrapes the WebHost's HTML and expects that format, which Puna does not produce and
-- will not start producing. What it offers an async is a place to say "I am blocked", "ping me", and
-- a line of context, so this stores those directly instead.
--
-- **Everything here is participant-only, and the room toggle is a second gate on top of that.** An
-- anonymous viewer of a `link`-policy tracker sees exactly what it saw before, toggle or no toggle.
-- Nothing below is rendered to somebody who is neither the room's staff nor holding a slot in it.

-- Off by default, and off means the tracker behaves exactly as it did. Its own column rather than a
-- fourth value on `tracker_policy`: the policy answers *who may see the tracker*, and this answers
-- *what the tracker is for*. Folding them together would make "public tracker without annotations"
-- and "member tracker with them" two points on one line, when they are independent choices.
ALTER TABLE rooms ADD COLUMN enhanced_tracker BOOLEAN NOT NULL DEFAULT false;

COMMENT ON COLUMN rooms.enhanced_tracker IS
  'Whether participants may annotate their slots on the tracker. Off means the tracker renders '
  'exactly as it did before this existed. Orthogonal to tracker_policy, which decides who may look.';

-- --------------------------------------------------------------------------------------------
-- Per-slot annotations.
--
-- **Columns on `room_slots` rather than a table of their own**, decided on a measurement rather
-- than on taste: the tracker's `digestible()` already calls `slot::list` on *every* request, so
-- these cost zero extra queries on the highest-volume polled path in the system -- the tracker
-- tier, four replicas, polled once per open tab. A side table would add a query there to save a
-- little tidiness here.
--
-- The cost is that `room_slots` rows are copied by the clone path, which makes carrying stale BK
-- status into a fresh playthrough the DEFAULT rather than a decision. It is not carried -- a clone
-- inserts fresh slots and only owners are copied over them -- and there is a test pinning that,
-- because the failure is a new room describing the old one's progress.
-- --------------------------------------------------------------------------------------------
CREATE TYPE progression_status AS ENUM ('unknown', 'unblocked', 'bk', 'soft_bk', 'go_mode');

ALTER TABLE room_slots
  ADD COLUMN progression progression_status NOT NULL DEFAULT 'unknown',
  ADD COLUMN note TEXT,
  -- Who last changed either field and when. One pair for both, because the question an operator
  -- asks is "who touched this slot's annotations", never "who set the progression specifically" --
  -- and staff may edit a player's, which is exactly when somebody wants to know.
  ADD COLUMN annotated_at TIMESTAMPTZ,
  ADD COLUMN annotated_by BIGINT REFERENCES users(id),
  -- **Emptying the note deletes it, and this is what makes that a property of the column** rather
  -- than a rule every writer has to remember. An empty string is unspellable, so absence is the
  -- only way to say nothing and no reader has to treat '' and NULL as the same thing.
  ADD CONSTRAINT note_is_absent_or_real CHECK (note IS NULL OR btrim(note) <> ''),
  -- Characters, not bytes: the limit must not depend on the alphabet somebody writes in, the same
  -- rule `room::validate_name` follows. Enforced here as well as in the route because this column
  -- is rendered into a hover panel on a page that polls.
  ADD CONSTRAINT note_is_bounded CHECK (note IS NULL OR char_length(note) <= 1000);

COMMENT ON COLUMN room_slots.note IS
  'A participant-visible line of context, written by the slot owner or by staff. NULL, never an '
  'empty string. At most 1000 characters. Untrusted text: render with textContent, never innerHTML.';

-- --------------------------------------------------------------------------------------------
-- Ping preference.
--
-- **Per person per room, not per slot**, because it is a statement about how somebody wants to be
-- contacted rather than about a world: "you do not mind being pinged about this multiworld". A
-- player holding three slots has one answer, and all three of their chips show it.
--
-- Its own table because a slot owner is **not** a `room_members` row -- membership is staff -- so
-- there is no existing per-person-per-room row to hang this on.
--
-- No row means `unknown`, which is the default answer, so the common case stores nothing.
-- --------------------------------------------------------------------------------------------
CREATE TYPE ping_preference AS ENUM ('no', 'unknown', 'see_notes', 'for_hints', 'yes');

CREATE TABLE room_ping_preferences (
  room_id UUID NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  -- No cascade on the user, matching `room_members`: a departing account is a decision rather than
  -- something a foreign key makes quietly.
  user_id BIGINT NOT NULL REFERENCES users(id),
  preference ping_preference NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (room_id, user_id)
);

COMMENT ON TABLE room_ping_preferences IS
  'How each participant wants to be contacted about one room. Absent means unknown. Only the '
  'person themselves sets this -- staff may edit notes and progression, never somebody''s stated '
  'willingness to be pinged.';
