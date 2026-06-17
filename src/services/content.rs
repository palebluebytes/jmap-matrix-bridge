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

/// How an email's body is rendered into a Matrix message.
///
/// Element shows `formatted_body` (HTML) when present, otherwise the plain
/// `body`. A clickable *named* link ("Confirm your subscription") therefore
/// requires HTML — plain text can only auto-link bare URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    /// Plain text only; never emit a formatted body. Cleanest, but links are
    /// just bare URLs and there are no buttons.
    Plain,
    /// Plain text plus a lightweight formatted body that keeps links (so
    /// buttons become clickable links) and basic formatting, but drops images,
    /// layout containers and styling. The default.
    #[default]
    Links,
    /// Plain text plus the full cleaned HTML as the formatted body — closest to
    /// the email's real layout (images, formatting), but busier.
    Rich,
}

impl std::str::FromStr for RenderMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plain" => Ok(Self::Plain),
            "links" => Ok(Self::Links),
            "rich" | "html" => Ok(Self::Rich),
            other => Err(format!(
                "unknown render mode {other:?} (expected plain, links or rich)"
            )),
        }
    }
}

impl EmailBody {
    /// Extract the best available body from a JMAP Email, rendered per `mode`.
    #[must_use]
    pub fn from_email(email: &Email, mode: RenderMode) -> Self {
        let subject = email.subject().unwrap_or(NO_SUBJECT);
        let plain_part = Self::plain_candidate(email);
        let html_part = Self::html_candidate(email);

        // Timeline plain text: prefer a genuine text/plain part; otherwise
        // render the HTML to text; otherwise fall back to the subject.
        let (raw, is_truncated) = if let Some((text, trunc)) = &plain_part {
            (text.clone(), *trunc)
        } else if let Some((html, trunc)) = &html_part {
            (
                html2text::from_read(html.as_bytes(), 80).unwrap_or_else(|_| html.clone()),
                *trunc,
            )
        } else {
            (subject.to_owned(), false)
        };
        let mut body = strip_quoted_reply(&raw);
        if is_truncated {
            body.push_str("\n\n[Email truncated by server due to size limit]");
        }
        let plain = normalize_plain(&body);

        // Formatted body: only when the mode wants HTML and an HTML
        // representation exists (htmlBody, or a textBody that was actually HTML).
        let html = match mode {
            RenderMode::Plain => None,
            RenderMode::Links | RenderMode::Rich => html_part.map(|(mut html, trunc)| {
                if trunc {
                    html.push_str(
                        "<br><br><strong>[Email truncated by server due to size limit]</strong>",
                    );
                }
                match mode {
                    RenderMode::Rich => clean_html_for_matrix(&html),
                    _ => lightweight_html(&html),
                }
            }),
        };

        Self { plain, html }
    }

    /// A genuine `text/plain` body part (not HTML). JMAP's textBody points at
    /// the HTML part when an email has no plain alternative, so guard on the
    /// part's content type, and reject a "plain" part that actually contains
    /// HTML markup (some senders embed `<ol>`/`<blockquote>`/`<figure>` islands).
    fn plain_candidate(email: &Email) -> Option<(String, bool)> {
        let (text, truncated, content_type) = email
            .text_body()
            .and_then(|parts| Self::extract_body(email, parts))?;
        if content_type.as_deref() == Some("text/html") || looks_like_html(&text) {
            return None;
        }
        Some((text, truncated))
    }

    /// The best HTML representation: `htmlBody`, or a `textBody` that was
    /// actually `text/html` (or plain-with-HTML islands). Returns `None` for a
    /// plain-text-only email — JMAP points `htmlBody` at the `text/plain` part
    /// when there is no HTML alternative (symmetric to `textBody` pointing at
    /// HTML), so guard on the part's content type in BOTH branches; otherwise
    /// plain text would be emitted verbatim as a bogus formatted body, skipping
    /// the quote-strip.
    fn html_candidate(email: &Email) -> Option<(String, bool)> {
        if let Some((html, truncated, content_type)) = email
            .html_body()
            .and_then(|parts| Self::extract_body(email, parts))
            && (content_type.as_deref() == Some("text/html") || looks_like_html(&html))
        {
            return Some((html, truncated));
        }
        let (text, truncated, content_type) = email
            .text_body()
            .and_then(|parts| Self::extract_body(email, parts))?;
        if content_type.as_deref() == Some("text/html") || looks_like_html(&text) {
            Some((text, truncated))
        } else {
            None
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
    // Drop quoted-reply blocks (`<blockquote …>…</blockquote>`, as Proton/Gmail/
    // Apple Mail wrap the quoted original): the prior message is already in the
    // room, so it is noise in the formatted body.
    s = strip_region(&s, "<blockquote", "</blockquote>");
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

    // Linearize table-based newsletter layouts: drop the scaffolding tags
    // (keeping their content). Matrix clients strip the CSS that newsletters use
    // for layout (max-width, word-break), so a `<table>` column is sized to its
    // widest element (a code block or long URL) and stretches every paragraph
    // onto ultra-wide, non-wrapping lines. Removing the table tags lets the
    // paragraphs flow as top-level blocks and wrap to the viewport. Longest tag
    // first so e.g. `<thead>` is gone before the `<th>` pass.
    for tag in [
        "table", "thead", "tbody", "tfoot", "colgroup", "col", "tr", "td", "th", "center",
    ] {
        s = strip_region(&s, &format!("<{tag}"), ">");
        s = strip_region(&s, &format!("</{tag}"), ">");
    }
    s.trim().to_owned()
}

/// Reduce an HTML email to a lightweight formatted body: keep text, links (so
/// buttons become clickable links) and basic inline/list formatting, but drop
/// images and layout containers. Builds on `clean_html_for_matrix` (which
/// already removes chrome and quoted blocks and linearizes tables), then strips
/// images and unwraps `<div>`/`<span>`/`<font>` so the content flows as text.
#[must_use]
fn lightweight_html(html: &str) -> String {
    let mut s = clean_html_for_matrix(html);
    s = strip_region(&s, "<img", ">"); // images dropped entirely
    for tag in ["div", "span", "font"] {
        s = strip_region(&s, &format!("<{tag}"), ">");
        s = strip_region(&s, &format!("</{tag}"), ">");
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

/// Drop the quoted-reply trailer that mail clients append when replying — the
/// attribution line ("On … wrote:" / Outlook "Original Message" divider) and
/// everything after it (the `>`-quoted original). In a Matrix room the prior
/// message is already visible, so the quote is pure noise. Deliberately
/// conservative: it only cuts at a recognized attribution line, so ordinary
/// prose is never truncated. Line endings are normalized to `\n` (the following
/// `normalize_plain` pass tidies the result regardless).
#[must_use]
fn strip_quoted_reply(s: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for line in s.lines() {
        let t = line.trim();
        let is_attribution = t.starts_with("On ") && t.ends_with("wrote:");
        let is_divider = t.eq_ignore_ascii_case("-----original message-----");
        if is_attribution || is_divider {
            break;
        }
        kept.push(line);
    }
    kept.join("\n").trim_end().to_owned()
}

/// Build a standard email reply quote: an attribution line followed by the
/// parent message, each line `>`-prefixed. This is the INVERSE of
/// [`strip_quoted_reply`] — the attribution is emitted as `On {date}, {from}
/// wrote:` precisely so that `strip_quoted_reply` recognises and removes it
/// (the `starts_with("On ") && ends_with("wrote:")` rule). That matched pair is
/// what lets the bridge add a quote to outbound email while keeping the Matrix
/// timeline clean: when the quote round-trips back, it is stripped on ingest.
#[must_use]
pub(crate) fn format_reply_quote(from: &str, date: &str, body: &str) -> String {
    let quoted = body
        .lines()
        .map(|l| {
            if l.is_empty() {
                ">".to_owned()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("On {date}, {from} wrote:\n{quoted}")
}

/// Format a Unix epoch (seconds, UTC) as `YYYY-MM-DD HH:MM UTC` for a reply
/// attribution line. Dependency-free (the crate carries no date library): the
/// calendar date comes from Howard Hinnant's `civil_from_days` algorithm.
#[must_use]
pub(crate) fn format_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hour = secs / 3_600;
    let min = (secs % 3_600) / 60;
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02} UTC")
}

/// Days since the Unix epoch (1970-01-01) → `(year, month, day)`.
/// Howard Hinnant's `civil_from_days` (<http://howardhinnant.github.io/date_algorithms.html>).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
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
    use super::{EmailBody, RenderMode};
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
        let body = EmailBody::from_email(&email, RenderMode::Rich);
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
        let body = EmailBody::from_email(&email, RenderMode::Links);
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
        let body = EmailBody::from_email(&email, RenderMode::Links);
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
    fn clean_html_linearizes_table_layout() {
        use super::clean_html_for_matrix;
        // A newsletter table wrapper must be unwrapped so the paragraphs flow as
        // top-level blocks (otherwise Matrix renders them on ultra-wide lines).
        let dirty = "<table style=\"max-width:600px\"><tbody><tr>\
                     <td><p>First para</p><p>Second para</p></td></tr></tbody></table>";
        let clean = clean_html_for_matrix(dirty);
        assert_eq!(clean, "<p>First para</p><p>Second para</p>");
    }

    #[test]
    fn normalize_plain_strips_invisible_padding_and_blank_runs() {
        use super::normalize_plain;
        // soft hyphen + zero-width space padding, and 4 blank lines.
        let input = "Hi\u{00AD}\u{200B}there\n\n\n\n\nbye";
        assert_eq!(normalize_plain(input), "Hithere\n\nbye");
    }

    #[test]
    fn strip_quoted_reply_cuts_attribution_and_quote() {
        use super::strip_quoted_reply;
        // ProtonMail-style reply: new text, then the attribution + `>` quote.
        let input = "09123\n\nOn Tuesday, June 16th, 2026 at 23:23, \
                     Thomas Kelly <thomas@palebluebytes.space> wrote:\n\n> 5678";
        assert_eq!(strip_quoted_reply(input), "09123");
        // Outlook divider variant.
        let outlook = "my reply\n\n-----Original Message-----\nFrom: a@b\n> old";
        assert_eq!(strip_quoted_reply(outlook), "my reply");
        // No quote: text is preserved (line endings normalized to \n).
        assert_eq!(
            strip_quoted_reply("just a message\nsecond line"),
            "just a message\nsecond line"
        );
        // A line that merely mentions writing is NOT an attribution.
        assert_eq!(strip_quoted_reply("On call I said hi"), "On call I said hi");
    }

    #[test]
    fn plain_reply_strips_quoted_trailer() {
        // End-to-end: a plain-text reply carrying ProtonMail's quoted original
        // must reach the timeline as just the new text.
        let email = email_from_json(serde_json::json!({
            "id": "e4",
            "threadId": "t4",
            "textBody": [{ "partId": "1", "type": "text/plain" }],
            "bodyValues": {
                "1": {
                    "value": "09123\n\nOn Tuesday, June 16th, 2026 at 23:23, \
                              Thomas Kelly <thomas@palebluebytes.space> wrote:\n\n> 5678",
                    "isTruncated": false
                }
            }
        }));
        let body = EmailBody::from_email(&email, RenderMode::Links);
        assert_eq!(body.plain, "09123");
        assert!(!body.plain.contains("> 5678") && !body.plain.contains("wrote:"));
    }

    #[test]
    fn format_utc_renders_known_epochs() {
        use super::format_utc;
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        // 2001-09-09 01:46:40 UTC
        assert_eq!(format_utc(1_000_000_000), "2001-09-09 01:46 UTC");
    }

    #[test]
    fn format_reply_quote_structure() {
        use super::format_reply_quote;
        let q = format_reply_quote("Thomas <t@x>", "2026-06-17 00:07 UTC", "a\n\nb");
        assert_eq!(q, "On 2026-06-17 00:07 UTC, Thomas <t@x> wrote:\n> a\n>\n> b");
    }

    #[test]
    fn format_reply_quote_is_reversed_by_strip_quoted_reply() {
        use super::{format_reply_quote, strip_quoted_reply};
        // The matched-pair invariant: a body we quote is fully removed again by
        // the inbound stripper, so the quote never leaks into the timeline.
        let quote = format_reply_quote("Thomas <t@x>", "2026-06-17 00:07 UTC", "old line 1\nold line 2");
        let outbound = format!("my new reply\n\n{quote}");
        assert_eq!(strip_quoted_reply(&outbound), "my new reply");
    }

    #[test]
    fn clean_html_strips_quoted_blockquote() {
        use super::clean_html_for_matrix;
        let dirty = "<p>my reply</p>\
                     <blockquote type=\"cite\"><p>old quoted message</p></blockquote>";
        let clean = clean_html_for_matrix(dirty);
        assert_eq!(clean, "<p>my reply</p>");
    }

    #[test]
    fn render_mode_parses() {
        assert_eq!("plain".parse(), Ok(RenderMode::Plain));
        assert_eq!("links".parse(), Ok(RenderMode::Links));
        assert_eq!("rich".parse(), Ok(RenderMode::Rich));
        assert_eq!("HTML".parse(), Ok(RenderMode::Rich));
        assert_eq!(RenderMode::default(), RenderMode::Links);
        assert!("nope".parse::<RenderMode>().is_err());
    }

    #[test]
    fn lightweight_html_keeps_links_drops_images_and_layout() {
        use super::lightweight_html;
        // A Kit-style button: an <a> inside layout wrappers, plus an image.
        let dirty = "<div class=\"box\"><img src=\"x.png\"/>\
                     <a href=\"https://kit.com/confirm\">Confirm your subscription</a>\
                     <span>extra</span></div>";
        let out = lightweight_html(dirty);
        assert!(
            out.contains("<a href=\"https://kit.com/confirm\">Confirm your subscription</a>"),
            "the link (button) must survive as a clickable link: {out}"
        );
        assert!(!out.contains("<img"), "images must be dropped: {out}");
        assert!(
            !out.contains("<div") && !out.contains("<span"),
            "layout containers must be unwrapped: {out}"
        );
    }

    fn link_email() -> Email {
        // An email with a plain alternative and an HTML alternative carrying a link.
        email_from_json(serde_json::json!({
            "id": "e5",
            "threadId": "t5",
            "textBody": [{ "partId": "1", "type": "text/plain" }],
            "htmlBody": [{ "partId": "2", "type": "text/html" }],
            "bodyValues": {
                "1": { "value": "Confirm your subscription ( https://kit.com/confirm )", "isTruncated": false },
                "2": { "value": "<div><img src=\"x.png\"/><a href=\"https://kit.com/confirm\">Confirm your subscription</a></div>", "isTruncated": false }
            }
        }))
    }

    #[test]
    fn links_mode_emits_clickable_link_without_images() {
        let body = EmailBody::from_email(&link_email(), RenderMode::Links);
        let html = body.html.as_deref().expect("links mode emits a formatted body");
        assert!(html.contains("href=\"https://kit.com/confirm\""), "{html}");
        assert!(!html.contains("<img"), "links mode drops images: {html}");
    }

    #[test]
    fn rich_mode_keeps_full_html_including_images() {
        let body = EmailBody::from_email(&link_email(), RenderMode::Rich);
        let html = body.html.as_deref().expect("rich mode emits a formatted body");
        assert!(html.contains("<img"), "rich mode keeps images: {html}");
    }

    #[test]
    fn plain_only_email_emits_no_html_even_when_htmlbody_aliases_plain() {
        // A plain-text-only ProtonMail reply: JMAP points BOTH textBody and
        // htmlBody at the same text/plain part. The bridge must not treat that
        // as an HTML alternative — otherwise the raw plain text (with its quoted
        // trailer) is emitted as formatted_body and Element shows the un-stripped
        // quote even though the plain body was stripped. (Regression: kelpy
        // event $yov7… showed body="9012" but formatted_body kept "> 5678".)
        let email = email_from_json(serde_json::json!({
            "id": "e6",
            "threadId": "t6",
            "textBody": [{ "partId": "1", "type": "text/plain" }],
            "htmlBody": [{ "partId": "1", "type": "text/plain" }],
            "bodyValues": {
                "1": {
                    "value": "9012\n\n\n\nOn Wednesday, June 17th, 2026 at 00:07, Thomas Kelly  wrote:\n\n> 5678\n>",
                    "isTruncated": false
                }
            }
        }));
        let body = EmailBody::from_email(&email, RenderMode::Links);
        assert_eq!(body.plain, "9012");
        assert!(
            body.html.is_none(),
            "a plain-only email must not emit a formatted body: {:?}",
            body.html
        );
    }

    #[test]
    fn plain_mode_emits_no_formatted_body() {
        let body = EmailBody::from_email(&link_email(), RenderMode::Plain);
        assert!(
            body.html.is_none(),
            "plain mode never emits a formatted body"
        );
        // The timeline text still comes through.
        assert!(body.plain.contains("Confirm your subscription"));
    }
}
