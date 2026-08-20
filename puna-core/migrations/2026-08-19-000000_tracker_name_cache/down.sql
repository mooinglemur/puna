-- Dropping these loses nothing that cannot be rebuilt from the seeds on disk, which is the whole
-- claim the `up` side makes. The tracker degrades to rendering raw ids until a rebuild runs.
DROP TABLE IF EXISTS generation_slot_locations;
DROP TABLE IF EXISTS generation_game_names;
