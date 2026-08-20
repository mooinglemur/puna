-- The tracker's name cache.
--
-- Item and location NAMES, plus each slot's own location list, extracted from a generation's
-- multidata so the tracker tier can render them. The tier has no `puna-data` mount and is not
-- getting one -- it serves no artifacts, and that omission is a property worth keeping -- so the
-- one thing it cannot read is the seed these come out of.
--
-- ## Every row here is DERIVED DATA, and saying so is what makes the design safe
--
-- All of it is rebuildable from `generations/<sha>/seed.archipelago`. Losing the lot costs a
-- rebuild, not a room: a missing row degrades to rendering the raw id, which is what the reference
-- implementation does too (`Unknown Item (ID: n)`). Nothing may ever become authoritative here.
--
-- ## Keyed by generation, NEVER by game alone
--
-- The load-bearing decision. These names come out of a datapackage embedded in an *uploaded zip*,
-- so a malformed or hostile one must not be able to rename items in somebody else's room. Scoped
-- per generation, its blast radius is exactly the blast radius the zip already had: itself. A
-- shared `game -> names` table would have been smaller and would have made one bad upload everyone
-- else's problem.

CREATE TABLE generation_game_names (
  generation_id UUID NOT NULL REFERENCES generations(id) ON DELETE CASCADE,
  game TEXT NOT NULL,
  -- `{"<id>": "<name>"}`. JSONB rather than a row per name: a lookup table is read whole or not at
  -- all, and a large seed would otherwise put hundreds of thousands of rows here to answer one
  -- question. Postgres TOASTs and compresses these, which is most of why the size is tolerable.
  item_names JSONB NOT NULL,
  location_names JSONB NOT NULL,
  PRIMARY KEY (generation_id, game)
);

-- Every location in a slot's OWN world, checked or not.
--
-- This is what lets Puna show the locations a slot has *not* checked, where a tracker holding only
-- the live document can report the checked set and nothing else.
--
-- **THE ITEM AT EACH LOCATION IS DELIBERATELY ABSENT.** `MultiData.locations` carries
-- `(location, item, receiver, flags)` per entry, and the last three are the seed's central spoiler
-- -- the answer to "what is in that chest". Only the location id is copied, so the spoiler cannot
-- leak through a table that never held it. This is a structural property, not a filter applied
-- later, and it is the reason this column is not simply the multidata's own array.
CREATE TABLE generation_slot_locations (
  generation_id UUID NOT NULL REFERENCES generations(id) ON DELETE CASCADE,
  slot_number INTEGER NOT NULL,
  location_ids BIGINT[] NOT NULL,
  PRIMARY KEY (generation_id, slot_number)
);
