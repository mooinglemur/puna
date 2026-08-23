-- Reverting drops every filter Puna knows about. A room that is running keeps filtering, because
-- pahoa holds its own copy in `room.save` -- so the visible result is filters nobody can see,
-- explain or remove from the UI, until each room next starts with nothing to re-assert.
--
-- That is worse than it sounds and worth reading twice before running this against an environment
-- with live rooms: an unexplainable filter is the failure mode this whole feature introduces.
DROP TABLE IF EXISTS room_slot_filters;
DROP TABLE IF EXISTS room_filters;
