//! Ingestion and bridging of content (bodies and attachments) from JMAP to Matrix.

use crate::matrix::MatrixClient;
use crate::store::{Store, ThreadRepository};
use anyhow::{Context, Result};
use jmap_client::client::Client;
use jmap_client::email::{Email, EmailBodyPart};
use tracing::warn;

const NO_SUBJECT: &str = "(No Subject)";

/// Internal representation of an email's content for bridging.
#[derive(Debug)]
pub struct EmailBody {
    /// Plain-text content (shown in Matrix timeline).
    pub plain: String,
    /// Optional HTML content (sent as the formatted body).
    pub html: Option<String>,
}

impl EmailBody {
    /// Extract the best available body from a JMAP Email.
    #[must_use]
    pub fn from_email(email: &Email) -> Self {
        let subject = email.subject().unwrap_or(NO_SUBJECT);

        // Prefer a genuine text/plain body. JMAP's textBody points at the HTML
        // part when an email has no plain alternative, so guard on the part's
        // content type — otherwise raw HTML is shown verbatim in the timeline.
        if let Some((mut plain, is_truncated, content_type)) = email
            .text_body()
            .and_then(|parts| Self::extract_body(email, parts))
            && content_type.as_deref() != Some("text/html")
        {
            if is_truncated {
                plain.push_str("\n\n[Email truncated by server due to size limit]");
            }
            return Self { plain, html: None };
        }

        // HTML body — either from htmlBody, or a textBody that was actually
        // text/html. Convert to text for the timeline and keep the HTML as the
        // formatted body for clients that render it.
        if let Some((mut html, is_truncated, _)) = email
            .html_body()
            .and_then(|parts| Self::extract_body(email, parts))
            .or_else(|| {
                email
                    .text_body()
                    .and_then(|parts| Self::extract_body(email, parts))
            })
        {
            if is_truncated {
                html.push_str(
                    "<br><br><strong>[Email truncated by server due to size limit]</strong>",
                );
            }
            let plain = html2text::from_read(html.as_bytes(), 80).unwrap_or_else(|_| html.clone());
            return Self {
                plain,
                html: Some(html),
            };
        }

        Self {
            plain: subject.to_owned(),
            html: None,
        }
    }

    fn extract_body(email: &Email, parts: &[EmailBodyPart]) -> Option<(String, bool, Option<String>)> {
        let part = parts.first()?;
        let part_id = part.part_id()?;
        let content_type = part.content_type().map(str::to_owned);
        email
            .body_value(part_id)
            .map(|v| (v.value().to_owned(), v.is_truncated(), content_type))
    }
}

/// Bridge JMAP attachments to Matrix media repository and send them in the room.
#[allow(clippy::too_many_arguments)]
pub async fn handle_attachments(
    client: &Client,
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    email: &Email,
    room_id: &str,
    thread_root_id: Option<&str>,
    thread_latest_event_id: Option<&str>,
    sender_id: &str,
    timestamp: Option<u64>,
) -> Result<()> {
    let attachments = email.attachments().unwrap_or(&[]);
    if attachments.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        "Email has {} attachments. Preparing to bridge them.",
        attachments.len()
    );

    let session = client.session();
    let download_template = session.download_url();
    // Use the session's default (primary) account, the same lookup the rest of
    // the bridge uses. Filtering primaryAccounts for the `core` capability
    // specifically returns nothing on Stalwart (which registers the primary
    // account under the `mail` capability), so attachment downloads failed with
    // "No account".
    let account_id = client.default_account_id();
    let thread_id = email.thread_id();
    let mut latest_owned_id = None;

    for part in attachments {
        let next_latest = latest_owned_id.as_deref().or(thread_latest_event_id);
        match bridge_attachment(
            matrix,
            store,
            matrix_user_id,
            part,
            &matrix.http_client,
            download_template,
            account_id,
            room_id,
            thread_root_id,
            next_latest,
            sender_id,
            timestamp,
        )
        .await
        {
            Ok(evt_id) => {
                // Update latest event ID for subsequent attachments in this email
                latest_owned_id = Some(evt_id.clone());
                // Also update the store if we have a thread_id
                if let Some(tid) = thread_id {
                    if let Err(e) = store.update_thread_latest_event(tid, &evt_id).await {
                        warn!(error = %e, %tid, "Failed to update thread latest event for attachment");
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to bridge attachment");
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn bridge_attachment(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    part: &EmailBodyPart,
    http: &reqwest::Client,
    download_template: &str,
    account_id: &str,
    room_id: &str,
    thread_root_id: Option<&str>,
    thread_latest_event_id: Option<&str>,
    sender_id: &str,
    timestamp: Option<u64>,
) -> Result<String> {
    let blob_id = part.blob_id().context("No blobId")?;
    let mime_type = part.content_type().unwrap_or("application/octet-stream");
    let file_name = part.name().unwrap_or("attachment");

    let url = download_template
        .replace("{accountId}", account_id)
        .replace("{blobId}", blob_id)
        .replace("{name}", file_name);

    // Memory Management: Check size before downloading
    let size = part.size();

    // JMAP sessions advertise a maxSizeUpload, but for now we use a safe 50MB limit
    // to protect the bridge's memory.
    let max_upload = 50 * 1024 * 1024;

    if size > max_upload {
        anyhow::bail!("Attachment too large ({size} bytes). Skipping to protect memory.");
    }

    let user = store
        .get_user(matrix_user_id)
        .await?
        .context("User session credentials missing from store")?;

    // The bridge authenticates to JMAP with Basic auth (jmap-client connects
    // with `Credentials::Basic`), so the blob download must match. Sending a
    // Bearer token here made Stalwart reject the download with 401.
    let resp = http
        .get(&url)
        .basic_auth(&user.jmap_username, Some(&user.jmap_token))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Download failed: {}", resp.status());
    }
    let stream = resp.bytes_stream();

    let mxc_url = matrix
        .upload_media_stream(sender_id, stream, mime_type, file_name)
        .await?;
    let event_id = matrix
        .send_file_as(
            room_id,
            &mxc_url,
            file_name,
            mime_type,
            thread_root_id,
            thread_latest_event_id,
            sender_id,
            timestamp,
        )
        .await?;
    Ok(event_id)
}

/// Appends the user's custom signature to the body text if configured.
pub async fn append_user_signature(store: &Store, user_id: &str, body: &mut String) -> Result<()> {
    if let Some(sig) = store.get_user_signature(user_id).await? {
        if !sig.is_empty() {
            body.push_str("\n\n-- \n");
            body.push_str(&sig);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::EmailBody;
    use jmap_client::email::Email;

    fn email_from_json(v: serde_json::Value) -> Email {
        serde_json::from_value(v).expect("valid Email json")
    }

    #[test]
    fn html_only_email_is_converted_not_shown_raw() {
        // JMAP returns the HTML part in textBody when an email has no plain
        // alternative; the bridge must convert it, not show raw markup.
        let email = email_from_json(serde_json::json!({
            "id": "e1",
            "threadId": "t1",
            "textBody": [{ "partId": "1", "type": "text/html" }],
            "bodyValues": {
                "1": {
                    "value": "<!DOCTYPE html><html><head><style>p{}</style></head><body><p>Hello <b>world</b></p></body></html>",
                    "isTruncated": false
                }
            }
        }));
        let body = EmailBody::from_email(&email);
        assert!(
            !body.plain.contains("<!DOCTYPE") && !body.plain.contains("<html"),
            "timeline body should be rendered text, not raw HTML: {}",
            body.plain
        );
        assert!(
            body.plain.to_lowercase().contains("hello"),
            "rendered text should contain the message: {}",
            body.plain
        );
        assert!(
            body.html.as_deref().is_some_and(|h| h.contains("<body>")),
            "the HTML should be kept as the formatted body"
        );
    }

    #[test]
    fn plain_text_email_is_used_verbatim() {
        let email = email_from_json(serde_json::json!({
            "id": "e2",
            "threadId": "t2",
            "textBody": [{ "partId": "1", "type": "text/plain" }],
            "bodyValues": {
                "1": { "value": "Just plain text", "isTruncated": false }
            }
        }));
        let body = EmailBody::from_email(&email);
        assert_eq!(body.plain, "Just plain text");
        assert!(body.html.is_none());
    }
}
