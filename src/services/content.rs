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
        // content type. Also bail to the HTML path if the "plain" part actually
        // contains HTML markup — some senders embed HTML islands (`<ol>`,
        // `<blockquote>`, `<figure>`) in their text/plain alternative, which
        // would otherwise be shown as literal tags in the timeline.
        if let Some((mut plain, is_truncated, content_type)) = email
            .text_body()
            .and_then(|parts| Self::extract_body(email, parts))
            && content_type.as_deref() != Some("text/html")
            && !looks_like_html(&plain)
        {
            if is_truncated {
                plain.push_str("\n\n[Email truncated by server due to size limit]");
            }
            return Self {
                plain: normalize_plain(&plain),
                html: None,
            };
        }

        // HTML body — either from htmlBody, or a textBody that was actually
        // text/html (or plain-with-HTML). Convert to text for the timeline and
        // keep a sanitized copy of the HTML as the formatted body.
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
            let plain = normalize_plain(
                &html2text::from_read(html.as_bytes(), 80).unwrap_or_else(|_| html.clone()),
            );
            return Self {
                plain,
                html: Some(clean_html_for_matrix(&html)),
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

/// Heuristic: does this text contain real HTML markup (a recognizable tag)?
/// Deliberately conservative so plain prose with a stray `<` or `<3` is not
/// misclassified.
#[must_use]
fn looks_like_html(s: &str) -> bool {
    const TAGS: &[&str] = &[
        "<div", "<p>", "<p ", "<br", "<a ", "<a>", "<table", "<td", "<tr", "<span", "<img",
        "<ul", "<ol", "<li", "<blockquote", "<figure", "<h1", "<h2", "<h3", "<h4", "<strong",
        "<em>", "<b>", "<i>", "<html", "<body", "<head", "<style", "<font", "<center",
    ];
    let bytes = s.as_bytes();
    TAGS.iter()
        .any(|t| find_ci(bytes, t.as_bytes(), 0).is_some())
}

/// ASCII-case-insensitive substring search returning the byte offset in
/// `haystack` (offsets land on char boundaries because matches start only at
/// ASCII bytes).
fn find_ci(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() || from > haystack.len() - needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&i| haystack[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Remove every `open..close` region (case-insensitive on the ASCII delimiters).
/// If an `open` has no matching `close`, the remainder is dropped.
fn strip_region(s: &str, open: &str, close: &str) -> String {
    let bytes = s.as_bytes();
    let (ob, cb) = (open.as_bytes(), close.as_bytes());
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let Some(start) = find_ci(bytes, ob, i) else {
            out.push_str(&s[i..]);
            break;
        };
        out.push_str(&s[i..start]);
        let Some(end) = find_ci(bytes, cb, start + ob.len()) else {
            break; // unterminated region: drop the rest
        };
        i = end + cb.len();
    }
    out
}

/// Reduce a full HTML email document to body content suitable for a Matrix
/// `formatted_body`: drop comments, `<head>`, `<style>`, `<script>`, the
/// doctype, and unwrap `<body>`. Matrix clients sanitize the rest on render, so
/// this just removes the bulk/clutter rather than enforcing the tag allowlist.
#[must_use]
fn clean_html_for_matrix(html: &str) -> String {
    let mut s = strip_region(html, "<!--", "-->");
    s = strip_region(&s, "<head", "</head>");
    s = strip_region(&s, "<style", "</style>");
    s = strip_region(&s, "<script", "</script>");
    s = strip_region(&s, "<!doctype", ">");

    // Unwrap to the inner content of <body> … </body> if present.
    let bytes = s.as_bytes();
    if let Some(open) = find_ci(bytes, b"<body", 0)
        && let Some(gt) = s[open..].find('>')
        && let Some(close) = find_ci(bytes, b"</body>", open)
    {
        let inner_start = open + gt + 1;
        if inner_start <= close {
            s = s[inner_start..close].to_owned();
        }
    }
    s.trim().to_owned()
}

/// Tidy converted plain text: drop invisible padding characters (soft hyphen,
/// zero-width space, BOM) that marketing emails use as preheader spacers, and
/// collapse 3+ blank lines.
#[must_use]
fn normalize_plain(s: &str) -> String {
    let filtered: String = s
        .chars()
        .filter(|&c| c != '\u{00AD}' && c != '\u{200B}' && c != '\u{FEFF}')
        .collect();
    let mut out = String::with_capacity(filtered.len());
    let mut newlines = 0u32;
    for c in filtered.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(c);
            }
        } else {
            newlines = 0;
            out.push(c);
        }
    }
    out.trim().to_owned()
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
    let file_name = part.name().unwrap_or("📎 attachment");

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
        // formatted_body is the sanitized body inner — no doctype/head/style.
        let html = body.html.as_deref().expect("html formatted body present");
        assert!(
            html.contains("<p>Hello") && html.contains("<b>world</b>"),
            "formatted body should keep the rendered content: {html}"
        );
        assert!(
            !html.contains("<!DOCTYPE")
                && !html.to_lowercase().contains("<head")
                && !html.to_lowercase().contains("<style"),
            "formatted body should be stripped of doctype/head/style: {html}"
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

    #[test]
    fn text_plain_part_with_html_islands_is_converted() {
        // Some senders (e.g. Buttondown) put HTML inside their text/plain
        // alternative. It must be converted, not shown as literal tags, and a
        // formatted_body produced.
        let email = email_from_json(serde_json::json!({
            "id": "e3",
            "threadId": "t3",
            "textBody": [{ "partId": "1", "type": "text/plain" }],
            "bodyValues": {
                "1": {
                    "value": "Intro line\n<ol><li>first</li><li>second</li></ol>\n<blockquote>quote</blockquote>",
                    "isTruncated": false
                }
            }
        }));
        let body = EmailBody::from_email(&email);
        assert!(
            !body.plain.contains("<ol>") && !body.plain.contains("<blockquote>"),
            "html islands must not survive as raw tags: {}",
            body.plain
        );
        assert!(body.html.is_some(), "a formatted body should be produced");
    }

    #[test]
    fn looks_like_html_detection() {
        use super::looks_like_html;
        assert!(looks_like_html("<ol><li>x</li></ol>"));
        assert!(looks_like_html("hi <blockquote>q</blockquote>"));
        assert!(looks_like_html("see <a href=\"x\">link</a>"));
        assert!(!looks_like_html("just normal prose, nothing here"));
        assert!(!looks_like_html("i <3 you and a < b"));
    }

    #[test]
    fn clean_html_strips_chrome_and_unwraps_body() {
        use super::clean_html_for_matrix;
        let dirty = "<!DOCTYPE html><html><head><style>/* css */</style></head>\
                     <body><!-- hi --><p>Hello <b>world</b></p></body></html>";
        let clean = clean_html_for_matrix(dirty);
        assert_eq!(clean, "<p>Hello <b>world</b></p>");
    }

    #[test]
    fn normalize_plain_strips_invisible_padding_and_blank_runs() {
        use super::normalize_plain;
        // soft hyphen + zero-width space padding, and 4 blank lines.
        let input = "Hi\u{00AD}\u{200B}there\n\n\n\n\nbye";
        assert_eq!(normalize_plain(input), "Hithere\n\nbye");
    }
}
