-- Losing this column costs a reading, never a room: it is written by the probe pass and rebuilt on
-- the next one, and nothing decides anything from it.
ALTER TABLE rooms DROP COLUMN gameplay_options;
