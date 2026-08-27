-- Whether a served patch carries the credential to connect, or only the address.
--
-- 'open'     the patch's `server` is `host:port`, as the reference writes it. A player with a
--            password still has to type it, and a room with none connects on the address alone.
-- 'claimed'  the patch's `server` is `wss://<slot>:<password>@<host>:<port>` where the room or the
--            slot has a password, so the client the patch launches connects without being told
--            anything. Falls back to the bare address when there is no password to embed.
--
-- **Verified against Archipelago rather than assumed**: `CommonClient.py`'s `server_loop` parses
-- userinfo out of the address and `unquote`s both halves, and a patch's `server` field reaches that
-- same parser through `args.connect`. Both halves must therefore be percent-encoded -- a slot name
-- is arbitrary text out of a seed and may contain `@`, `:` or a space.
--
-- 'claimed' is the default because the patch is already a per-slot artifact served only to that
-- slot's owner and the room's staff: it carries the slot's identity either way, and a credential
-- the recipient is entitled to is not a new disclosure. 'open' exists for a room whose patches get
-- passed around.
CREATE TYPE patch_policy AS ENUM ('open', 'claimed');

-- **Existing rooms default to 'open', which is exactly what they do today.** Every patch served so
-- far carries a bare `host:port`, and a migration must not change what a file already downloaded
-- disagrees with.
ALTER TABLE rooms
    ADD COLUMN patch_policy patch_policy NOT NULL DEFAULT 'open';

COMMENT ON COLUMN rooms.patch_policy IS
    'Whether a served patch embeds the slot credential (claimed) or only the address (open). '
    'New rooms are created claimed; the column default is open so that rooms predating this '
    'keep serving what their players already downloaded.';
