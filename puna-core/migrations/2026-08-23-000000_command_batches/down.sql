-- Reverting loses only the grouping, never a command: every row keeps its own state, result and
-- error, and the console's history renders them exactly as it did before batches existed. What goes
-- is the ability to ask which bulk action a row belonged to.
DROP INDEX IF EXISTS room_commands_batch_idx;

ALTER TABLE room_commands
    DROP COLUMN batch_id;
