//! On-demand inline image loading.
//!
//! Newsletter `<img>` are remote `https://` URLs, which Matrix forbids inline
//! (only `mxc://` renders). Rather than fetch them server-side for every email —
//! which would trip the sender's tracking pixels automatically — we load a
//! single email's images only when the user opts in for that one message — by
//! reacting 🖼️ or replying `show-images` (ADR-0011). The message is then edited
//! in place to show the images inline. Strictly per-message: only the email the
//! user pointed at is touched.

use crate::matrix::MatrixClient;
use crate::routes::AppState;
use crate::services::content;
use anyhow::{Context, Result};
use jmap_client::client::Client;
use jmap_client::email::{Email, Property};
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Framed-picture emoji (U+1F5BC) — the reaction that loads an email's images.
const LOAD_IMAGES_CODEPOINT: char = '\u{1F5BC}';
/// Caps so opting in can't pull an unbounded amount of remote data.
const MAX_IMAGES: usize = 20;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// True if a reaction key is the "load images" emoji, tolerating the optional
/// U+FE0F variation selector and any skin-tone/extra codepoints.
#[must_use]
pub(crate) fn is_load_images_reaction(key: &str) -> bool {
    key.chars().any(|c| c == LOAD_IMAGES_CODEPOINT)
}

/// Load and inline the images of the single bridged email that `target_event_id`
/// refers to, then edit that message in place. No-op (logged) when the sender may
/// not use the bridge, the event isn't a bridged email, the user isn't logged in,
/// or there's nothing loadable.
///
/// Both the 🖼️ reaction and its `show-images` text twin land here, so the
/// permission gate lives on this shared path rather than in either caller — the
/// two must never drift (ADR-0011).
pub(crate) async fn handle_load_images(
    state: &AppState,
    sender_id: &str,
    room_id: &str,
    target_event_id: &str,
) -> Result<()> {
    // A de-permissioned user can't act, even on a room they once used, and even
    // while their JMAP client is still live (ADR-0010). Loading remote images
    // tells the correspondent their mail was opened, so it is theirs to withhold.
    if state.permissions.level_for(sender_id).is_none() {
        debug!(%sender_id, "Image load from a sender without permission; ignoring");
        return Ok(());
    }
    let store = &state.client_manager.store;
    let matrix = &state.client_manager.matrix;

    let Some(email_id) = store.get_email_id_from_event_id(target_event_id).await? else {
        debug!(%target_event_id, "Image load on a non-email event; ignoring");
        return Ok(());
    };
    // The m.replace edit must be authored by the original sender (the ghost).
    let Some(ghost_email) = store.get_ghost_email_by_room(room_id).await? else {
        debug!(%room_id, "Image load in a non-ghost room; ignoring");
        return Ok(());
    };
    let ghost_user_id = format!(
        "@{}:{}",
        crate::ghost::email_to_localpart(&ghost_email),
        matrix.domain
    );

    let Some(client) = state.client_manager.get_client(sender_id).await else {
        warn!(%sender_id, "No JMAP client for image load (not logged in?)");
        return Ok(());
    };
    let Some(email) = fetch_email(&client, &email_id).await? else {
        warn!(%email_id, "Email not found when loading images");
        return Ok(());
    };
    let Some(html) = content::original_html(&email) else {
        debug!(%email_id, "Email has no HTML body; nothing to load");
        return Ok(());
    };

    let plain = content::EmailBody::from_email(&email, content::RenderMode::Plain).plain;
    inline_email_images(
        matrix,
        room_id,
        target_event_id,
        &ghost_user_id,
        &html,
        &plain,
    )
    .await
}

/// Download an email's non-tracker remote images, upload each to the homeserver,
/// rewrite the body to reference them as `mxc://`, and edit the original event in
/// place. No-op (message left unchanged) when nothing is loadable. Split out from
/// the JMAP re-fetch / lookup glue so it can be tested without a live JMAP client.
async fn inline_email_images(
    matrix: &MatrixClient,
    room_id: &str,
    target_event_id: &str,
    ghost_user_id: &str,
    html: &str,
    plain: &str,
) -> Result<()> {
    let candidates: Vec<_> = content::extract_remote_images(html)
        .into_iter()
        .filter(|img| !img.is_decorative)
        .take(MAX_IMAGES)
        .collect();
    if candidates.is_empty() {
        debug!("No loadable images in email");
        return Ok(());
    }

    info!(
        count = candidates.len(),
        "Loading inline images on user request"
    );
    let mut url_to_mxc: HashMap<String, String> = HashMap::new();
    for img in candidates {
        let fetch_url = content::decode_src_entities(&img.url);
        match fetch_and_upload(matrix, ghost_user_id, &fetch_url).await {
            Ok(mxc) => {
                url_to_mxc.insert(img.url, mxc);
            }
            Err(e) => warn!(url = %fetch_url, error = %e, "Skipping image that failed to load"),
        }
    }
    if url_to_mxc.is_empty() {
        warn!("All images failed to load");
        return Ok(());
    }

    let rich = content::render_inline_images(html, &url_to_mxc);
    matrix
        .send_edit_as(room_id, target_event_id, plain, &rich, ghost_user_id)
        .await?;
    info!(
        loaded = url_to_mxc.len(),
        "Edited message with inline images"
    );
    Ok(())
}

/// Re-fetch a single email's HTML/text bodies from JMAP by id, mirroring the
/// poller's `fetch_emails` property set.
async fn fetch_email(client: &Client, email_id: &str) -> Result<Option<Email>> {
    let mut request = client.build();
    let email_req = request.get_email();
    email_req.ids([email_id]).properties([
        Property::Id,
        Property::Subject,
        Property::TextBody,
        Property::HtmlBody,
        Property::BodyValues,
    ]);
    email_req
        .arguments()
        .fetch_html_body_values(true)
        .fetch_text_body_values(true)
        .max_body_value_bytes(524_288);
    let mut response = request
        .send()
        .await?
        .pop_method_response()
        .context("Email/get failed")?
        .unwrap_get_email()?;
    Ok(response.take_list().into_iter().next())
}

/// Download a remote image and upload it to the homeserver, returning its
/// `mxc://`. Rejects non-image content types and anything over the size cap.
async fn fetch_and_upload(matrix: &MatrixClient, ghost_user_id: &str, url: &str) -> Result<String> {
    // SSRF-safe fetch: the URL comes from an attacker-controlled email `<img src>`,
    // so validate the host (and every redirect hop) resolves to a public address.
    let resp = crate::net::safe_get(url, 3).await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_owned())
        .unwrap_or_default();
    if !mime.starts_with("image/") {
        anyhow::bail!("not an image (content-type {mime:?})");
    }
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_IMAGE_BYTES)
    {
        anyhow::bail!("image too large");
    }
    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        anyhow::bail!("image too large ({} bytes)", bytes.len());
    }
    matrix.upload_media(ghost_user_id, &bytes, &mime).await
}

#[cfg(test)]
mod tests {
    use super::is_load_images_reaction;

    #[test]
    fn recognizes_framed_picture_reaction() {
        assert!(is_load_images_reaction("🖼️")); // U+1F5BC + U+FE0F
        assert!(is_load_images_reaction("🖼")); // bare U+1F5BC
        assert!(is_load_images_reaction("\u{1F5BC}"));
        assert!(!is_load_images_reaction("👍"));
        assert!(!is_load_images_reaction(""));
    }

    /// Mocked cover of the load→inline flow: an email's remote `<img>` is
    /// downloaded, uploaded to the homeserver, and the message is edited in place
    /// (m.replace) referencing it as `mxc://`. One `wiremock` server plays the
    /// image host and Matrix; the JMAP re-fetch/lookup glue is exercised
    /// separately (its pieces are reused, verified helpers).
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn inlines_remote_image_and_edits_message_with_mxc() {
        use crate::matrix::MatrixClient;
        use wiremock::matchers::{method, path, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let uri = server.uri();
        let img_url = format!("{uri}/img.png");
        // One remote image plus a 1×1 tracker pixel that must NOT be loaded.
        let html = format!(
            "<p>hello</p><img src=\"{img_url}\"><img src=\"{uri}/beacon.gif\" width=\"1\" height=\"1\">"
        );

        // The remote content image.
        Mock::given(method("GET"))
            .and(path("/img.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(vec![137, 80, 78, 71, 13, 10, 26, 10]),
            )
            .mount(&server)
            .await;

        // Matrix media upload -> mxc.
        Mock::given(method("POST"))
            .and(path_regex(r".*media.*upload.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content_uri": "mxc://localhost/IMG1"
            })))
            .mount(&server)
            .await;

        // Matrix send (the in-place edit lands here, type stays m.room.message).
        Mock::given(method("PUT"))
            .and(path_regex(r".*/send/m\.room\.message/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "event_id": "$edited:localhost"
            })))
            .mount(&server)
            .await;

        let matrix = MatrixClient::new(&uri, "as_token", "localhost")
            .await
            .unwrap();
        // The mock image host is on loopback, which the SSRF guard blocks in
        // production; permit it for this test only.
        let _allow_private = crate::net::test_support::allow_private();
        super::inline_email_images(
            &matrix,
            "!room:localhost",
            "$msg:localhost",
            "@_jmap_brad=40x.com:localhost",
            &html,
            "hello",
        )
        .await
        .unwrap();

        let reqs = server.received_requests().await.unwrap();
        // The tracker pixel was never fetched.
        assert!(
            !reqs.iter().any(|r| r.url.path() == "/beacon.gif"),
            "1x1 tracker must not be downloaded"
        );
        // The content image was, and the message was edited carrying its mxc.
        assert!(
            reqs.iter().any(|r| r.url.path() == "/img.png"),
            "content image fetched"
        );
        let edit = reqs
            .iter()
            .find(|r| r.url.path().contains("/send/m.room.message/"))
            .expect("an in-place edit must have been sent");
        let body = String::from_utf8_lossy(&edit.body);
        assert!(
            body.contains("mxc://localhost/IMG1"),
            "edit must inline the uploaded image: {body}"
        );
        assert!(
            body.contains("m.new_content"),
            "edit must be an m.replace with new content: {body}"
        );
    }

    /// Fixtures for the permission gate on the 🖼️ path (#97): the reaction must
    /// refuse exactly the senders its `show-images` text twin refuses (ADR-0010,
    /// ADR-0011).
    mod permission_gate {
        use crate::client_manager::ClientManager;
        use crate::matrix::MatrixClient;
        use crate::permissions::Permissions;
        use crate::puppet::PuppetManager;
        use crate::routes::AppState;
        use crate::state::StateStore;
        use crate::store::Store;
        use std::sync::Arc;
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const ROOM_ID: &str = "!room:localhost";
        const EVENT_ID: &str = "$msg:localhost";
        const EMAIL_ID: &str = "email-1";

        /// JMAP session discovery, so `login` can connect a real client.
        async fn mount_jmap(server: &MockServer) {
            let uri = server.uri();
            Mock::given(method("GET"))
                .and(path("/.well-known/jmap"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "username": "user",
                    "accounts": { "A1": { "name": "user", "isPersonal": true, "isReadOnly": false,
                        "accountCapabilities": {
                            "urn:ietf:params:jmap:core": {},
                            "urn:ietf:params:jmap:mail": {} } } },
                    "primaryAccounts": {
                        "urn:ietf:params:jmap:core": "A1",
                        "urn:ietf:params:jmap:mail": "A1" },
                    "apiUrl": format!("{uri}/api"),
                    "downloadUrl": format!("{uri}/download"),
                    "uploadUrl": format!("{uri}/upload"),
                    "eventSourceUrl": format!("{uri}/events"),
                    "capabilities": {
                        "urn:ietf:params:jmap:core": {},
                        "urn:ietf:params:jmap:mail": {} },
                    "state": "s1"
                })))
                .mount(server)
                .await;
            // The re-fetch the reaction makes. An empty list is enough: the
            // oracle is whether the request happens at all.
            Mock::given(method("POST"))
                .and(path("/api"))
                .and(body_string_contains("Email/get"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "sessionState": "s1",
                    "methodResponses": [["Email/get", { "accountId": "A1", "state": "s", "list": [], "notFound": [] }, "0"]]
                })))
                .mount(server)
                .await;
        }

        /// An `AppState` where `sender` holds a live JMAP client and reacted to a
        /// bridged email in a ghost room — the de-permissioned-but-logged-in
        /// situation #97 is about. `permissions` decides whether they may act.
        #[allow(clippy::unwrap_used)]
        async fn state_with_logged_in_sender(
            server: &MockServer,
            sender: &str,
            permissions: Permissions,
        ) -> AppState {
            let store = Store::new_in_memory(None).await.unwrap();
            let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
                .await
                .unwrap();
            let client_manager = Arc::new(ClientManager::new(store.clone(), matrix, 10));
            client_manager
                .login(
                    sender.to_owned(),
                    "user".to_owned(),
                    "secret".to_owned(),
                    server.uri(),
                )
                .await
                .unwrap();
            // The poller issues JMAP calls of its own; stop it so every request
            // the assertions see belongs to the reaction.
            client_manager.abort_poller(sender).await;

            store
                .save_message_mapping(EMAIL_ID, EVENT_ID)
                .await
                .unwrap();
            store
                .save_room_ghost_mapping(ROOM_ID, "brad@x.com", sender)
                .await
                .unwrap();

            AppState {
                client_manager,
                state_store: Arc::new(StateStore::new()),
                puppet_manager: Arc::new(PuppetManager::new(
                    String::new(),
                    "@_jmap_bot:localhost".to_owned(),
                )),
                permissions: Arc::new(permissions),
                double_puppet_secret: None,
                hs_token: "hs_token".to_owned(),
            }
        }

        /// How many times the email was re-fetched from JMAP — the first thing the
        /// reaction does with the user's credentials, and so the visible edge of
        /// "this action ran".
        #[allow(clippy::unwrap_used)]
        async fn email_fetches(server: &MockServer) -> usize {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| {
                    r.url.path() == "/api" && String::from_utf8_lossy(&r.body).contains("Email/get")
                })
                .count()
        }

        #[tokio::test]
        #[allow(clippy::unwrap_used)]
        async fn permitted_sender_loads_images() {
            let server = MockServer::start().await;
            mount_jmap(&server).await;
            let state = state_with_logged_in_sender(
                &server,
                "@user:localhost",
                Permissions::from_specs(&["@user:localhost=user".to_owned()], "localhost").unwrap(),
            )
            .await;

            super::super::handle_load_images(&state, "@user:localhost", ROOM_ID, EVENT_ID)
                .await
                .unwrap();

            assert_eq!(
                email_fetches(&server).await,
                1,
                "a permitted sender's 🖼️ must re-fetch the email to load its images"
            );
        }

        /// The gap #97 reports: a sender who has been de-permissioned but still
        /// has a live JMAP client could load remote images by reacting, while the
        /// identical `show-images` command refused them. Loading images signals to
        /// the correspondent that their mail was opened, so the reaction must
        /// refuse too.
        #[tokio::test]
        #[allow(clippy::unwrap_used)]
        async fn unpermitted_sender_loads_nothing() {
            let server = MockServer::start().await;
            mount_jmap(&server).await;
            let state = state_with_logged_in_sender(
                &server,
                "@denied:localhost",
                // Permits somebody else; "@denied:localhost" matches no entry.
                Permissions::from_specs(&["@other:localhost=user".to_owned()], "localhost")
                    .unwrap(),
            )
            .await;

            super::super::handle_load_images(&state, "@denied:localhost", ROOM_ID, EVENT_ID)
                .await
                .unwrap();

            assert_eq!(
                email_fetches(&server).await,
                0,
                "a de-permissioned sender's 🖼️ must not touch their mail"
            );
            // ...and nothing reached the room: no in-place edit was sent.
            assert!(
                !server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.url.path().contains("/send/m.room.message/")),
                "a de-permissioned sender's 🖼️ must not edit the message"
            );
        }
    }
}
