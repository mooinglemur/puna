-- The room's own effective gameplay rules, as the room last reported them.
--
-- **Observed, never desired**, and that distinction is the whole reason this column exists rather
-- than a set of settings columns beside `slot_auth` and `journal_policy`. pahoa's
-- `save::encode_options` persists these into `room.save` and `Room::restore` takes them from the
-- snapshot, so once a room has saved once its own copy outranks anything Puna passed at startup --
-- including a live `!admin /option` change an organizer made. A Puna-side column holding what Puna
-- *wanted* would therefore be a value the next restart ignores, which is the trap §7 names: never
-- render a gameplay option from Puna's own configuration.
--
-- So this is a reading, written only by the orchestrator's probe pass out of `/admin/v1/status`,
-- and every surface that shows a gameplay rule shows it from here.
--
-- **JSONB and deliberately unshaped.** Giving these eight keys columns would be Puna claiming a
-- schema it does not own and would have to track through pahoa's releases -- and a rule pahoa adds
-- later would be invisible until Puna shipped a migration for it. An opaque document renders
-- whatever the room says, which is the only honest thing to do with somebody else's vocabulary.
--
-- **Freshness is `probed_at`**, which the same statement moves, so the two cannot disagree about
-- how old this reading is. A stopped room keeps its last one rather than losing it: an async room
-- is down most of its life and its rules are still what an organizer needs to know, which is the
-- reason to store this at all instead of asking a live room on every page load.
ALTER TABLE rooms ADD COLUMN gameplay_options JSONB;

COMMENT ON COLUMN rooms.gameplay_options IS
  'The room''s effective gameplay rules, observed from /admin/v1/status by the orchestrator''s '
  'probe pass. NEVER Puna''s own configuration: after a room''s first save its copy outranks what '
  'Puna passed. NULL means nobody has managed to ask yet. Freshness is probed_at.';
