-- How much of a room's journal somebody who is not staff may read.
--
-- The journal already rides on `tracker_policy` for *reachability* -- `/journal/<id>` answers only
-- where `may_see_tracker` does -- and that answers the wrong question on its own. A feed link is
-- handed to an audience the organizers did not choose, and the file behind it carries every line
-- anybody typed in the room. Whether that audience gets the item feed, the whole history, or
-- nothing at all is a judgment about THIS room, not one Puna can make for every room from a single
-- constant.
--
-- 'disabled'  the journal is staff-only; a non-organizer gets the same 404 an unknown id gets
-- 'feed'      `check` and `gap` over the socket, everything else withheld and COUNTED; no download
-- 'full'      the history as pahoa wrote it, and the download with it
--
-- An organizer reads everything under all three: this column says what the tier BELOW them gets.
CREATE TYPE journal_policy AS ENUM ('disabled', 'feed', 'full');

-- **Existing rooms default to 'feed', which is exactly what they do today**, so the migration
-- widens nothing. New rooms take their default from the seed's `race_mode` at creation, the same
-- way `tracker_policy` and `spoiler_policy` already do -- and that is a decision in Rust rather
-- than here, because it reads a column on `generations`.
ALTER TABLE rooms
    ADD COLUMN journal_policy journal_policy NOT NULL DEFAULT 'feed';

COMMENT ON COLUMN rooms.journal_policy IS
    'How much of history.jsonl a NON-ORGANIZER may read, on top of tracker_policy deciding whether '
    'they may reach the feed at all: disabled = 404, feed = check/gap with the rest withheld and '
    'counted, full = the file as written plus the download. Organizers always read everything.';
