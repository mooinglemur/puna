-- Per-room and per-slot traffic filters: which of a slot's messages the room drops.
--
-- ## Puna owns the INTENT; pahoa owns the behavior
--
-- The same division `room_slots.locked_at` already draws, and for stronger reasons. pahoa persists
-- filters in `room.save` -- which is why its save format went to 3 -- so a save reset or a recreated
-- PVC takes every filter with it, silently, leaving a room that quietly stops filtering. pahoa also
-- records the rules and nothing about **who** set them or when, which for a control that changes
-- what a player can see is the half an operator needs a week later.
--
-- So these tables are the authority and the orchestrator re-asserts them whenever a room reaches
-- `running`, exactly as it re-applies locks. pahoa's `PATCH` is idempotent by construction and its
-- handoff says so in as many words -- "a reconcile loop can assert its intent every pass without
-- growing the filter" -- which is that side designing for this one.
--
-- ## The room's filter and a slot's are INDEPENDENT, and Puna does not merge them
--
-- pahoa's rule is that a slot's ruleset **replaces** the room's rather than adding to it. Puna could
-- have hidden that behind a maintained union, and deliberately does not: two authorities, one per
-- scope, and Puna's only job across the boundary is to *show* what the effective set would be.
--
-- The cost is that the replacement is easy to walk into -- add one rule to a slot and the room's
-- rules stop reaching it -- so that is a thing the UI must say out loud at the moment of editing,
-- rather than a thing this schema tries to prevent.
--
-- ## Three states for a slot, and the absent row is one of them
--
--   row absent          follows the room's filter, whatever it is
--   rules = '[]'        EXEMPT: filtered by nothing, even when the room filters
--   rules = '[...]'     its own rules, INSTEAD of the room's
--
-- `[]` and "no row" are genuinely different and the difference is the only way to say "everybody
-- gets thinned except this one". A nullable `rules` column would have collapsed two of the three
-- into one ambiguous value, so the column is NOT NULL and absence carries the third state.
--
-- A room has no such distinction: with nothing above it to inherit from, an empty ruleset and no
-- ruleset are the same thing, so the room's row is simply absent when it does not filter.
CREATE TABLE room_filters (
    room_id UUID PRIMARY KEY REFERENCES rooms(id) ON DELETE CASCADE,
    -- Validated in Rust before it lands here. JSONB rather than typed columns for pahoa's own
    -- reason: the matcher vocabulary is open-ended, and a new matcher should be a new optional
    -- field rather than a migration on both sides.
    rules JSONB NOT NULL,
    -- Nullable and not cascading, like `room_slots.locked_by`: the person who set a filter may
    -- later leave the room, and losing the record of who acted is worse than a dangling reference.
    set_by BIGINT REFERENCES users(id),
    set_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE room_slot_filters (
    room_id UUID NOT NULL,
    slot_number INTEGER NOT NULL,
    rules JSONB NOT NULL,
    set_by BIGINT REFERENCES users(id),
    set_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (room_id, slot_number),
    -- To the SLOT, not just the room: a filter naming a slot that does not exist is a filter pahoa
    -- would answer `404` to, and the roster could not render it against anything.
    FOREIGN KEY (room_id, slot_number)
        REFERENCES room_slots (room_id, slot_number) ON DELETE CASCADE
);

COMMENT ON TABLE room_filters IS
    'The room-wide traffic filter, or no row when the room does not filter. Puna is the authority: '
    'pahoa keeps its copy in room.save, which a save reset destroys, and records no actor.';

COMMENT ON COLUMN room_slot_filters.rules IS
    'THREE STATES, and the absent row is one of them. No row: this slot follows the room''s filter. '
    '''[]'': exempt, filtered by nothing even when the room filters. Non-empty: these rules INSTEAD '
    'of the room''s -- pahoa replaces rather than merges, and Puna does not paper over that.';
