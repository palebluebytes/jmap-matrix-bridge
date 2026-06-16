-- Per-thread rooms: a contact can now have many rooms (one Matrix room per
-- email thread/chain), so the old "one room per (ghost_email, matrix_user_id)"
-- uniqueness must be removed. The room -> email binding is still unique by
-- matrix_room_id (the table's primary key), which is all the outbound reply
-- lookup (get_ghost_email_by_room) needs.
DROP INDEX IF EXISTS idx_room_ghost_user;
