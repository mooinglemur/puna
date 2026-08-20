-- Everything here is observed, derived, or a pending request. Dropping it loses no room state: the
-- observed columns are rewritten by the next tick, `desired_spec_hash` by the next hourly lane, and
-- `fleet.pahoa_image` by the orchestrator at startup.
--
-- The one real loss is a `redeploy_requested_at` that was set and not yet acted on, which is a
-- button somebody will have to press again.
DROP TABLE IF EXISTS fleet;

DROP INDEX IF EXISTS rooms_redeploy_idx;

ALTER TABLE rooms
  DROP COLUMN IF EXISTS redeploy_requested_at,
  DROP COLUMN IF EXISTS desired_spec_hash,
  DROP COLUMN IF EXISTS process_started_at,
  DROP COLUMN IF EXISTS deployment_created_at,
  DROP COLUMN IF EXISTS running_image;
