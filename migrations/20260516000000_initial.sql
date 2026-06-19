-- Initial schema for JMAP-Matrix Bridge (Squashed Pre-Beta, Enterprise-Grade)

-- Enable strict type checking and foreign keys
-- Core table: users
CREATE TABLE IF NOT EXISTS users (
    matrix_user_id TEXT PRIMARY KEY,
    jmap_username TEXT NOT NULL,
    jmap_token TEXT NOT NULL,
    jmap_url TEXT NOT NULL
) STRICT;

-- JMAP state store with cascade delete on user removal
CREATE TABLE IF NOT EXISTS jmap_state (
    matrix_user_id TEXT NOT NULL,
    state_key TEXT NOT NULL,
    state_value TEXT NOT NULL,
    PRIMARY KEY (matrix_user_id, state_key),
    FOREIGN KEY (matrix_user_id) REFERENCES users(matrix_user_id) ON DELETE CASCADE
) STRICT;

-- User custom signatures
CREATE TABLE IF NOT EXISTS user_signatures (
    matrix_user_id TEXT PRIMARY KEY,
    signature TEXT NOT NULL,
    FOREIGN KEY (matrix_user_id) REFERENCES users(matrix_user_id) ON DELETE CASCADE
) STRICT;

-- Mailbox mapping (JMAP Mailbox <-> Matrix Room)
CREATE TABLE IF NOT EXISTS mailbox_mapping (
    jmap_mailbox_id TEXT PRIMARY KEY,
    matrix_room_id TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS idx_mailbox_room_id ON mailbox_mapping(matrix_room_id);

-- Thread mapping (JMAP Thread <-> Matrix Thread Root Event)
CREATE TABLE IF NOT EXISTS thread_mapping (
    jmap_thread_id TEXT PRIMARY KEY,
    matrix_root_event_id TEXT NOT NULL,
    matrix_room_id TEXT NOT NULL,
    latest_event_id TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS idx_thread_room_id ON thread_mapping(matrix_room_id);

-- Message mapping (JMAP Email ID <-> Matrix Event ID)
CREATE TABLE IF NOT EXISTS message_mapping (
    jmap_email_id TEXT PRIMARY KEY,
    matrix_event_id TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS idx_message_event_id ON message_mapping(matrix_event_id);

-- Room ghost mapping for multi-tenant isolation
CREATE TABLE IF NOT EXISTS room_ghost_mapping (
    matrix_room_id TEXT PRIMARY KEY,
    ghost_email TEXT NOT NULL,
    last_email_id TEXT,
    matrix_user_id TEXT NOT NULL,
    FOREIGN KEY (matrix_user_id) REFERENCES users(matrix_user_id) ON DELETE CASCADE
) STRICT;
-- Note: a contact can have many rooms (one Matrix room per email thread), so
-- there is intentionally no uniqueness on (ghost_email, matrix_user_id). The
-- room -> email binding is unique by matrix_room_id (the primary key), which is
-- all the outbound reply lookup (get_ghost_email_by_room) needs.

-- Thread subject cache
CREATE TABLE IF NOT EXISTS matrix_thread_subjects (
    matrix_root_event_id TEXT PRIMARY KEY,
    subject TEXT NOT NULL
) STRICT;

-- Processed transactions for idempotency
CREATE TABLE IF NOT EXISTS processed_transactions (
    txn_id TEXT PRIMARY KEY,
    processed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
) STRICT;
CREATE INDEX IF NOT EXISTS idx_processed_transactions_at ON processed_transactions(processed_at);

-- Outbound queue for reliable delivery retries
CREATE TABLE IF NOT EXISTS outbound_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    matrix_user_id TEXT NOT NULL,
    room_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    body_text TEXT NOT NULL,
    formatted_body TEXT,
    thread_root_id TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_retry_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    attachments_json TEXT,
    FOREIGN KEY (matrix_user_id) REFERENCES users(matrix_user_id) ON DELETE CASCADE
) STRICT;
CREATE INDEX IF NOT EXISTS idx_outbound_queue_retry ON outbound_queue(last_retry_at) WHERE retry_count < 10;
CREATE INDEX IF NOT EXISTS idx_outbound_queue_user ON outbound_queue(matrix_user_id);

-- Room creation locks for preventing race conditions
CREATE TABLE IF NOT EXISTS room_creation_locks (
    lock_key TEXT PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
) STRICT;
CREATE INDEX IF NOT EXISTS idx_room_creation_locks_at ON room_creation_locks(created_at);

-- Destroyed emails cache to prevent re-sync
CREATE TABLE IF NOT EXISTS destroyed_emails (
    jmap_email_id TEXT PRIMARY KEY,
    destroyed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
) STRICT;
CREATE INDEX IF NOT EXISTS idx_destroyed_emails_at ON destroyed_emails(destroyed_at);
