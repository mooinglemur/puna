DROP INDEX users_status_idx;

ALTER TABLE users
    DROP COLUMN status_changed_by,
    DROP COLUMN status_changed_at,
    DROP COLUMN status_note,
    DROP COLUMN status;

DROP TYPE user_status;
