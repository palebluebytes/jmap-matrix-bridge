//! Outbound send-state indicator (ADR-0012, #26).
//!
//! A send progresses ⏳ held → ✅ submitted (or ❌ failed). The bot reacts to the
//! user's outbound message; the held ⏳ is redacted when the send resolves so the
//! message ends on a single glanceable glyph rather than an accumulating row.

use crate::matrix::MatrixClient;
use crate::store::Store;

/// ⏳ — held in the send-delay window (redact the message to undo).
pub const HELD: &str = "⏳";
/// ✅ — submission verified.
pub const SUBMITTED: &str = "✅";
/// ❌ — permanently failed to deliver.
pub const FAILED: &str = "❌";

fn held_key(event_id: &str) -> String {
    format!("send_state:{event_id}")
}

/// Mark a message held: react ⏳ and remember the reaction id so it can be
/// redacted on resolution. Best-effort — a UI hiccup must never affect delivery.
pub(crate) async fn mark_held(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    room_id: &str,
    event_id: &str,
) {
    match matrix.send_reaction(room_id, event_id, HELD).await {
        Ok(reaction_id) => {
            let _ = store
                .save_jmap_state(matrix_user_id, &held_key(event_id), &reaction_id)
                .await;
        }
        Err(e) => tracing::warn!(error = %e, "Failed to add held send-state reaction"),
    }
}

/// Mark a message resolved: redact the ⏳ (if any) and react with the final
/// glyph (`SUBMITTED` / `FAILED`). Best-effort.
pub(crate) async fn mark_final(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    room_id: &str,
    event_id: &str,
    glyph: &str,
) {
    let key = held_key(event_id);
    if let Ok(Some(prior)) = store.get_jmap_state(matrix_user_id, &key).await {
        let _ = matrix.redact_event(room_id, &prior, "send resolved").await;
        let _ = store.delete_jmap_state(matrix_user_id, &key).await;
    }
    if let Err(e) = matrix.send_reaction(room_id, event_id, glyph).await {
        tracing::warn!(error = %e, "Failed to add final send-state reaction");
    }
}
