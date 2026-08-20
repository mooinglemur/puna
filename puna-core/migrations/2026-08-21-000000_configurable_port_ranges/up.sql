-- The port range becomes a property of the deployment rather than of the code.
--
-- `port_reservations` was created with the two ranges written into a CHECK constraint, and
-- pre-seeded from the same literals. That made the dev/prod partition a database-enforced fact,
-- which was the right instinct -- an overlap is the one mistake here that is unrecoverable, because
-- two environments sharing one public address share one port space and the loser ends up reachable
-- at an address DNS never mentions.
--
-- But the literals were one deployment's answer. Which ports a deployment owns depends on what else
-- shares its address, what its firewall permits, and how the space was divided; none of that is
-- Puna's to decide. So the range moves into configuration, and the guard moves with it: recorded
-- per environment in `port_ranges`, enforced by a trigger against whatever is recorded.
--
-- ## What this can and cannot guard, stated plainly
--
-- A database holds ONE environment, so it can only ever check its own range. The old CHECK carried
-- both ranges in every database and would therefore have caught a dev database handed a prod port.
-- That is genuinely lost. **Non-overlap between environments is now the deployment's to get right**,
-- alongside the addresses it already owns. What is kept: nothing can reserve a port outside the
-- range its own environment declared, and a range cannot be narrowed out from under a room that
-- already holds a port in the part being removed.

CREATE TABLE port_ranges (
    environment puna_environment PRIMARY KEY,
    -- Inclusive, and both even: these are BASE ports, and each room takes `base` and `base + 1`.
    base_low    INTEGER NOT NULL,
    base_high   INTEGER NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    CHECK (base_low % 2 = 0),
    CHECK (base_high % 2 = 0),
    CHECK (base_low <= base_high)
);

-- Backfilled from what is already seeded, so an existing database keeps exactly the range it has
-- been running with and nothing observable changes at the moment this migration applies. The
-- orchestrator reconciles it against configuration at startup.
INSERT INTO port_ranges (environment, base_low, base_high)
    SELECT environment, min(base_port), max(base_port)
      FROM port_reservations
     GROUP BY environment;

-- Replaces the hardcoded CHECK. A function rather than a constraint because it reads another table,
-- which a CHECK may not do.
CREATE FUNCTION port_reservation_within_range() RETURNS trigger AS $$
DECLARE
    low  INTEGER;
    high INTEGER;
BEGIN
    SELECT base_low, base_high INTO low, high
      FROM port_ranges WHERE environment = NEW.environment;

    -- No configured range is a refusal, not a pass. An environment whose range has not been
    -- recorded is one nobody has told which ports it owns, and guessing is how two environments
    -- end up on the same port.
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'no port range recorded for environment %; the orchestrator writes it at startup',
            NEW.environment;
    END IF;

    IF NEW.base_port < low OR NEW.base_port > high THEN
        RAISE EXCEPTION 'base_port % is outside the % range %..%',
            NEW.base_port, NEW.environment, low, high;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- `UPDATE OF base_port` rather than `UPDATE`: the allocator writes `room_id` and `last_activity` on
-- every allocation and never touches `base_port`, so this costs nothing on the hot path and fires
-- only when a row is created or a port is moved.
CREATE TRIGGER port_reservations_within_range
    BEFORE INSERT OR UPDATE OF base_port ON port_reservations
    FOR EACH ROW EXECUTE FUNCTION port_reservation_within_range();

-- The generated name for the partition CHECK, verified against the live schema. Deliberately NOT
-- `IF EXISTS`: if this name is ever wrong, the old constraint survives and silently rejects every
-- port outside the two ranges it was born with, which would present as a configured range that
-- half-works. Failing the migration is the better outcome.
--
-- `port_reservations_base_port_check` -- the `base_port % 2 = 0` rule -- is a different constraint
-- and stays.
ALTER TABLE port_reservations DROP CONSTRAINT port_reservations_check;
