-- What a seed's data package weighs on the wire, so a room's outbound budget can account for it.
--
-- **The budget was sized from slot count, and slot count does not predict this.** A client's largest
-- single download is one `GetDataPackage` answer, and that scales with the number of GAMES rather
-- than with players: a live 189-slot room carrying 106 games answers with 6.19 MB against a 256 KiB
-- per-connection share, and clients pipeline about fourteen of those. Rooms were closing healthy
-- players in a reconnect loop with the room-wide budget at 0.6% used, because the refusals were
-- per-connection and the thing overrunning them was invisible to the derivation.
--
-- pahoa's `outbound_budget_for` now takes both terms and Puna passes `--outbound-budget` explicitly,
-- so an explicit value wins and a fix on their side does not reach a Puna room until this number
-- exists. See `spec::room::outbound_budget_mib`.
--
-- **Nullable, and NULL is not zero.** It means "computed before this column existed", which is every
-- generation already on the volume: those rooms size exactly as they do today (the floor applies)
-- until the seed is re-read. Zero would say the same thing while also being a value a real seed
-- cannot have, since every seed carries at least the `Archipelago` pseudo-game.
--
-- Filled at ingest from `pahoa_multidata::DataPackage::wire_size_estimate`, which is pahoa's own
-- estimate rather than a second one: sizing a limit against a number the room does not agree with is
-- how the unit bug of M37 happened. The admin name-cache rebuild fills it in for older rows, because
-- that path already re-reads the seed for exactly this kind of derived fact.
ALTER TABLE generations ADD COLUMN datapackage_bytes BIGINT
  CHECK (datapackage_bytes IS NULL OR datapackage_bytes >= 0);

COMMENT ON COLUMN generations.datapackage_bytes IS
  'Estimated wire size of the seed''s merged data package, for sizing a room''s outbound budget. '
  'NULL means the generation predates the column; the room then sizes from slots alone, as before.';
