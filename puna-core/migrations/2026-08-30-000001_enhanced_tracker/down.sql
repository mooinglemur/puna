-- Dropping these loses what participants wrote, which is real data rather than a cache: a note is
-- somebody's own words and nothing rebuilds it. The columns go before the types they use.
ALTER TABLE room_slots
  DROP CONSTRAINT note_is_bounded,
  DROP CONSTRAINT note_is_absent_or_real,
  DROP COLUMN annotated_by,
  DROP COLUMN annotated_at,
  DROP COLUMN note,
  DROP COLUMN progression;

DROP TABLE room_ping_preferences;

DROP TYPE ping_preference;
DROP TYPE progression_status;

ALTER TABLE rooms DROP COLUMN enhanced_tracker;
