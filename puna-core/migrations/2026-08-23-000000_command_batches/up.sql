-- Commands enqueued together by one bulk action, so their answers can be read together.
--
-- ## Why a batch needs an identity at all
--
-- A bulk action over a sync of hundreds of slots is not one command with many targets -- it is many
-- commands, each of which the room answers separately, and **some of them will not succeed while
-- the rest do**. That is the whole reason this exists: without a shared id there is no way to ask
-- "how did the thing I just did go?", only "what are the last twenty commands in this room?", which
-- for a two-hundred-slot release is the same question asked uselessly.
--
-- ## A column rather than a `command_batches` table
--
-- Every fact a batch header would hold is already on its rows and identical across them: who asked
-- (`requested_by`), under what authority (`requested_role`), when (`requested_at`), and what
-- (`command`, whose `name()` is the action). A table would restate all four and then have to be
-- kept in step with them.
--
-- The one thing it would buy is a batch that survives enqueueing **zero** commands -- an operator
-- who staged an empty selection, or whose every target vanished between staging and submitting.
-- That is a page saying "nothing to do", which the route can answer without a row.
--
-- ## NULL is the ordinary case and always will be
--
-- Every command from the console and from the room page's moderation column is its own thing, and
-- nothing should ever group them after the fact. `NULL` means exactly "not part of a bulk action",
-- which is why the index is partial: it carries only the rows a batch page will ever ask for.
ALTER TABLE room_commands
    ADD COLUMN batch_id UUID;

-- Partial, because batched rows are the rare case and this index exists for exactly one query:
-- every row of one batch, oldest first, which is the order the panel staged them in.
CREATE INDEX room_commands_batch_idx ON room_commands (batch_id, requested_at)
    WHERE batch_id IS NOT NULL;

COMMENT ON COLUMN room_commands.batch_id IS
    'Groups commands enqueued together by one bulk action so their outcomes can be read as a set. '
    'NULL for every ordinary command, which is the common case. Deliberately not a foreign key: '
    'there is no batches table, because every fact one would hold is already on these rows.';
