-- The feed's own URL segment, independent of the room's id and of the tracker's.
--
-- Same argument the tracker's id already carries and for the same reason: `/room/<id>/journal`
-- embeds the room, so sharing the feed shared the room -- and the feed is the surface most likely to
-- be handed to an audience the organizers did not choose. A stream chat gets the feed; it does not
-- get the door to start, stop or configure the multiworld.
--
-- **Its own column rather than reusing `tracker_id`**, even though both are gated by
-- `tracker_policy` today. Reuse would make the two links derivable from each other, collapsing two
-- capabilities into one -- and the policy is a *current* answer to who may read each, where the id
-- is what makes them separately shareable at all. The same reasoning already gives a slot its own
-- tracker id rather than a path under the room's.
ALTER TABLE rooms
    ADD COLUMN journal_id UUID NOT NULL DEFAULT gen_random_uuid();

-- Backfilled by the DEFAULT above, so existing rooms get one without a second statement. The default
-- stays: every insert path should get one without naming it, and a room with no feed id would be a
-- room whose feed cannot be reached.
ALTER TABLE rooms ADD CONSTRAINT rooms_journal_id_key UNIQUE (journal_id);

COMMENT ON COLUMN rooms.journal_id IS
    'The feed URL segment: /journal/<journal_id>. Unguessable and bearer, the same capability class '
    'as rooms.id and rooms.tracker_id, and deliberately derivable from neither -- sharing a feed '
    'must not share the room or its tracker.';
