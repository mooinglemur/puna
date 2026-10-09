ALTER TABLE room_slots DROP CONSTRAINT slot_password_hidden_needs_a_password;
ALTER TABLE room_slots DROP COLUMN password_hidden;
