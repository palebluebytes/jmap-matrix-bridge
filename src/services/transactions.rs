use crate::routes::AppState;
use crate::state::LoginState;
use anyhow::{Context, Result};
use matrix_sdk::ruma::{
    events::{
        AnySyncEphemeralRoomEvent, AnyTimelineEvent, SyncEphemeralRoomEvent,
        receipt::{ReceiptEventContent, ReceiptType},
        room::member::RoomMemberEventContent,
        room::message::MessageType,
    },
    serde::Raw,
};
use tracing::{error, info, warn};

#[derive(serde::Deserialize, Debug)]
pub struct MatrixTransaction {
    pub events: Vec<Raw<AnyTimelineEvent>>,
    #[serde(default)]
    pub ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
}

#[allow(clippy::too_many_lines)]
pub async fn process_transaction(
    state: &AppState,
    txn_id: &str,
    txn: MatrixTransaction,
) -> Result<()> {
    // 2. Process ephemeral events (Read Receipts)
    for ephemeral_raw in txn.ephemeral {
        if let Ok(AnySyncEphemeralRoomEvent::Receipt(receipt)) = ephemeral_raw.deserialize() {
            tracing::debug!(%txn_id, "Processing ephemeral read receipt");
            if let Err(e) = handle_receipt(state, receipt).await {
                error!(error = %e, "Failed to handle read receipt");
            }
        }
    }

    // 3. Process timeline events
    for raw_event in txn.events {
        let event = match raw_event.deserialize() {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to deserialize Matrix event: {}", e);
                continue;
            }
        };

        let sender_id = event.sender().to_string();
        let event_id = event.event_id().to_string();
        let room_id = Some(event.room_id().to_string());

        tracing::debug!(
            "Processing timeline event: id={}, type={}, sender={}, room={:?}",
            event_id,
            event.event_type(),
            sender_id,
            room_id
        );
        tracing::trace!("Deserialized event details: {:?}", event);

        match event {
            AnyTimelineEvent::State(e) => {
                if let matrix_sdk::ruma::events::AnyStateEvent::RoomMember(
                    matrix_sdk::ruma::events::room::member::RoomMemberEvent::Original(ev),
                ) = e
                {
                    let state_key = ev.state_key.to_string();
                    let sender_id = ev.sender.to_string();
                    tracing::debug!(
                        "Handling RoomMember event: state_key={}, sender={}, membership={:?}, room={:?}",
                        state_key,
                        sender_id,
                        ev.content.membership,
                        room_id
                    );
                    if let Err(err) = handle_room_member_event(
                        state,
                        &sender_id,
                        &state_key,
                        room_id.as_deref(),
                        &ev.content,
                    )
                    .await
                    {
                        error!("Failed to handle member event: {}", err);
                    }
                }
            }
            AnyTimelineEvent::MessageLike(e) => {
                // Ignore messages the bridge itself authored: the bot and every
                // `@_jmap_*` ghost live in the appservice's exclusive namespace
                // and their events are echoed straight back to us. Treating a
                // ghost's bridged email as an outbound user message replies
                // "You are not logged in" to every email, and the bot-sent reply
                // is itself echoed back, so it loops and floods the room.
                if sender_id.starts_with("@_jmap_") {
                    continue;
                }
                if let matrix_sdk::ruma::events::AnyMessageLikeEvent::RoomMessage(message_event) = e
                    && let Some(content) = message_event.as_original().map(|ev| &ev.content)
                {
                    let body_str = content.body();
                    tracing::debug!(
                        "Received RoomMessage: event_id={}, sender={}, msgtype={:?}, room={:?}",
                        event_id,
                        sender_id,
                        content.msgtype,
                        room_id
                    );

                    match &content.msgtype {
                        MessageType::Text(_) | MessageType::File(_) | MessageType::Image(_) => {
                            // Check if this room is a ghost room (outbound email).
                            if let Some(rm_id) = room_id.as_deref() {
                                match state
                                    .client_manager
                                    .store
                                    .get_ghost_email_by_room(rm_id)
                                    .await
                                {
                                    Ok(Some(email)) => {
                                        tracing::debug!(
                                            "Routing outbound message to email ({}) from ghost room {}",
                                            email,
                                            rm_id
                                        );
                                        let result = match &content.msgtype {
                                            MessageType::Text(_) => {
                                                crate::ghost::handle_ghost_outbound(
                                                    state,
                                                    &sender_id,
                                                    content,
                                                    Some(rm_id),
                                                    &event_id,
                                                )
                                                .await
                                            }
                                            MessageType::Image(_)
                                            | MessageType::File(_)
                                            | MessageType::Audio(_)
                                            | MessageType::Video(_) => {
                                                crate::ghost::handle_ghost_media_outbound(
                                                    state,
                                                    &sender_id,
                                                    content,
                                                    Some(rm_id),
                                                    &event_id,
                                                )
                                                .await
                                            }
                                            _ => Ok(()),
                                        };

                                        if let Err(err) = result {
                                            error!(%sender_id, %rm_id, error = %err, "Ghost outbound handler failed");
                                            if err.downcast_ref::<sqlx::Error>().is_some() {
                                                return Err(err);
                                            }
                                        }
                                        continue;
                                    }
                                    Err(err) => {
                                        error!(%rm_id, error = %err, "Failed to query ghost email mapping for room");
                                        return Err(err);
                                    }
                                    Ok(None) => {}
                                }
                            }

                            let login_state = state.state_store.get_login_state(&sender_id).await;
                            tracing::debug!(
                                "User {} is not in a ghost room. Current login state: {:?}",
                                sender_id,
                                login_state
                            );
                            let result = match login_state {
                                LoginState::None => {
                                    crate::commands::handle_login_none(
                                        state,
                                        &sender_id,
                                        body_str,
                                        room_id.as_deref(),
                                        Some(&event_id),
                                        content,
                                    )
                                    .await
                                }
                                LoginState::WaitingForEmail => {
                                    crate::commands::handle_login_waiting_for_email(
                                        state,
                                        &sender_id,
                                        body_str,
                                        room_id.as_deref(),
                                    )
                                    .await
                                }
                                LoginState::WaitingForPassword { email } => {
                                    crate::commands::handle_login_waiting_for_password(
                                        state,
                                        &sender_id,
                                        body_str,
                                        room_id.as_deref(),
                                        Some(&event_id),
                                        &email,
                                    )
                                    .await
                                }
                                LoginState::WaitingForUrl { email, password } => {
                                    crate::commands::handle_login_waiting_for_url(
                                        state,
                                        &sender_id,
                                        body_str,
                                        room_id.as_deref(),
                                        &email,
                                        &password,
                                    )
                                    .await
                                }
                            };

                            if let Err(err) = result {
                                error!(sender = %sender_id, error = %err, "Command handler failed");
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_receipt(
    state: &AppState,
    receipt: SyncEphemeralRoomEvent<ReceiptEventContent>,
) -> Result<()> {
    use crate::sender::JmapSender;
    // ReceiptEvent contains a map of event_id -> receipts
    for (event_id, types) in receipt.content {
        tracing::trace!("Processing receipt for event_id: {}", event_id);
        // We only care about read receipts for events we've bridged
        if let Some(email_id) = state
            .client_manager
            .store
            .get_email_id_from_event_id(event_id.as_str())
            .await?
        {
            tracing::debug!("Found JMAP email mapping for event {event_id}: email_id={email_id}");
            // Check the 'm.read' receipts
            if let Some(users) = types.get(&ReceiptType::Read) {
                for user_id in users.keys() {
                    // If it's a user we manage, sync to JMAP
                    if state
                        .client_manager
                        .store
                        .get_user(user_id.as_str())
                        .await?
                        .is_some()
                        && let Some(client) =
                            state.client_manager.get_client(user_id.as_str()).await
                    {
                        let sender = JmapSender::new(client);
                        if let Err(e) = sender.mark_as_read(&email_id).await {
                            warn!(user = %user_id, email_id, error = %e, "Failed to mark JMAP email as read");
                        } else {
                            info!(user = %user_id, email_id, "Synchronized read receipt to JMAP");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_room_member_event(
    state: &AppState,
    sender_id: &str,
    target_user_id: &str,
    room_id: Option<&str>,
    content: &RoomMemberEventContent,
) -> anyhow::Result<()> {
    let room_id = room_id.context("Missing room_id in member event")?;
    tracing::debug!(
        "Handling room member event in room {room_id}: sender_id={sender_id}, target_user_id={target_user_id}, membership={:?}",
        content.membership
    );

    // Auto-join if this is an invite for one of our bridge users (the bot or a ghost).
    if content.membership == matrix_sdk::ruma::events::room::member::MembershipState::Invite {
        let bot_user_id = state.client_manager.matrix.bot_user_id();
        let is_bot = target_user_id == bot_user_id;
        let is_ghost = target_user_id.starts_with("@_jmap_") && !is_bot;

        if is_bot || is_ghost {
            info!(
                "Accepting invite for {} to room {}",
                target_user_id, room_id
            );
            if let Err(e) = state
                .client_manager
                .matrix
                .join_room_as(room_id, target_user_id)
                .await
            {
                error!(
                    "Failed to join room {} as {}: {}",
                    room_id, target_user_id, e
                );
            } else {
                info!("Successfully joined room {} as {}", room_id, target_user_id);
                if is_bot {
                    notify(
                        state,
                        Some(room_id),
                        "Welcome! Please type `login` to connect your JMAP account to this bridge.",
                    )
                    .await;
                }
            }
        }
    }

    if content.membership == matrix_sdk::ruma::events::room::member::MembershipState::Join {
        // Ignore joins by the bridge's own users (the bot and every `@_jmap_*`
        // ghost). Ghosts auto-join their contact rooms, and ghosts have no JMAP
        // session, so without this every contact room got a "Welcome! Please
        // type `login`" posted when its ghost joined.
        if sender_id.starts_with("@_jmap_") {
            return Ok(());
        }

        // If a real user joins a ghost room without a session, prompt them.
        if state
            .client_manager
            .store
            .get_ghost_email_by_room(room_id)
            .await?
            .is_some()
            && state.client_manager.get_client(sender_id).await.is_none()
        {
            notify(
                state,
                Some(room_id),
                "Welcome! Please type `login` to connect your JMAP account to this bridge.",
            )
            .await;
        }
    }
    Ok(())
}

/// Convenience wrapper for "fire-and-forget" user notifications.
pub async fn notify(state: &AppState, room_id: Option<&str>, msg: &str) {
    if let Some(rm_id) = room_id
        && let Err(e) = state
            .client_manager
            .matrix
            .send_message(rm_id, msg, None, None)
            .await
    {
        warn!("Failed to notify room {}: {}", rm_id, e);
    }
}
