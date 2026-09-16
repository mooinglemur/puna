-- Whether anybody signed in may take an unclaimed slot in this room, without holding its claim link.
--
-- The existing path is a per-slot unguessable token that staff mint and hand out: the link IS the
-- authorization, and it names one slot. That is right for a room whose roster was decided before it
-- opened, and it is the wrong shape for the other way a multiworld fills up, where an organizer
-- posts the room and people take a world. Today that costs the organizer one minted link per person
-- per slot, delivered by hand, and the slots sit unclaimed until they do it.
--
-- --- OFF BY DEFAULT, WHICH IS EXACTLY THE BEHAVIOR EVERY EXISTING ROOM HAS --------------------
-- There is a real case for the other default: somebody holding the room's link can already see the
-- roster, and letting them sign in and take a free slot is what most people mean by sharing it. The
-- case against is what decides it here: turning this on is a widening, and
-- a widening that arrives by migration is one nobody chose. Every room that exists was created on
-- the promise that a slot changes hands only when staff hand out a link, and some of those rooms
-- are races.
--
-- So it is `false` for every existing room and every new one, and an organizer turns it on. The
-- same reasoning `enhanced_tracker` records: off is how the room behaved before the column existed.
--
-- --- WHAT IT DOES NOT CHANGE -------------------------------------------------------------------
-- It admits somebody to an UNCLAIMED slot and nothing else. A slot that is already held is not
-- available to anybody, and the room's staff keep the only way to take one back (`slot::release`,
-- which mints a fresh link, so the person being replaced cannot walk back in on the old one).
--
-- It says nothing about who may SEE the room: anybody holding `/room/<id>` can view it either way,
-- and the unguessable id is what stands between a room and the internet. This decides only whether
-- looking at the roster is enough to join it.
--
-- **Deliberately not the word "public", anywhere this is explained to an organizer.** It reads as
-- *discoverable*, which is false and is the opposite of what they expect, and a reader who believes
-- the room is already findable will mis-weigh exactly the decision this column asks them to make.
--
-- And nothing here reaches pahoa, so it is a live option rather than a restart: turning it on
-- changes what the next page render offers and what one route admits.
ALTER TABLE rooms ADD COLUMN open_claims BOOLEAN NOT NULL DEFAULT false;

COMMENT ON COLUMN rooms.open_claims IS
    'Whether any signed-in user may claim an unclaimed slot without holding its claim link. False '
    'by default, which is how every room behaved before this existed. Applies only to slots nobody '
    'holds; releasing one is still staff-only, and it is deliberately absent from the spec hash '
    'because pahoa never reads it.';
