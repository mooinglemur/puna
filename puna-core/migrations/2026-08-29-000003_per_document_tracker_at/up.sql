-- Each cached tracker document gets its own timestamp.
--
-- One `last_tracker_at` covered two documents that are written on different schedules and under
-- different rules, and the combination served hours-old data as current:
--
--   * `tracker::store` refuses a document over PUNA_TRACKER_CACHE_MAX and LEAVES the old one in the
--     column -- correctly, since a truncated document is not a smaller document.
--   * A 2000-slot room's live document outgrows that cap partway through a run, so the stored copy
--     stops advancing and is pinned at whatever it was when it last fit.
--   * The static document is small, never grows, and keeps being stored -- stamping this one shared
--     timestamp `now()` every time.
--   * Freshness was judged from that stamp, so every live read within 60s of a static write took the
--     pinned body and reported it with `stale: false`.
--
-- Measured on the dev cluster: a room whose own `pahoa_checks_total` was flat at 169,938 for half an
-- hour served alternating documents reading 169,938 and 233 checks, both claiming to be current.
-- The pinned copy is whatever the live document looked like the last time it fit, which for a room
-- that fills quickly is very early -- so the symptom was a tracker reporting every slot at zero.
ALTER TABLE rooms ADD COLUMN last_static_tracker_at TIMESTAMPTZ;

COMMENT ON COLUMN rooms.last_tracker_at IS
  'When the LIVE document in last_tracker_doc was stored. Never set by a static write.';
COMMENT ON COLUMN rooms.last_static_tracker_at IS
  'When the static document in last_tracker_doc was stored.';

-- **The backfill takes the safe direction on each column, and they differ.**
--
-- Nothing recorded which document last wrote the old stamp, so it is evidence about one of them at
-- most. For the static document it is almost certainly right -- that document is small, so every
-- fetch of it stored successfully -- and being wrong there costs one needless refetch of a tiny
-- body.
UPDATE rooms SET last_static_tracker_at = last_tracker_at WHERE last_tracker_at IS NOT NULL;

-- For the live document the same guess is the bug: a room in the state described above carries a
-- stamp minutes old and a body hours old, so keeping it would serve the stale copy as fresh for one
-- more window. Clearing it makes every live document read as "cached, age unknown" -- which cannot
-- satisfy a freshness check and can still be served as the torn-down-room fallback, where the age is
-- reported rather than assumed. The cost is one refetch per room on the next request.
UPDATE rooms SET last_tracker_at = NULL;
