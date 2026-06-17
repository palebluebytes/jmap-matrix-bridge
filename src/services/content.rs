//! Ingestion and bridging of content (bodies and attachments) from JMAP to Matrix.

use crate::matrix::MatrixClient;
use crate::store::{Store, ThreadRepository};
use ammonia::Builder;
use anyhow::{Context, Result};
use jmap_client::client::Client;
use jmap_client::email::{Email, EmailBodyPart};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
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
                sanitize_for_matrix(&html, mode)
            }),
        };

        let (plain, html) = clamp_to_matrix_limit(plain, html);
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

/// Sanitize an HTML email body into a Matrix `formatted_body` that is provably
/// conformant to the Matrix spec's allowed-HTML subset (spec.matrix.org), using
/// `ammonia` as an allowlist sanitizer. Anything outside the allowlist is either
/// unwrapped (its text content kept) or, for chrome/quoted blocks, removed with
/// its content. `RenderMode::Links` additionally drops images and unwraps
/// `<div>`/`<span>` so the content flows as lightweight text + clickable links;
/// `RenderMode::Rich` keeps them.
#[must_use]
fn sanitize_for_matrix(html: &str, mode: RenderMode) -> String {
    let builder = match mode {
        RenderMode::Links => &*LINKS_SANITIZER,
        // Plain never reaches here (from_email emits no formatted body for it).
        RenderMode::Plain | RenderMode::Rich => &*RICH_SANITIZER,
    };
    builder.clean(html).to_string()
}

static RICH_SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| matrix_sanitizer(false));
static LINKS_SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| matrix_sanitizer(true));

/// Build an `ammonia::Builder` configured to the Matrix allowed-HTML subset.
/// Everything is set explicitly (not inherited from ammonia's defaults) so the
/// output is provably a subset of Matrix's allowlist.
///
/// Two deliberate, spec-conformant deviations preserve existing bridge
/// behaviour: table tags are excluded (so newsletter table-layouts linearize —
/// Matrix clients render tables ultra-wide without the email's CSS), and
/// `blockquote` is dropped with its content (quoted-reply originals are already
/// in the room, so they are noise). `links` mode further drops images and
/// unwraps `<div>`/`<span>`.
fn matrix_sanitizer(links_mode: bool) -> Builder<'static> {
    // Matrix v1.18 allowed tags, minus the table tags (linearized by unwrapping)
    // and `blockquote` (in clean_content_tags below). `font` is not a Matrix tag
    // either, so it is unwrapped automatically.
    let mut tags: HashSet<&str> = HashSet::from([
        "del", "h1", "h2", "h3", "h4", "h5", "h6", "p", "a", "ul", "ol", "sup", "sub", "li", "b",
        "i", "u", "strong", "em", "s", "code", "hr", "br", "div", "span", "img", "pre", "details",
        "summary",
    ]);
    if links_mode {
        tags.remove("img"); // void → dropped entirely
        tags.remove("div"); // unwrapped (content kept)
        tags.remove("span");
    }

    let tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::from([
        ("a", HashSet::from(["href", "target"])),
        ("img", HashSet::from(["src", "width", "height", "alt", "title"])),
        ("ol", HashSet::from(["start"])),
        ("code", HashSet::from(["class"])),
    ]);

    let mut b = Builder::default();
    b.tags(tags)
        // Removed WITH their content: chrome (so `<style>` CSS does not leak as
        // text when unwrapped) and quoted-reply blocks.
        .clean_content_tags(HashSet::from([
            "script",
            "style",
            "head",
            "title",
            "blockquote",
        ]))
        .tag_attributes(tag_attributes)
        // No blanket attributes — only the per-tag ones above plus data-mx-*.
        .generic_attributes(HashSet::new())
        .generic_attribute_prefixes(HashSet::from(["data-mx-"]))
        .url_schemes(HashSet::from([
            "https", "http", "ftp", "mailto", "magnet", "mxc",
        ]))
        .url_relative(ammonia::UrlRelative::Deny)
        .link_rel(Some("noopener"))
        // `url_schemes` is global, but the Matrix spec restricts `<img src>` to
        // `mxc://` specifically (http/https are only valid on `<a href>`). Drop a
        // non-mxc img src so the output is a strict subset of the allowlist.
        .attribute_filter(|element, attribute, value| {
            if element == "img" && attribute == "src" && !value.starts_with("mxc://") {
                return None;
            }
            Some(std::borrow::Cow::Borrowed(value))
        });
    b
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

/// Byte budget for a bridged message's `body` + `formatted_body`. Matrix rejects
/// any event whose serialized PDU exceeds 65535 bytes (`M_TOO_LARGE`); the rest
/// of the 64 KB is headroom for the envelope (sender/room/type plus the
/// server-added auth/prev-events, hashes and signatures).
const MATRIX_BODY_BUDGET: usize = 57_000;

const MATRIX_TRUNCATION_NOTICE: &str =
    "\n\n[Message truncated — too large for Matrix; view the full email in your mail client.]";

/// Bound `(plain, html)` so their combined byte length fits [`MATRIX_BODY_BUDGET`],
/// so a large email is delivered TRUNCATED rather than dropped with
/// `M_TOO_LARGE`. HTML cannot be byte-truncated without breaking tags, so an
/// oversized formatted body is dropped entirely (Element falls back to the plain
/// body, which is a faithful `html2text` rendering); the plain body is truncated
/// on a UTF-8 boundary with a notice.
fn clamp_to_matrix_limit(plain: String, html: Option<String>) -> (String, Option<String>) {
    let html = html.filter(|h| h.len() <= MATRIX_BODY_BUDGET);
    let allowance = MATRIX_BODY_BUDGET - html.as_ref().map_or(0, String::len);
    if plain.len() <= allowance {
        return (plain, html);
    }
    let keep = allowance.saturating_sub(MATRIX_TRUNCATION_NOTICE.len());
    let mut out = clamp_utf8(&plain, keep);
    out.push_str(MATRIX_TRUNCATION_NOTICE);
    (out, html)
}

/// Truncate `s` to at most `max_bytes` without splitting a UTF-8 codepoint,
/// preferring the last newline/space within the tail for a cleaner break.
fn clamp_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    // Back up to the last whitespace if it is close to the cut (avoids chopping a
    // word in half), but not if the only whitespace is far back (e.g. a long URL).
    if let Some(ws) = s[..end].rfind([' ', '\n']) {
        if end - ws < 200 {
            end = ws;
        }
    }
    s[..end].trim_end().to_owned()
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
/// attribution line, via `jiff`. Returns an empty string if the epoch is out of
/// representable range (best-effort: the caller still sends the reply).
#[must_use]
pub(crate) fn format_utc(epoch: i64) -> String {
    jiff::Timestamp::from_second(epoch).map_or_else(
        |_| String::new(),
        |ts| {
            ts.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y-%m-%d %H:%M UTC")
                .to_string()
        },
    )
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
        use super::{RenderMode, sanitize_for_matrix};
        let dirty = "<!DOCTYPE html><html><head><style>/* css */</style></head>\
                     <body><!-- hi --><p>Hello <b>world</b></p></body></html>";
        let clean = sanitize_for_matrix(dirty, RenderMode::Rich);
        assert!(clean.contains("<p>Hello") && clean.contains("<b>world</b>"), "{clean}");
        // Chrome and its CONTENT are gone (style is not merely unwrapped).
        assert!(
            !clean.contains("css")
                && !clean.to_lowercase().contains("<style")
                && !clean.to_lowercase().contains("<head")
                && !clean.contains("<!DOCTYPE"),
            "{clean}"
        );
    }

    #[test]
    fn clean_html_linearizes_table_layout() {
        use super::{RenderMode, sanitize_for_matrix};
        // A newsletter table wrapper must be unwrapped so the paragraphs flow as
        // top-level blocks (otherwise Matrix renders them on ultra-wide lines).
        let dirty = "<table style=\"max-width:600px\"><tbody><tr>\
                     <td><p>First para</p><p>Second para</p></td></tr></tbody></table>";
        let clean = sanitize_for_matrix(dirty, RenderMode::Rich);
        assert!(clean.contains("First para") && clean.contains("Second para"), "{clean}");
        assert!(
            !clean.contains("<table") && !clean.contains("<td") && !clean.contains("<tr"),
            "table tags must be unwrapped (linearized): {clean}"
        );
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
    fn clamp_utf8_never_splits_a_codepoint() {
        use super::clamp_utf8;
        let s = "é".repeat(100); // 200 bytes, no whitespace
        let out = clamp_utf8(&s, 51); // 51 lands mid-codepoint -> backs up to 50
        assert!(out.len() <= 51 && out.is_char_boundary(out.len()));
        assert_eq!(out.chars().count(), 25, "should keep whole 'é' chars: {out:?}");
        // Shorter than the limit -> unchanged.
        assert_eq!(clamp_utf8("hi", 100), "hi");
    }

    #[test]
    fn clamp_drops_oversized_html_and_truncates_plain() {
        use super::{MATRIX_BODY_BUDGET, clamp_to_matrix_limit};
        let html = Some(format!("<p>{}</p>", "a".repeat(MATRIX_BODY_BUDGET)));
        let plain = "word ".repeat(MATRIX_BODY_BUDGET); // way over budget
        let (p, h) = clamp_to_matrix_limit(plain, html);
        assert!(h.is_none(), "oversized formatted body must be dropped");
        assert!(p.len() <= MATRIX_BODY_BUDGET, "plain must fit the budget: {}", p.len());
        assert!(p.contains("truncated"), "a truncation notice must be appended");
    }

    #[test]
    fn clamp_keeps_small_html_and_shrinks_plain() {
        use super::{MATRIX_BODY_BUDGET, clamp_to_matrix_limit};
        let html = Some("<p>small</p>".to_owned());
        let plain = "word ".repeat(MATRIX_BODY_BUDGET);
        let (p, h) = clamp_to_matrix_limit(plain, html.clone());
        assert_eq!(h, html, "a fitting formatted body must be kept");
        assert!(
            p.len() + h.unwrap().len() <= MATRIX_BODY_BUDGET,
            "combined body must fit the budget"
        );
        assert!(p.contains("truncated"));
    }

    #[test]
    fn clamp_leaves_small_message_unchanged() {
        use super::clamp_to_matrix_limit;
        let (p, h) = clamp_to_matrix_limit("hello".to_owned(), Some("<p>hello</p>".to_owned()));
        assert_eq!(p, "hello");
        assert_eq!(h.as_deref(), Some("<p>hello</p>"));
    }

    #[test]
    fn from_email_clamps_a_giant_body() {
        use super::MATRIX_BODY_BUDGET;
        let email = email_from_json(serde_json::json!({
            "id": "big1",
            "threadId": "tbig",
            "textBody": [{ "partId": "1", "type": "text/plain" }],
            "bodyValues": { "1": { "value": "word ".repeat(MATRIX_BODY_BUDGET), "isTruncated": false } }
        }));
        let body = EmailBody::from_email(&email, RenderMode::Links);
        let total = body.plain.len() + body.html.as_deref().map_or(0, str::len);
        assert!(total <= MATRIX_BODY_BUDGET, "bridged event body must fit Matrix's limit: {total}");
        assert!(body.plain.contains("truncated"));
    }

    #[test]
    fn clean_html_strips_quoted_blockquote() {
        use super::{RenderMode, sanitize_for_matrix};
        let dirty = "<p>my reply</p>\
                     <blockquote type=\"cite\"><p>old quoted message</p></blockquote>";
        let clean = sanitize_for_matrix(dirty, RenderMode::Rich);
        assert!(clean.contains("my reply"), "{clean}");
        // The quoted original is removed WITH its content, not just unwrapped.
        assert!(
            !clean.contains("old quoted message") && !clean.contains("<blockquote"),
            "quoted blockquote must be dropped entirely: {clean}"
        );
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
        use super::{RenderMode, sanitize_for_matrix};
        // A Kit-style button: an <a> inside layout wrappers, plus an image.
        let dirty = "<div class=\"box\"><img src=\"https://kit.com/x.png\"/>\
                     <a href=\"https://kit.com/confirm\">Confirm your subscription</a>\
                     <span>extra</span></div>";
        let out = sanitize_for_matrix(dirty, RenderMode::Links);
        // The link survives as a clickable link (ammonia may add rel/reorder
        // attributes, so assert on the href + text rather than the exact anchor).
        assert!(
            out.contains("href=\"https://kit.com/confirm\"")
                && out.contains("Confirm your subscription"),
            "the link (button) must survive as a clickable link: {out}"
        );
        assert!(!out.contains("<img"), "images must be dropped: {out}");
        assert!(
            !out.contains("<div") && !out.contains("<span"),
            "layout containers must be unwrapped: {out}"
        );
    }

    #[test]
    fn sanitize_output_is_matrix_allowlist_conformant() {
        use super::{RenderMode, sanitize_for_matrix};
        // Adversarial input: scripts, inline handlers, a javascript: URL, a
        // disallowed tag, a relative + non-mxc image, and a legit data-mx-color.
        let dirty = "<script>alert(1)</script>\
                     <style>secretcss</style>\
                     <p onclick=\"steal()\">hi <a href=\"javascript:alert(1)\">x</a></p>\
                     <marquee>noped</marquee>\
                     <img src=\"http://evil/x.png\">\
                     <span data-mx-color=\"#ff0000\">red</span>";
        for mode in [RenderMode::Rich, RenderMode::Links] {
            let out = sanitize_for_matrix(dirty, mode);
            let lower = out.to_lowercase();
            assert!(!lower.contains("<script"), "{mode:?}: script tag survived: {out}");
            assert!(!lower.contains("alert(1)"), "{mode:?}: script content survived: {out}");
            assert!(!lower.contains("<style") && !lower.contains("secretcss"), "{mode:?}: style survived: {out}");
            assert!(!lower.contains("onclick"), "{mode:?}: inline handler survived: {out}");
            assert!(!lower.contains("javascript:"), "{mode:?}: javascript: scheme survived: {out}");
            assert!(!lower.contains("<marquee"), "{mode:?}: disallowed tag survived: {out}");
            // Non-mxc/non-allowed-scheme img src must be dropped (url_relative/schemes).
            assert!(!out.contains("http://evil"), "{mode:?}: bad img src survived: {out}");
            // The text content of unwrapped tags is preserved.
            assert!(out.contains("hi") && out.contains("red"), "{mode:?}: text lost: {out}");
        }
        // data-mx-color is a Matrix attribute and must be preserved (Rich keeps span).
        let rich = sanitize_for_matrix(dirty, RenderMode::Rich);
        assert!(rich.contains("data-mx-color"), "data-mx-* must be preserved: {rich}");
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
