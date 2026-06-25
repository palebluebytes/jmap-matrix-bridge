-- Send-delay hold window (ADR-0012).
--
-- Outbound mail is enqueued with a `release_at` time and only submitted by the
-- worker once it passes, giving a Gmail-style undo window in which a redaction
-- cancels the send and an edit rewrites its body. Existing rows default to
-- "now", so any in-flight retries at upgrade time stay immediately eligible.
ALTER TABLE outbound_queue ADD COLUMN release_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP;
