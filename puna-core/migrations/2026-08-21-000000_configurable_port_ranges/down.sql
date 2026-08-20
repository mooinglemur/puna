-- Restores the hardcoded partition. This will FAIL if any reservation now sits outside the two
-- original ranges, which is correct: those rooms hold ports the restored constraint says cannot
-- exist, and silently dropping the constraint's guarantee to make the rollback succeed would be
-- worse than the rollback failing.
ALTER TABLE port_reservations
    ADD CONSTRAINT port_reservations_check
    CHECK ((environment = 'dev'  AND base_port BETWEEN 40000 AND 44998)
        OR (environment = 'prod' AND base_port BETWEEN 45000 AND 49998));

DROP TRIGGER IF EXISTS port_reservations_within_range ON port_reservations;
DROP FUNCTION IF EXISTS port_reservation_within_range();
DROP TABLE IF EXISTS port_ranges;
