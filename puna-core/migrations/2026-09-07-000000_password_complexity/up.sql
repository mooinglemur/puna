-- How long a password this room generates, for the three that a PERSON types.
--
-- 'low'     a1b2c              5 symbols, no separator
-- 'medium'  a1b2c-3d4e5       10 symbols, two groups
-- 'high'    a1b2c-3d4e5-f6g7h 15 symbols, three groups
--
-- The alphabet is 32 confusable-free symbols, so each is exactly five bits: 25, 50 and 75 bits.
-- pahoa rate-limits authentication failures to ten a minute per room, so even the floor is years of
-- guessing for one slot in one game, and the reason this is a choice at all is the other end of the
-- trade: these are typed by hand off a web page, often on a phone, sometimes by somebody debugging a
-- client that is mangling what it is given.
--
-- --- IT GOVERNS THREE CREDENTIALS AND DELIBERATELY NOT THE FOURTH ------------------------------
-- In: rooms.password (the room-wide one), room_slots.password (per slot), and rooms.server_password
-- (pahoa's `!admin login` gate). All three are read by a person and typed into a client.
--
-- Out: rooms.admin_token, which is not. It is a bearer token for a mutating, internet-reachable API
-- that nothing ever renders, and **pahoa refuses to start on a token under 32 bytes**
-- (MIN_ADMIN_TOKEN_BYTES in crates/pahoa/src/secrets.rs). Even 'high' is 17 characters, so putting
-- the token under this policy would be every room in the environment failing to start behind a
-- healthy-looking banner. It stays at 52 characters whatever this says.
--
-- --- CHANGING IT REGENERATES NOTHING ------------------------------------------------------------
-- A password already issued keeps working: this decides only what the NEXT generated one looks
-- like, which is a claim, a rotation, a room switched into per-slot mode, or a new room. So an
-- organizer tightening a race room does not invalidate credentials their players are holding, and
-- one loosening it to debug a client does not have to re-issue the whole roster to get there.
--
-- That is also why this is NOT in the room's spec hash: pahoa never sees the policy, only the
-- values it produces, and password values are kept out of that hash on purpose so a rotation does
-- not bounce a room. A policy change moves nothing pahoa reads, so it must not cost a restart.
CREATE TYPE password_complexity AS ENUM ('low', 'medium', 'high');

-- **'medium' for every existing room, and for every new one.** Unlike patch_policy's split default,
-- there is nothing here that a file already downloaded could disagree with, so one value does.
--
-- Worth stating plainly, because unifying three credentials under one policy means no default
-- preserves every current shape: a room-wide or remote-admin password rotated after this gets ten
-- symbols where it would have got fifteen, and a slot password gets ten grouped where the 2026-09-06
-- hotfix gave nine unbroken. Both are the point rather than a side effect, both are far past what
-- ten guesses a minute can reach, and 'high' restores the fifteen-symbol shape per room for anybody
-- who wants it.
ALTER TABLE rooms
    ADD COLUMN password_complexity password_complexity NOT NULL DEFAULT 'medium';

COMMENT ON COLUMN rooms.password_complexity IS
    'How long a generated password is for the three a person types: the room-wide password, each '
    'slot password, and the remote-admin password. Never the admin_token, which pahoa requires to '
    'be at least 32 bytes. Changing it regenerates nothing; it decides the next password issued, '
    'and it is deliberately absent from the spec hash because pahoa never reads it.';
