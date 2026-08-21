-- Rolling back LOSES references, and it cannot do otherwise.
--
-- `first_ingested_by` holds one uploader; this table holds all of them. Dropping it collapses that
-- set back to whoever happened to be first, so any second uploader silently stops seeing a
-- generation they uploaded. Nothing here can prevent that -- there is no column to put them in --
-- so the loss is stated rather than worked around.
--
-- The bytes are untouched either way: this only ever recorded who holds a reference, never what was
-- stored.

COMMENT ON COLUMN generations.first_ingested_by IS NULL;

DROP TABLE IF EXISTS generation_uploads;
