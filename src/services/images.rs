//! On-demand inline image loading.
//!
//! Newsletter `<img>` are remote `https://` URLs, which Matrix forbids inline
//! (only `mxc://` renders). Rather than fetch them server-side for every email —
//! which would trip the sender's tracking pixels automatically — we load a
//! single email's images only when the user opts in by reacting to that one
//! message with 🖼️. The reacted message is then edited in place to show the
//! images inline. Strictly per-message: only the reacted email is touched.

use crate::matrix::MatrixClient;
use crate::routes::AppState;
use crate::services::content;
use anyhow::{Context, Result};
use jmap_client::client::Client;
use jmap_client::email::{Email, Property};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Framed-picture emoji (U+1F5BC) — the reaction that loads an email's images.
const LOAD_IMAGES_CODEPOINT: char = '\u{1F5BC}';
/// Caps so opting in can't pull an unbounded amount of remote data.
const MAX_IMAGES: usize = 20;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const FETCH_TIMEOUT_SECS: u64 = 15;

/// True if a reaction key is the "load images" emoji, tolerating the optional
/// U+FE0F variation selector and any skin-tone/extra codepoints.
#[must_use]
pub(crate) fn is_load_images_reaction(key: &str) -> bool {
    key.chars().any(|c| c == LOAD_IMAGES_CODEPOINT)
}

/// Load and inline the images of the single bridged email that `reacted_event_id`
/// refers to, then edit that message in place. No-op (logged) when the event
/// isn't a bridged email, the user isn't logged in, or there's nothing loadable.
pub(crate) async fn handle_load_images_reaction(
    state: &AppState,
    user_sender_id: &str,
    room_id: &str,
    reacted_event_id: &str,
) -> Result<()> {
    let store = &state.client_manager.store;
    let matrix = &state.client_manager.matrix;

    let Some(email_id) = store.get_email_id_from_event_id(reacted_event_id).await? else {
        debug!(%reacted_event_id, "Image reaction on a non-email event; ignoring");
        return Ok(());
    };
    // The m.replace edit must be authored by the original sender (the ghost).
    let Some(ghost_email) = store.get_ghost_email_by_room(room_id).await? else {
        debug!(%room_id, "Image reaction in a non-ghost room; ignoring");
        return Ok(());
    };
    let ghost_user_id = format!(
        "@{}:{}",
        crate::ghost::email_to_localpart(&ghost_email),
        matrix.domain
    );

    let Some(client) = state.client_manager.get_client(user_sender_id).await else {
        warn!(%user_sender_id, "No JMAP client for image reaction (not logged in?)");
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
    inline_email_images(matrix, room_id, reacted_event_id, &ghost_user_id, &html, &plain).await
}

/// Download an email's non-tracker remote images, upload each to the homeserver,
/// rewrite the body to reference them as `mxc://`, and edit the original event in
/// place. No-op (message left unchanged) when nothing is loadable. Split out from
/// the JMAP re-fetch / lookup glue so it can be tested without a live JMAP client.
async fn inline_email_images(
    matrix: &MatrixClient,
    room_id: &str,
    reacted_event_id: &str,
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

    info!(count = candidates.len(), "Loading inline images on user request");
    let mut url_to_mxc: HashMap<String, String> = HashMap::new();
    for img in candidates {
        let fetch_url = content::decode_src_entities(&img.url);
        match fetch_and_upload(&matrix.http_client, matrix, ghost_user_id, &fetch_url).await {
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
        .send_edit_as(room_id, reacted_event_id, plain, &rich, ghost_user_id)
        .await?;
    info!(loaded = url_to_mxc.len(), "Edited message with inline images");
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
async fn fetch_and_upload(
    http: &reqwest::Client,
    matrix: &MatrixClient,
    ghost_user_id: &str,
    url: &str,
) -> Result<String> {
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .send()
        .await?;
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
    if resp.content_length().is_some_and(|len| len > MAX_IMAGE_BYTES) {
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

        let matrix = MatrixClient::new(&uri, "as_token", "localhost").await.unwrap();
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
        assert!(reqs.iter().any(|r| r.url.path() == "/img.png"), "content image fetched");
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
}
