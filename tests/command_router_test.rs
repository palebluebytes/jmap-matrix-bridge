#![allow(clippy::unwrap_used)]

#[cfg(test)]
mod tests {
    use jmap_matrix_bridge::commands::{CommandContext, CommandRouter};
    use matrix_sdk::ruma::events::relation::InReplyTo;
    use matrix_sdk::ruma::events::room::message::{
        MessageType, RoomMessageEventContent, TextMessageEventContent,
    };

    #[tokio::test]
    async fn test_command_router_matching() {
        let router = CommandRouter::new();

        let content = RoomMessageEventContent::text_plain("help");
        let ctx = CommandContext {
            sender_id: "@alice:localhost",
            body_str: "help",
            room_id: Some("!room:localhost"),
            event_id: Some("$event:localhost"),
            message_content: &content,
        };

        // The help command should match
        let matched = router.matches_any(&ctx);
        assert!(matched, "Expected help command to match");
    }

    #[tokio::test]
    async fn test_unknown_command_no_match() {
        let router = CommandRouter::new();

        let content = RoomMessageEventContent::text_plain("not a command");
        let ctx = CommandContext {
            sender_id: "@alice:localhost",
            body_str: "not a command",
            room_id: Some("!room:localhost"),
            event_id: Some("$event:localhost"),
            message_content: &content,
        };

        let matched = router.matches_any(&ctx);
        assert!(
            !matched,
            "Expected unknown command not to match any registered commands"
        );
    }

    #[tokio::test]
    async fn test_signature_command_matching() {
        let router = CommandRouter::new();

        let content = RoomMessageEventContent::text_plain("signature hello");
        let ctx = CommandContext {
            sender_id: "@alice:localhost",
            body_str: "signature hello",
            room_id: Some("!room:localhost"),
            event_id: Some("$event:localhost"),
            message_content: &content,
        };

        let matched = router.matches_any(&ctx);
        assert!(matched, "Expected signature command to match");
    }

    #[tokio::test]
    async fn test_email_command_matching() {
        let router = CommandRouter::new();

        let content = RoomMessageEventContent::text_plain("!email test@example.com Subject Body");
        let ctx = CommandContext {
            sender_id: "@alice:localhost",
            body_str: "!email test@example.com Subject Body",
            room_id: Some("!room:localhost"),
            event_id: Some("$event:localhost"),
            message_content: &content,
        };

        let matched = router.matches_any(&ctx);
        assert!(matched, "Expected email command to match");
    }

    #[tokio::test]
    async fn test_compose_command_matching() {
        let router = CommandRouter::new();

        for body in [
            "!compose new@example.com Subject",
            "!email-to new@example.com",
        ] {
            let content = RoomMessageEventContent::text_plain(body);
            let ctx = CommandContext {
                sender_id: "@alice:localhost",
                body_str: body,
                room_id: Some("!room:localhost"),
                event_id: Some("$event:localhost"),
                message_content: &content,
            };
            assert!(
                router.matches_any(&ctx),
                "Expected compose command to match: {body}"
            );
        }

        // A word that merely starts with the command name must NOT match.
        let content = RoomMessageEventContent::text_plain("!composed thoughts");
        let ctx = CommandContext {
            sender_id: "@alice:localhost",
            body_str: "!composed thoughts",
            room_id: Some("!room:localhost"),
            event_id: Some("$event:localhost"),
            message_content: &content,
        };
        assert!(
            !router.matches_any(&ctx),
            "Did not expect '!composed' to match the compose command"
        );
    }

    #[tokio::test]
    async fn test_reply_command_matching() {
        let router = CommandRouter::new();

        // Create a RoomMessageEventContent with a reply relation
        let text_content = TextMessageEventContent::plain("my reply message");
        let mut content = RoomMessageEventContent::new(MessageType::Text(text_content));
        content.relates_to = Some(matrix_sdk::ruma::events::room::message::Relation::Reply {
            in_reply_to: InReplyTo::new("$original_event_id:localhost".try_into().unwrap()),
        });

        let ctx = CommandContext {
            sender_id: "@alice:localhost",
            body_str: "my reply message",
            room_id: Some("!room:localhost"),
            event_id: Some("$reply_event:localhost"),
            message_content: &content,
        };

        let matched = router.matches_any(&ctx);
        assert!(
            matched,
            "Expected reply command to match when relates_to is present"
        );
    }
}
