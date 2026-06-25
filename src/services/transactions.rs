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
                // A 🖼️ reaction from the user on a bridged email loads that one
                // email's images and edits the message in place (per-message,
                // opt-in — see services::images). Image fetch + upload is slow
                // network I/O, so run it detached rather than blocking the
                // transaction ACK (a homeserver retry would dedupe anyway).
                if let matrix_sdk::ruma::events::AnyMessageLikeEvent::Reaction(reaction) = &e
                    && let Some(annotation) =
                        reaction.as_original().map(|ev| &ev.content.relates_to)
                    && crate::services::images::is_load_images_reaction(&annotation.key)
                {
                    if let Some(rm_id) = room_id.as_deref() {
                        let state = state.clone();
                        let sender_id = sender_id.clone();
                        let rm_id = rm_id.to_owned();
                        let target = annotation.event_id.to_string();
                        tokio::spawn(async move {
                            if let Err(err) = crate::services::images::handle_load_images_reaction(
                                &state, &sender_id, &rm_id, &target,
                            )
                            .await
                            {
                                error!(%sender_id, %rm_id, error = %err, "Image-load reaction failed");
                            }
                        });
                    }
                    continue;
                }
                // 🗑/🚫 reactions are the gesture twins of `delete-room`/`spam`:
                // move the whole thread to Trash/Junk and unbridge the room. Run
                // detached — the JMAP move is network I/O.
                if let matrix_sdk::ruma::events::AnyMessageLikeEvent::Reaction(reaction) = &e
                    && let Some(annotation) =
                        reaction.as_original().map(|ev| &ev.content.relates_to)
                {
                    let action = if crate::ghost::is_trash_reaction(&annotation.key) {
                        Some((jmap_client::mailbox::Role::Trash, "Trash"))
                    } else if crate::ghost::is_junk_reaction(&annotation.key) {
                        Some((jmap_client::mailbox::Role::Junk, "Junk"))
                    } else {
                        None
                    };
                    if let Some((role, label)) = action {
                        if let Some(rm_id) = room_id.as_deref() {
                            let state = state.clone();
                            let sender_id = sender_id.clone();
                            let rm_id = rm_id.to_owned();
                            tokio::spawn(async move {
                                if let Err(err) = crate::ghost::handle_thread_action(
                                    &state, &sender_id, &rm_id, role, label,
                                )
                                .await
                                {
                                    error!(%sender_id, %rm_id, error = %err, "Thread {label} action failed");
                                }
                            });
                        }
                        continue;
                    }
                }
                // A redaction of the user's own still-queued message cancels the
                // send within the send-delay window (ADR-0012); past the window
                // (already submitted) it's a silent no-op.
                if let matrix_sdk::ruma::events::AnyMessageLikeEvent::RoomRedaction(redaction) = &e
                {
                    if let Some(redacted) = redaction
                        .as_original()
                        .and_then(|r| r.redacts.clone().or_else(|| r.content.redacts.clone()))
                    {
                        match state
                            .client_manager
                            .store
                            .cancel_outbound_by_event(redacted.as_str())
                            .await
                        {
                            Ok(true) => {
                                notify(
                                    state,
                                    room_id.as_deref(),
                                    "🗙 Unsent — that message was still within the send-delay window.",
                                )
                                .await;
                            }
                            Ok(false) => {}
                            Err(err) => {
                                error!(error = %err, "Failed to cancel queued send on redaction");
                            }
                        }
                    }
                    continue;
                }
                if let matrix_sdk::ruma::events::AnyMessageLikeEvent::RoomMessage(message_event) = e
                    && let Some(content) = message_event.as_original().map(|ev| &ev.content)
                {
                    // An edit (m.replace) of the user's own still-queued message
                    // rewrites its body within the send-delay window (ADR-0012).
                    // Intercept it so it is never treated as a fresh outbound
                    // message (which would send a second email).
                    if let Some(matrix_sdk::ruma::events::room::message::Relation::Replacement(
                        repl,
                    )) = &content.relates_to
                    {
                        let target = repl.event_id.to_string();
                        let new_body = repl.new_content.msgtype.body().to_owned();
                        match state
                            .client_manager
                            .store
                            .update_outbound_body_by_event(&target, &new_body)
                            .await
                        {
                            Ok(true) => {
                                notify(
                                    state,
                                    room_id.as_deref(),
                                    "✎ Updated the queued message before it sends.",
                                )
                                .await;
                            }
                            Ok(false) => {}
                            Err(err) => {
                                error!(error = %err, "Failed to apply edit to queued send");
                            }
                        }
                        continue;
                    }

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
