-- Whether this slot's password is withheld from the person holding the slot.
--
-- A room on per-slot passwords mints one secret per slot at creation, and every one of them is
-- readable by its owner from that moment. For an organizer running a synchronized start that is the
-- wrong default: the credential IS the start gate, and handing it out at creation means the room
-- opens whenever the first player gets bored. Today the only way to hold that line is `lock`, which
-- is a pahoa verb on a running room and reports to the player as a refused connection.
--
-- This is the Puna-side answer: the password exists, pahoa holds it and will authenticate it, and
-- Puna declines to show it until staff say so. Nothing here reaches pahoa, so it is a rendering
-- decision rather than a restart, and a room whose every flag is false behaves exactly as rooms
-- behaved before the column existed.
--
-- --- IT IS NOT A SECOND LOCK, AND THE DIFFERENCE IS THE POINT ---------------------------------
-- `locked_at` bars a slot from connecting: pahoa refuses it, and a player who already has the
-- password is still out. This withholds the password and bars nothing. A player who was told their
-- password by hand, or who had it before staff hid it again, connects normally.
--
-- That is deliberate. The two answer different questions -- "may this slot play" versus "has this
-- person been given their credential yet" -- and collapsing them would make un-hiding a password a
-- live pahoa command on a room that may be down, which is precisely the case this exists to serve:
-- an organizer sets a room up, distributes nothing, and starts it when everyone is ready.
--
-- --- WHAT MUST NOT READ IT -------------------------------------------------------------------
-- `PAHOA_SLOT_PASSWORDS` carries every password regardless. pahoa treats a slot ABSENT from that
-- map as having no password at all, so filtering the hidden ones out of it would turn each one into
-- the single unprivileged door into a room where everybody else needs a credential: the exact
-- inverse of what this column is for. See the note on room_slots.password.
--
-- It is also absent from the room's spec hash, like every password value and like the complexity
-- policy that generates them, so hiding or revealing one never bounces a room.
ALTER TABLE room_slots ADD COLUMN password_hidden BOOLEAN NOT NULL DEFAULT false;

-- A flag on a slot with no password describes nothing: the other two auth modes either have no
-- password or have one room-wide password, which is not a property of a slot. Stated as a CHECK for
-- the reason `room_password_matches_mode` is on `rooms`: the mode transition that NULLs every slot
-- password has to clear these in the same statement, and a constraint is what makes forgetting it
-- a loud failure rather than a row that means nothing.
ALTER TABLE room_slots
    ADD CONSTRAINT slot_password_hidden_needs_a_password
    CHECK (password IS NOT NULL OR NOT password_hidden);

COMMENT ON COLUMN room_slots.password_hidden IS
    'Whether Puna withholds this slot password from its owner. False by default, which is how '
    'every slot behaved before this existed. A rendering decision only: the password is still in '
    'PAHOA_SLOT_PASSWORDS and pahoa still authenticates it, so this bars nobody from connecting '
    'and is deliberately absent from the spec hash. Not a substitute for locked_at, which does '
    'bar a slot.';
