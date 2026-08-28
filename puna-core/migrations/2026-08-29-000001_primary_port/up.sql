-- Which of a room's two ports the room page leads with.
--
-- **An enum rather than a boolean, and next to `wants_filtered` that is not fussiness.** That
-- column already decides whether the filtered port is published at all; a second bool beside it
-- would leave `wants_filtered = false, filtered_primary = true` spellable and meaningless, and no
-- name short enough to fit a column would keep the two apart in a reader's head.
--
-- 'full'      the standard address leads; the filtered one is behind a click
-- 'filtered'  the filtered address leads; the full one is behind a click
--
-- Defaulted at creation from the seed's slot count -- filtered for 200 and up -- because that is
-- where a game client starts drowning in other people's item traffic, and the failure is silent:
-- somebody on the wrong port plays happily and concludes the multiworld is dead.
CREATE TYPE primary_port AS ENUM ('full', 'filtered');

-- 'full' for every existing room, which is what they all do today.
ALTER TABLE rooms
    ADD COLUMN primary_port primary_port NOT NULL DEFAULT 'full';

COMMENT ON COLUMN rooms.primary_port IS
    'Which port the room page shows prominently. The other is rendered behind a collapsed heading '
    'with its own wording, never in the same position -- so the address on offer always says which '
    'of the two it is.';
