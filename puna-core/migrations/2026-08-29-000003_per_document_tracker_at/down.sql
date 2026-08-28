-- Back to one timestamp for both documents, taking the NEWER of the two so a rollback cannot make
-- a document look older than it is. It can make one look newer, which is the defect this migration
-- exists to remove -- there is no way to collapse two facts into one column without reintroducing
-- it, and saying so is better than pretending the rollback is lossless.
UPDATE rooms
   SET last_tracker_at = GREATEST(
         COALESCE(last_tracker_at, '-infinity'::timestamptz),
         COALESCE(last_static_tracker_at, '-infinity'::timestamptz))
 WHERE last_tracker_at IS NOT NULL OR last_static_tracker_at IS NOT NULL;

UPDATE rooms SET last_tracker_at = NULL WHERE last_tracker_at = '-infinity'::timestamptz;

COMMENT ON COLUMN rooms.last_tracker_at IS NULL;
ALTER TABLE rooms DROP COLUMN last_static_tracker_at;
