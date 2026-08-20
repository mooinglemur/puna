-- Spec drift: seeing it, and closing it on purpose.
--
-- Two facts motivate every column here, and both were found by asking what a changed
-- `PUNA_PAHOA_IMAGE` actually does to a running room. The answer is NOTHING: `rooms.spec_hash` is
-- written only in the start path, in the same statement that writes the Deployment's own
-- annotation, and the planner compares those two values against each other -- so they agree until
-- the room next starts. Nothing re-renders a spec for a running room, so nothing can notice.
--
-- That default is correct and stays. What was missing is that it held by omission: with no drift
-- computed for a running room, there was nothing to SHOW and no way to act on it deliberately.
--
-- ## The web tier cannot see the cluster, and that shapes all of this
--
-- `puna-web` has no ServiceAccount token at all -- the point of the two-binary split -- so every
-- OBSERVED value the admin table renders has to be written onto the row by the orchestrator. The
-- three observed columns below are exactly that, and each comes from a read the reconcile tick
-- already performs. None of them costs a new API call, a new permission, or a new object type.

ALTER TABLE rooms
  -- What the CLUSTER says the pod is running, not what Puna believes it started. Those disagree
  -- exactly when something has gone wrong, which is when this table is being read -- so the
  -- observed value is the honest one and the remembered one would hide the case it exists for.
  ADD COLUMN running_image TEXT,

  -- How long the current SPEC has been in force.
  ADD COLUMN deployment_created_at TIMESTAMPTZ,

  -- How long THIS pahoa has been serving, from `/admin/v1/status`.`started_at` -- already parsed by
  -- the probe on every tick and, until now, thrown away.
  --
  -- The pair is the point. They diverge when Kubernetes moved the pod (eviction, drain, preemption)
  -- or when the container restarted in place; either way the room reloaded its save and every
  -- client reconnected, which an organizer noticed at the time and which Puna could not explain.
  -- Reading pod objects would say WHICH of those happened, and is deliberately not done: it needs
  -- `pods` in the orchestrator's Role, which grants three resources on purpose, and the probe
  -- already answers the question the table asks.
  ADD COLUMN process_started_at TIMESTAMPTZ,

  -- What the spec WOULD render to now, against `spec_hash` which is what was last applied.
  --
  -- Filled in on the sweep's HOURLY lane, not the tick. Rendering a spec costs four queries per
  -- room -- the row, its secrets, its slots, its reservation -- which is why `reconcile` computes
  -- it only for failed rooms. Four per room per hour is nothing, and an admin table does not need
  -- drift detected within thirty seconds.
  ADD COLUMN desired_spec_hash TEXT,

  -- The ONLY thing that makes a running room restart for a spec change. Drift alone never does.
  --
  -- A request that gets CONSUMED, never a state that repeats: the recreate step clears it in the
  -- same pass that acts on it. A step that forgot would leave a room bouncing every tick forever,
  -- with no error anywhere -- the one catastrophic-and-silent failure in this design.
  --
  -- Two writers, deliberately one mechanism: the admin console's redeploy control, and
  -- `POST /room/<id>/settings` on a `slot_auth` change. The second is a FIX, not a feature -- the
  -- plan's §4 says a password-mode change applies immediately and nothing ever implemented it, so
  -- turning per-slot passwords *on* for a live room left that room open until its next restart.
  ADD COLUMN redeploy_requested_at TIMESTAMPTZ;

-- Partial: the overwhelmingly common state is NULL, and the planner reads this to order and cap
-- the rooms it will recreate this tick.
CREATE INDEX rooms_redeploy_idx ON rooms (redeploy_requested_at)
  WHERE redeploy_requested_at IS NOT NULL;

-- The fleet's configured pahoa image, published by the orchestrator for the web tier to read.
--
-- `PUNA_PAHOA_IMAGE` is an environment variable on the ORCHESTRATOR alone, so the web tier has no
-- way to know what a room *should* be running. Setting the same variable on `puna-web` was the
-- obvious alternative and is worse: two copies in git that can drift, and that drift would make the
-- admin page lie about precisely the thing it exists to report. One writer, one value.
--
-- NOT a row in `settings`, which the plan suggested: `settings.mode` is `gate_mode NOT NULL`, so an
-- image would need a meaningless mode beside it and the string stuffed into `detail`. A gates table
-- holding something that is not a gate is how a schema starts lying about itself.
--
-- Keyed by environment for the same reason `rooms` is: the orchestrator already refuses to start if
-- it sees bound reservations for the other environment, and a value keyed this way cannot be read
-- by a process that is pointed at the wrong database and quietly believed.
CREATE TABLE fleet (
  environment puna_environment PRIMARY KEY,
  pahoa_image TEXT NOT NULL,
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
