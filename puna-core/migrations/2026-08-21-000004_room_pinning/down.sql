DROP INDEX rooms_reapable_idx;

ALTER TABLE rooms
    DROP COLUMN pinned_by,
    DROP COLUMN pinned_at;
