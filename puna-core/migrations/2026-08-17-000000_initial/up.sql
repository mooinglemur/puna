-- Puna's initial schema.
--
-- Column families on `rooms` are split by writer and the split is load-bearing: the web tier
-- writes only DESIRED columns, the orchestrator writes only OBSERVED ones. Nothing in the
-- database enforces that -- a capability token in puna-core does -- but the grouping is here so
-- a reader can see which side owns what.
--
-- Every timestamp is TIMESTAMPTZ. Never TIMESTAMP WITHOUT TIME ZONE: that stores a wall-clock
-- reading with no offset, which is ambiguous across DST and across servers. TIMESTAMPTZ stores
-- an instant, and its '-infinity' is load-bearing in port_reservations (see below).

CREATE TYPE puna_environment AS ENUM ('dev', 'prod');
CREATE TYPE room_source      AS ENUM ('direct', 'lobby');
CREATE TYPE spoiler_policy   AS ENUM ('never', 'admin_only', 'players', 'public');

-- 'link'    = the unguessable URL is the authorization, as the reference implementation does.
-- 'members' = the URL reaches the page, but Discord login plus membership or slot ownership
--             is also required. Defaulted from race_mode.
CREATE TYPE tracker_policy   AS ENUM ('link', 'members', 'disabled');

CREATE TYPE gate_mode        AS ENUM ('disabled', 'allowlist', 'open');
CREATE TYPE room_role        AS ENUM ('helper', 'organizer');  -- Ord in Rust: helper < organizer
CREATE TYPE command_state    AS ENUM ('pending', 'running', 'ok', 'failed', 'rejected');

-- Mutually exclusive, mirroring reference Archipelago plus pahoa's per-slot addition.
CREATE TYPE slot_auth_mode   AS ENUM ('none', 'room', 'per_slot');

CREATE TYPE room_state AS ENUM (
    'provisioning',     -- row exists, room directory may not
    'idle',             -- directory exists, no Deployment
    'starting',         -- port allocated, objects created, not yet ready
    'running',          -- Deployment has a ready replica
    'degraded',         -- Deployment exists, no ready replica for N sweeps
    'stopping',
    'failed',
    'deleting',
    'integrity_fault'   -- provisioned_at set but directory missing. NEVER auto-repaired.
);

CREATE TYPE room_desired_state AS ENUM ('running', 'stopped', 'deleted');


CREATE TABLE users (
    id            BIGINT PRIMARY KEY,   -- Discord snowflake. No internal user id anywhere.
    username      TEXT NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);


-- Feature gates. Keys: 'room_creation.direct', 'room_creation.lobby'.
CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    mode       gate_mode NOT NULL,
    detail     JSONB NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by BIGINT REFERENCES users (id)
);

-- Both closed on a fresh deploy. Admins bypass every gate, so this IS the initial testing
-- posture: admin-only direct uploads, with the lobby path off until that integration lands.
INSERT INTO settings (key, mode) VALUES
    ('room_creation.direct', 'disabled'),
    ('room_creation.lobby', 'disabled');


-- Deliberately NO foreign key to users: someone must be authorizable before they have ever
-- logged in, and a FK would force the row to exist first.
CREATE TABLE creator_allowlist (
    user_id  BIGINT PRIMARY KEY,
    note     TEXT,
    added_by BIGINT REFERENCES users (id),
    added_at TIMESTAMPTZ NOT NULL DEFAULT now()
);


-- One row per ingested generation zip. Content-addressed: the sha256 hex is also the directory
-- name under generations/, which makes deduplication and idempotent ingest the same mechanism.
CREATE TABLE generations (
    id                 UUID PRIMARY KEY,
    sha256             BYTEA NOT NULL UNIQUE,
    size_bytes         BIGINT NOT NULL CHECK (size_bytes > 0),
    seed_name          TEXT NOT NULL,   -- pahoa's Room::restore refuses a mismatched seed
    slots              INTEGER NOT NULL,
    locations          BIGINT NOT NULL,
    games              TEXT[] NOT NULL DEFAULT '{}',
    race_mode          BOOLEAN NOT NULL DEFAULT false,  -- from the multidata; defaults policies
    spoiler_member     TEXT,   -- NULL when the zip carried no spoiler
    min_server_version TEXT,
    -- Provenance lives on ROOMS, not here: these bytes are content-addressed and deduplicated,
    -- so one generation row can back a direct upload and a lobby push at the same time.
    first_ingested_by  BIGINT REFERENCES users (id),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);


CREATE TABLE generation_slots (
    generation_id    UUID NOT NULL REFERENCES generations (id) ON DELETE CASCADE,
    slot_number      INTEGER NOT NULL,
    player_name      TEXT NOT NULL,
    game             TEXT NOT NULL,
    patch_member     TEXT,     -- path inside the zip; NULL for games with no patch
    patch_size_bytes BIGINT,
    PRIMARY KEY (generation_id, slot_number)
);


CREATE TABLE rooms (
    -- Always a fresh uuid4 minted by Puna. NOT the lobby's room id: one lobby room may be
    -- handed over repeatedly and each push opens a NEW room, so the lobby's id is provenance
    -- (below), not identity. Many rooms may also share one generation.
    id                       UUID PRIMARY KEY,
    lock_key                 INTEGER GENERATED ALWAYS AS IDENTITY UNIQUE,  -- advisory-lock key
    environment              puna_environment NOT NULL,
    name                     TEXT NOT NULL,
    generation_id            UUID NOT NULL REFERENCES generations (id),
    source                   room_source NOT NULL,

    -- Provenance. Both nullable and NEITHER unique: repeated handovers of one lobby room
    -- legitimately produce several rows sharing these values.
    lobby_room_id            UUID,
    lobby_job_id             TEXT,

    -- Supplied per push attempt so a retried HTTP request returns the existing room instead of
    -- opening a second one. NULL for direct uploads.
    idempotency_key          TEXT UNIQUE,

    cloned_from              UUID REFERENCES rooms (id) ON DELETE SET NULL,
    created_by               BIGINT REFERENCES users (id),  -- informational; authority is room_members
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- ---- DESIRED: the web tier writes these, the orchestrator only reads them ----
    desired_state            room_desired_state NOT NULL DEFAULT 'stopped',
    desired_at               TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Defaults ON: the port pair is reserved either way, the filtered listener is the same
    -- server, and it is purely additive for players whose clients drown in a large multiworld.
    wants_filtered           BOOLEAN NOT NULL DEFAULT true,

    spoiler_policy           spoiler_policy NOT NULL,

    -- The tracker's URL segment, INDEPENDENT of id: /tracker/<tracker_id> must not be walkable
    -- back to /room/<id>. Sharing a tracker is the point; sharing the room URL is not. Same
    -- capability class as id itself -- unguessable, and possession is the authorization.
    tracker_id               UUID NOT NULL UNIQUE,
    tracker_policy           tracker_policy NOT NULL,

    slot_auth                slot_auth_mode NOT NULL DEFAULT 'none',
    password                 TEXT,   -- set ONLY when slot_auth = 'room'
    server_password          TEXT,
    use_embedded_options     BOOLEAN NOT NULL DEFAULT true,
    save_interval_secs       INTEGER NOT NULL DEFAULT 30 CHECK (save_interval_secs >= 1),

    -- Ties the room-wide password to the mode that uses it, so the two cannot disagree.
    CONSTRAINT room_password_matches_mode
        CHECK ((slot_auth = 'room') = (password IS NOT NULL)),

    -- ---- OBSERVED: the orchestrator writes these, the web tier only reads them ----
    state                    room_state NOT NULL DEFAULT 'provisioning',
    state_changed_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    provisioned_at           TIMESTAMPTZ,   -- NULL => room directory not known to exist
    secret_synced_at         TIMESTAMPTZ,   -- the per-room Secret matches the database
    deployment_uid           TEXT,
    spec_hash                TEXT,
    advertised_host          TEXT,
    advertised_port          INTEGER,
    advertised_filtered_port INTEGER,
    started_at               TIMESTAMPTZ,
    stopped_at               TIMESTAMPTZ,
    last_error               TEXT,
    failure_count            INTEGER NOT NULL DEFAULT 0,
    retry_after              TIMESTAMPTZ,
    not_ready_sweeps         INTEGER NOT NULL DEFAULT 0,

    admin_token              TEXT NOT NULL,  -- never rendered in any template
    admin_token_rotated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- From RoomProbe. NULL means "the probe cannot tell", NEVER zero. clients_connected counts
    -- SOCKETS, not players: one player commonly holds three.
    clients_connected        INTEGER,
    last_activity_at         TIMESTAMPTZ,
    probed_at                TIMESTAMPTZ,
    probe_kind               TEXT,

    -- Last successful tracker document, so the tracker page stays useful while the room is torn
    -- down, which for an async is most of the time. Written by the web tier on every successful
    -- proxy fetch, and capped in code; over the cap it is left NULL and the page says so rather
    -- than serving something truncated.
    last_tracker_doc         JSONB,
    last_tracker_at          TIMESTAMPTZ
);

CREATE INDEX rooms_work_idx ON rooms (environment, state, retry_after);
CREATE INDEX rooms_live_idx ON rooms (state) WHERE state IN ('starting', 'running', 'degraded');
CREATE INDEX rooms_generation_idx ON rooms (generation_id);  -- sibling rooms sharing a seed
CREATE INDEX rooms_lobby_idx ON rooms (lobby_room_id) WHERE lobby_room_id IS NOT NULL;


-- Per-room staff. The uploader is simply the first 'organizer' row: no creator special case,
-- one resolution path.
CREATE TABLE room_members (
    room_id  UUID NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    user_id  BIGINT NOT NULL REFERENCES users (id),
    role     room_role NOT NULL,
    added_by BIGINT REFERENCES users (id),
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (room_id, user_id)
);

CREATE INDEX room_members_user_idx ON room_members (user_id);


-- A room must never be left with nobody who can administer it. This spans rows, so it cannot be
-- a CHECK constraint; and putting it in the one route that removes members would mean trusting
-- every future route to remember. An orphaned room has no one able to fix it, which is why this
-- is worth a trigger.
--
-- Fires per row on DELETE and on any UPDATE that could demote. The room-deleted case is exempt:
-- ON DELETE CASCADE removes the room's members legitimately, and by then the room row is gone.
CREATE FUNCTION forbid_removing_last_organizer() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.role = 'organizer' THEN
        RETURN NEW;  -- still an organizer, nothing to check
    END IF;

    IF OLD.role <> 'organizer' THEN
        RETURN CASE TG_OP WHEN 'DELETE' THEN OLD ELSE NEW END;
    END IF;

    -- The room itself going away takes its members with it; that is not an orphaning.
    IF NOT EXISTS (SELECT 1 FROM rooms WHERE id = OLD.room_id) THEN
        RETURN CASE TG_OP WHEN 'DELETE' THEN OLD ELSE NEW END;
    END IF;

    IF (SELECT count(*) FROM room_members
         WHERE room_id = OLD.room_id AND role = 'organizer') <= 1 THEN
        RAISE EXCEPTION
            'room % would be left with no organizer', OLD.room_id
            USING ERRCODE = 'restrict_violation';
    END IF;

    RETURN CASE TG_OP WHEN 'DELETE' THEN OLD ELSE NEW END;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER room_members_keep_an_organizer
    BEFORE DELETE OR UPDATE OF role ON room_members
    FOR EACH ROW EXECUTE FUNCTION forbid_removing_last_organizer();


-- Delegation by link, mirroring the slot claim pattern so an organizer never needs to know
-- anyone's Discord snowflake.
CREATE TABLE room_invites (
    token          TEXT PRIMARY KEY,
    room_id        UUID NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    role           room_role NOT NULL,
    created_by     BIGINT NOT NULL REFERENCES users (id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at     TIMESTAMPTZ,
    uses_remaining INTEGER CHECK (uses_remaining IS NULL OR uses_remaining >= 0)
);

CREATE INDEX room_invites_room_idx ON room_invites (room_id);


-- Per-slot identity, credential and claim. Copied from generation_slots at room creation so a
-- room is independent of later generation housekeeping.
CREATE TABLE room_slots (
    room_id     UUID NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    slot_number INTEGER NOT NULL,
    player_name TEXT NOT NULL,
    game        TEXT NOT NULL,
    password    TEXT,   -- NULL unless the room's slot_auth = 'per_slot'
    owner_id    BIGINT REFERENCES users (id),  -- NULL = unclaimed
    claim_token TEXT,                          -- NULL once claimed
    claimed_at  TIMESTAMPTZ,

    -- Its own URL segment, NOT a path under the room's. Better than the reference, where
    -- /tracker/<id>/<team>/<player> leaks the multiworld id: here a player can share their
    -- personal tracker without handing out the whole multiworld's.
    tracker_id  UUID NOT NULL UNIQUE,

    PRIMARY KEY (room_id, slot_number)
);

CREATE UNIQUE INDEX room_slots_claim_idx ON room_slots (claim_token) WHERE claim_token IS NOT NULL;
CREATE INDEX room_slots_owner_idx ON room_slots (owner_id);


-- The console. The web tier inserts 'pending'; the orchestrator claims, executes and responds.
CREATE TABLE room_commands (
    id             UUID PRIMARY KEY,
    room_id        UUID NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    requested_by   BIGINT NOT NULL REFERENCES users (id),
    requested_role room_role NOT NULL,  -- what authorized it, frozen at request time
    command        JSONB NOT NULL,      -- the typed command
    state          command_state NOT NULL DEFAULT 'pending',
    result         JSONB,
    error          TEXT,
    requested_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at     TIMESTAMPTZ,
    finished_at    TIMESTAMPTZ
);

CREATE INDEX room_commands_pending_idx ON room_commands (state, requested_at)
    WHERE state IN ('pending', 'running');
CREATE INDEX room_commands_room_idx ON room_commands (room_id, requested_at DESC);


-- Port reservations. One row per ADJACENT PAIR, keyed on the even base port -- not one row per
-- port. Two rows per pair would let a third allocation land on base+1 between the two inserts;
-- one row makes the primary key itself protect the pair.
--
-- Rows deliberately outlive the Services they describe: this is a reservation table, not an
-- allocation table. A torn-down room returns to the same port, which is the requirement that
-- forced a database in the first place.
CREATE TABLE port_reservations (
    environment       puna_environment NOT NULL,
    base_port         INTEGER NOT NULL,
    room_id           UUID REFERENCES rooms (id) ON DELETE SET NULL,

    -- '-infinity' means NEVER ALLOCATED, and it is load-bearing: it makes "never allocated" and
    -- "least recently used" the same ordering, so the allocator needs no sentinel and no
    -- COALESCE. Replacing this column with an epoch integer would put that branch back.
    last_activity     TIMESTAMPTZ NOT NULL DEFAULT '-infinity',

    quarantined_until TIMESTAMPTZ,

    PRIMARY KEY (environment, base_port),
    CHECK (base_port % 2 = 0),

    -- The dev/prod partition, enforced by the database rather than remembered by the code.
    -- Both environments' Services share one public address and therefore one port space, and
    -- Cilium does not report a collision -- it silently allocates a second IP, leaving a room
    -- reachable on an address DNS never mentions. This is the cheapest of three guards.
    CHECK ((environment = 'dev'  AND base_port BETWEEN 40000 AND 44998)
        OR (environment = 'prod' AND base_port BETWEEN 45000 AND 49998))
);

CREATE UNIQUE INDEX port_reservations_room_idx ON port_reservations (room_id)
    WHERE room_id IS NOT NULL;
CREATE INDEX port_reservations_lru_idx ON port_reservations (environment, last_activity, base_port);

-- Pre-seed every pair in both environments, 5000 rows. The allocator then only ever UPDATEs,
-- which is what lets it be a single atomic statement with no INSERT/conflict retry loop.
INSERT INTO port_reservations (environment, base_port)
    SELECT 'dev'::puna_environment, p FROM generate_series(40000, 44998, 2) AS p;
INSERT INTO port_reservations (environment, base_port)
    SELECT 'prod'::puna_environment, p FROM generate_series(45000, 49998, 2) AS p;


CREATE TABLE room_events (
    id      BIGSERIAL PRIMARY KEY,
    room_id UUID NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor   TEXT NOT NULL,   -- 'web:<discord id>' | 'orchestrator' | 'reconcile'
    kind    TEXT NOT NULL,
    detail  JSONB NOT NULL DEFAULT '{}'
);

CREATE INDEX room_events_room_idx ON room_events (room_id, at DESC);
