-- Who uploaded a generation becomes a SET, because deduplication made it one.
--
-- `generations.sha256` is UNIQUE and the directory on disk is named after the same hash, so two
-- people uploading the same zip converge on one row and one copy of the bytes. That is right and
-- stays. What was wrong is that the row could only remember ONE of them: `first_ingested_by` is a
-- single column, and `list_for_user` read it, so the second uploader's file successfully landed and
-- then did not appear in their uploads. They were left holding a URL and nothing else.
--
-- ## The dedup notice was also a disclosure, and this table is what fixes that
--
-- The upload route reported "these exact contents were already on file" whenever the INSERT
-- conflicted -- which is a GLOBAL fact. Shown to a second uploader it says *somebody else has this
-- seed*, which is not theirs to learn: they came with their own copy of the bytes and are entitled
-- to know what THEY have done with them, not what anyone else has.
--
-- With a row per (generation, user), "already uploaded" becomes a question about the caller alone.
-- Same account twice: the insert below conflicts, one reference, and they are told. Second account:
-- the insert succeeds, they get a reference, and the page reads exactly as it does for a first
-- upload -- because from their side it IS one.
--
-- ## This is the reference count a delete must consult
--
-- Nothing deletes generations today. When something does, the rule this table exists to make
-- expressible is: **removing a user's upload removes their row here; it removes the BYTES only when
-- no row remains.** A user must be able to drop their own upload without destroying a seed somebody
-- else also holds -- and without being able to detect that somebody else holds it, which is the same
-- property as above wearing different clothes. The cascade below deletes references when a
-- generation goes; it deliberately does not work in the other direction.

CREATE TABLE generation_uploads (
    generation_id UUID   NOT NULL REFERENCES generations(id) ON DELETE CASCADE,
    -- No cascade to users on purpose: a departing account's uploads are somebody's decision, not a
    -- side effect. Deleting a user with references left is refused, which is the loud failure.
    user_id       BIGINT NOT NULL REFERENCES users(id),
    -- THIS user's upload time, not the generation's. They differ by however long the seed sat here
    -- under somebody else's account, and showing the generation's would date a second uploader's
    -- brand new entry to a day they were not involved in -- both wrong and a leak.
    uploaded_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- One reference per person per generation. This is what makes a repeat upload converge instead
    -- of accumulating, and it is the whole mechanism: the route reads whether the insert conflicted.
    PRIMARY KEY (generation_id, user_id)
);

-- The listing's query: one user's uploads, newest first.
CREATE INDEX generation_uploads_user_idx ON generation_uploads (user_id, uploaded_at DESC);

-- Backfilled from `first_ingested_by`, which this table generalizes. Every existing generation has
-- exactly one uploader by construction, so the backfill is total and nobody loses a reference.
--
-- `created_at` is the honest `uploaded_at` for these rows: for a generation with one uploader the
-- two ARE the same moment.
INSERT INTO generation_uploads (generation_id, user_id, uploaded_at)
    SELECT id, first_ingested_by, created_at
      FROM generations
     WHERE first_ingested_by IS NOT NULL;

-- `generations.first_ingested_by` is KEPT, and demoted to what its name already said: who got here
-- first. It is provenance, the way `rooms.created_by` is -- authority now lives in the table above.
COMMENT ON COLUMN generations.first_ingested_by IS
    'Provenance only: who uploaded these bytes first. Authority over who holds a reference is '
    'generation_uploads. Never use this to scope a listing or to decide whether data may be deleted.';
