-- Account standing, for /admin/users.
--
-- Three states rather than a boolean, because the middle one is the useful sanction and a `banned`
-- flag would have grown into this the first time somebody wanted it. They are ordered by severity
-- and each withholds strictly more than the last:
--
--   active      -- everything.
--   restricted  -- may play: claim a slot, download a patch, run a room's console, start an idle
--                  room. May NOT create a room or upload a generation. Enforced in exactly one
--                  place, `CanCreateRoom`, which is already the only door onto both.
--   banned      -- may not log in, and an existing session is refused. Nothing is deleted: their
--                  rooms, slots and memberships are untouched, because a ban is a statement about
--                  the person and not about the games other people are mid-way through.
CREATE TYPE user_status AS ENUM ('active', 'restricted', 'banned');

ALTER TABLE users
    ADD COLUMN status            user_status NOT NULL DEFAULT 'active',
    -- Why, in the words of whoever did it. Shown on the admin table and to the banned person
    -- themselves: a sanction nobody can be told the reason for is one nobody can appeal.
    ADD COLUMN status_note       TEXT,
    ADD COLUMN status_changed_at TIMESTAMPTZ,
    -- Deliberately nullable and NOT cascading: the admin who set it may themselves be removed, and
    -- losing the record of who acted would be worse than a dangling name.
    ADD COLUMN status_changed_by BIGINT REFERENCES users(id);

COMMENT ON COLUMN users.status IS
    'Account standing. `restricted` withholds room creation and generation upload only; `banned` '
    'refuses login and every authenticated request. Never deletes anything.';

-- The admin listing sorts newest-seen first and the guard looks up by primary key, so the only
-- query needing help is "who is not active", which is a small minority of a small table.
CREATE INDEX users_status_idx ON users (status) WHERE status <> 'active';
