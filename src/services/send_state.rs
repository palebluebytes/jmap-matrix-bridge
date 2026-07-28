//! Outbound send-state indicator (ADR-0012, #26).
//!
//! The send-delay window is opt-in and off by default. When it is on, a held
//! message gets a ⏳ reaction (redact the message to undo), which resolves to ✅ on
//! a successful send or ❌ on a permanent failure. With the window off there is no
//! hold, so a successful send is silent; only a permanent failure still reacts ❌.

use crate::matrix::MatrixClient;
use crate::store::Store;

/// ⏳ — held in the send-delay window (redact the message to undo).
pub const HELD: &str = "⏳";
/// ✅ — submission verified (shown only when the message was held).
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

/// Redact and forget the held ⏳ marker if one was posted, reporting whether there
/// was one — i.e. whether the send-delay window was on for this message. Best-effort.
async fn take_held(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    room_id: &str,
    event_id: &str,
) -> bool {
    let key = held_key(event_id);
    if let Ok(Some(prior)) = store.get_jmap_state(matrix_user_id, &key).await {
        let _ = matrix.redact_event(room_id, &prior, "send resolved").await;
        let _ = store.delete_jmap_state(matrix_user_id, &key).await;
        true
    } else {
        false
    }
}

/// Resolve a successful send: redact the ⏳ and react ✅ — but *only* when the
/// message was held (send-delay window on). A plain instant send has no ⏳ and
/// stays reaction-free. Best-effort.
pub(crate) async fn mark_submitted(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    room_id: &str,
    event_id: &str,
) {
    if take_held(matrix, store, matrix_user_id, room_id, event_id).await {
        if let Err(e) = matrix.send_reaction(room_id, event_id, SUBMITTED).await {
            tracing::warn!(error = %e, "Failed to add submitted send-state reaction");
        }
    }
}

/// Mark a message permanently failed: clear the ⏳ (if any) and react ❌ alongside
/// the give-up notice the caller already posts. Shown regardless of the send-delay
/// window — a delivery failure must always be visible. Best-effort.
pub(crate) async fn mark_failed(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    room_id: &str,
    event_id: &str,
) {
    take_held(matrix, store, matrix_user_id, room_id, event_id).await;
    if let Err(e) = matrix.send_reaction(room_id, event_id, FAILED).await {
        tracing::warn!(error = %e, "Failed to add failed send-state reaction");
    }
}

#[cfg(test)]
mod tests {
    use super::{mark_held, mark_submitted};
    use crate::matrix::MatrixClient;
    use crate::store::Store;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A Matrix client wired to `server`, with reaction and redaction PUTs stubbed.
    async fn matrix_for(server: &MockServer) -> MatrixClient {
        Mock::given(method("PUT"))
            .and(path_regex(r".*/send/m\.reaction/.*"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "event_id": "$react:localhost" })),
            )
            .mount(server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r".*/redact/.*"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "event_id": "$redact:localhost" })),
            )
            .mount(server)
            .await;
        MatrixClient::new(&server.uri(), "as_token", "localhost")
            .await
            .unwrap()
    }

    /// The behaviour the ✅ gating exists for: with the send-delay window off (no ⏳
    /// was ever posted), a successful send must add NO reaction — a plain instant
    /// send stays silent.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn mark_submitted_is_silent_without_a_hold() {
        let server = MockServer::start().await;
        let matrix = matrix_for(&server).await;
        let store = Store::new_in_memory(None).await.unwrap();

        mark_submitted(
            &matrix,
            &store,
            "@u:localhost",
            "!r:localhost",
            "$e:localhost",
        )
        .await;

        let reqs = server.received_requests().await.unwrap();
        assert!(
            !reqs
                .iter()
                .any(|r| r.url.path().contains("/send/m.reaction/")),
            "an un-held (instant) send must post no ✅ reaction"
        );
    }

    /// When the window WAS on (a ⏳ hold exists), a successful send redacts the ⏳
    /// and reacts ✅.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn mark_submitted_resolves_a_held_message_to_check() {
        let server = MockServer::start().await;
        let matrix = matrix_for(&server).await;
        let store = Store::new_in_memory(None).await.unwrap();
        // The held ⏳ is persisted in `jmap_state`, which has a FK to `users`, so
        // the user must exist (as it does in production once logged in).
        store
            .save_user(&crate::store::RegisteredUser {
                matrix_user_id: "@u:localhost".to_owned(),
                jmap_username: "u".to_owned(),
                jmap_token: "t".to_owned(),
                jmap_url: "https://jmap.example".to_owned(),
            })
            .await
            .unwrap();

        mark_held(
            &matrix,
            &store,
            "@u:localhost",
            "!r:localhost",
            "$e:localhost",
        )
        .await;
        mark_submitted(
            &matrix,
            &store,
            "@u:localhost",
            "!r:localhost",
            "$e:localhost",
        )
        .await;

        let reqs = server.received_requests().await.unwrap();
        let paths: Vec<_> = reqs.iter().map(|r| r.url.path().to_owned()).collect();
        assert!(
            reqs.iter().any(|r| r.url.path().contains("/redact/")),
            "resolving a held send must redact the ⏳; saw {paths:?}"
        );
        let reacted_check = reqs.iter().any(|r| {
            r.url.path().contains("/send/m.reaction/")
                && String::from_utf8_lossy(&r.body).contains('✅')
        });
        assert!(
            reacted_check,
            "resolving a held send must react ✅; saw {paths:?}"
        );
    }
}
