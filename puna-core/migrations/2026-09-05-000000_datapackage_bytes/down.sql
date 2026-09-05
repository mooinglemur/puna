-- Derived from the seed, so dropping it loses nothing that cannot be recomputed: every value here
-- comes back from `wire_size_estimate` on the same file. What it costs while absent is that rooms
-- size their outbound budget from slot count alone, which is the behavior this column exists to fix.
ALTER TABLE generations DROP COLUMN datapackage_bytes;
