//! Ingestion and bridging of content (bodies and attachments) from JMAP to Matrix.

use crate::matrix::MatrixClient;
use crate::store::{Store, ThreadRepository};
use ammonia::Builder;
use anyhow::{Context, Result};
use jmap_client::client::Client;
use jmap_client::email::{Email, EmailBodyPart};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use tracing::warn;

const NO_SUBJECT: &str = "(No Subject)";

/// Path/query-unsafe ASCII for percent-encoding values substituted into the JMAP
/// download URL template (RFC 8620 `{accountId}`/`{blobId}`/`{name}`). `name` is
/// the attacker-controlled attachment filename, so encoding stops it from
/// injecting extra path segments / query (`/`, `?`, `#`, `%`, …). Unreserved
/// chars stay readable.
const URL_TEMPLATE_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

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

    fn extract_body(
        email: &Email,
        parts: &[EmailBodyPart],
    ) -> Option<(String, bool, Option<String>)> {
        let part = parts.first()?;
        let part_id = part.part_id()?;
        let content_type = part.content_type().map(str::to_owned);
        email
            .body_value(part_id)
            .map(|v| (v.value().to_owned(), v.is_truncated(), content_type))
    }
}

/// The email's best HTML representation (htmlBody, before any Matrix
/// sanitization), for callers that need the raw image URLs — e.g. on-demand
/// image loading. `None` for a plain-text-only email.
#[must_use]
pub(crate) fn original_html(email: &Email) -> Option<String> {
    EmailBody::html_candidate(email).map(|(html, _)| html)
}

/// Heuristic: does this text contain real HTML markup (a recognizable tag)?
/// Deliberately conservative so plain prose with a stray `<` or `<3` is not
/// misclassified.
#[must_use]
fn looks_like_html(s: &str) -> bool {
    const TAGS: &[&str] = &[
        "<div",
        "<p>",
        "<p ",
        "<br",
        "<a ",
        "<a>",
        "<table",
        "<td",
        "<tr",
        "<span",
        "<img",
        "<ul",
        "<ol",
        "<li",
        "<blockquote",
        "<figure",
        "<h1",
        "<h2",
        "<h3",
        "<h4",
        "<strong",
        "<em>",
        "<b>",
        "<i>",
        "<html",
        "<body",
        "<head",
        "<style",
        "<font",
        "<center",
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

/// Replace table tags with lightweight separators *before* the sanitizer runs.
/// Matrix's allowed-HTML has no table tags, so ammonia drops `<table>`/`<tr>`/`<td>`
/// keeping only their text — which glues adjacent cells (a `[logo][Feefo][on
/// behalf of]` layout row becomes "Feefoon behalf of") and runs a row straight
/// into the next block. We can't just *insert* a `<br>` between `</tr>` and `<tr>`:
/// the HTML5 parser foster-parents a `<br>` that isn't valid table content out of
/// the table, so it's lost. Instead we strip the table tags ourselves here —
/// dropping the open tags and turning each cell close into a space and each
/// row/table/section close into a `<br>` — so ammonia never sees a table and the
/// separators survive as ordinary inline content. `collapse_breaks` /
/// `collapse_blank_runs` fold any excess a deeply nested layout produces.
#[must_use]
fn separate_table_blocks(html: &str, mode: RenderMode) -> String {
    // Open tags dropped outright (case-insensitive, boundary-checked).
    const OPEN: &[&str] = &[
        "<table",
        "<tbody",
        "<thead",
        "<tfoot",
        "<tr",
        "<td",
        "<th",
        "<caption",
        "<col",
        "<colgroup",
    ];
    // Links mode unwraps <div> too (a block container), so its boundaries also
    // vanish — running one section into the next. Break on it here for the same
    // reason as a table row. Rich keeps <div> (it renders its own break), so leave
    // it alone there.
    let break_divs = matches!(mode, RenderMode::Links);
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() + html.len() / 16);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let rest = &html[i..];
        let end = rest.find('>').map_or(bytes.len(), |g| i + g + 1);
        // `</td`/`</th` are matched before the section wrappers so `</thead`
        // (a break) is not mistaken for `</th` (a space).
        if starts_tag(rest, "</td") || starts_tag(rest, "</th") {
            out.push(' '); // keep cell text apart
        } else if starts_tag(rest, "</tr")
            || starts_tag(rest, "</table")
            || starts_tag(rest, "</tbody")
            || starts_tag(rest, "</thead")
            || starts_tag(rest, "</tfoot")
            || starts_tag(rest, "</caption")
            || (break_divs && starts_tag(rest, "</div"))
        {
            out.push_str("<br>"); // break rows / whole tables / (links) div sections
        } else if break_divs && starts_tag(rest, "<div") {
            // drop the div open (links unwraps it anyway); its </div> broke above
        } else if !OPEN.iter().any(|t| starts_tag(rest, t)) {
            out.push_str(&html[i..end]); // not a table/handled tag — keep verbatim
        }
        // (table open tags fall through: dropped, nothing emitted)
        i = end;
    }
    out
}

/// Block-level elements that render with their own vertical spacing, so a `<br>`
/// directly adjacent to one only doubles the gap. `div` is deliberately excluded
/// (it has no default margin, so its `<br>` is load-bearing separation).
const BLOCK_TAGS: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "hr",
    "details",
    "summary",
];

/// True if the text already emitted ends with a block-level open or close tag.
fn ends_with_block(out: &str) -> bool {
    let t = out.trim_end();
    let Some(open) = t.strip_suffix('>').and_then(|_| t.rfind('<')) else {
        return false;
    };
    let name = t[open + 1..t.len() - 1]
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    BLOCK_TAGS.contains(&name.as_str())
}

/// True if `rest` begins (after leading whitespace) with a block open/close tag.
fn starts_with_block(rest: &str) -> bool {
    let r = rest.trim_start();
    BLOCK_TAGS
        .iter()
        .any(|b| starts_tag(r, &format!("<{b}")) || starts_tag(r, &format!("</{b}")))
}

/// End index of a run of one-or-more `<br>` starting at `i` (whitespace allowed
/// between them), or `None` if there is no `<br>` at `i`.
fn br_run_end(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut j = i;
    let mut found = false;
    loop {
        let mut k = j;
        while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r') {
            k += 1;
        }
        if s[k..].starts_with("<br>") {
            j = k + "<br>".len();
            found = true;
        } else {
            break;
        }
    }
    found.then_some(j)
}

/// Drop `<br>` runs sitting right after a block close/open or right before a
/// block open/close — the block already breaks the line, so the `<br>` only adds
/// an empty line. A newsletter that wraps paragraphs in `<p>`/`<h1>` AND pads them
/// with `<br>` otherwise renders as tall double gaps. Bare-text breaks (no
/// adjacent block) are kept, so genuine line breaks survive.
#[must_use]
fn strip_block_adjacent_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(run_end) = br_run_end(s, i) {
            // Redundant beside a block — drop; otherwise it's a bare-text break — keep.
            if !(ends_with_block(&out) || starts_with_block(&s[run_end..])) {
                out.push_str(&s[i..run_end]);
            }
            i = run_end;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
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
    // Drop quoted-REPLY blockquotes (the prior message a client wraps when
    // replying) BEFORE sanitizing, but keep editorial blockquotes (pull-quotes):
    // ammonia's clean_content_tags is all-or-nothing per tag, so the
    // reply-vs-editorial distinction is made here, by marker.
    let pre = strip_reply_blockquotes(html);
    // In links mode images are dropped, leaving no sign an email had any. Mark
    // each loadable image's spot with 🖼️ so the reader sees where images are and
    // knows to react 🖼️ to load them. (Rich keeps the real images.)
    let pre = if matches!(mode, RenderMode::Links) {
        placeholder_remote_images(&pre)
    } else {
        pre
    };
    // Restore cell/row breaks before ammonia unwraps the (disallowed) table tags,
    // so a layout-table row doesn't collapse into one glued line.
    let pre = separate_table_blocks(&pre, mode);
    let builder = match mode {
        RenderMode::Links => &*LINKS_SANITIZER,
        // Plain never reaches here (from_email emits no formatted body for it).
        RenderMode::Plain | RenderMode::Rich => &*RICH_SANITIZER,
    };
    let cleaned = builder.clean(&pre).to_string();
    // Drop invisible "preheader spacer" code points (combining grapheme joiner,
    // soft hyphen, zero-widths) newsletters pad with — they're invisible but
    // count as text, so they render as blank lines and keep wrapper elements
    // from looking empty to the pruner below.
    let cleaned = strip_invisibles(&cleaned);
    // Unwrap `<p>` inside `<li>` (purely-presentational in newsletters) so the
    // list marker and item text render inline instead of on separate lines.
    let cleaned = unwrap_li_paragraphs(&cleaned);
    // Drop links left holding only a `<br>` (a logo link whose image was dropped)
    // before the general pruner, so the now-empty parent can collapse too.
    let cleaned = prune_br_only_links(&cleaned);
    // Remove wrapper elements left empty once images/spacers are gone (e.g. a
    // logo `<h1><a><img></a></h1>` becomes an empty heading in Links mode) so
    // they don't render as tall blank gaps.
    let cleaned = prune_empty_elements(&cleaned);
    // Fold the whitespace/`&nbsp;` filler ladders newsletters leave between
    // blocks down to a single space (a lone `&nbsp;`, e.g. around a button
    // label, is left intact).
    let cleaned = collapse_blank_runs(&cleaned);
    // Drop <br> that only doubles a block element's own margin (a <p>/<h1>
    // padded with <br> renders as a tall gap otherwise).
    let cleaned = strip_block_adjacent_breaks(&cleaned);
    // Collapse the `<br>` ladders ProtonMail-style composers leave (empty
    // paragraphs) so the body isn't padded with blank lines.
    collapse_breaks(&cleaned)
}

/// Length of one "blank unit" at the start of `s`: a single ASCII-whitespace
/// byte, a U+00A0 char, or one `&nbsp;`/`&#160;`/`&#xa0;` entity. `None` if `s`
/// doesn't start with blank content.
fn blank_unit_len(s: &str) -> Option<usize> {
    // Any Unicode whitespace (ASCII spaces/newlines, U+00A0 no-break, U+2007
    // figure space and the other Zs spaces newsletters pad with) is one unit.
    if let Some(c) = s.chars().next().filter(|c| c.is_whitespace()) {
        return Some(c.len_utf8());
    }
    ["&nbsp;", "&#160;", "&#xA0;", "&#xa0;"]
        .into_iter()
        .find(|ent| s.starts_with(ent))
        .map(str::len)
}

/// Collapse runs of 2+ consecutive blank units (whitespace and `&nbsp;`) to a
/// single space, leaving a lone blank unit verbatim. Kills the filler ladders
/// newsletters pad with while preserving intentional single `&nbsp;` spacing.
#[must_use]
fn collapse_blank_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(first) = blank_unit_len(&s[i..]) {
            let mut j = i + first;
            let mut count = 1;
            while let Some(n) = blank_unit_len(&s[j..]) {
                j += n;
                count += 1;
            }
            if count >= 2 {
                out.push(' ');
            } else {
                out.push_str(&s[i..j]);
            }
            i = j;
        } else {
            let ch = s[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Invisible/zero-width code points newsletters use as "preheader spacers" to
/// pad the inbox preview. They show nothing but count as text, so a line of them
/// renders as a blank line and a wrapper full of them looks non-empty. Stripped
/// from both bodies. Directional marks (U+200E/200F) are deliberately NOT here —
/// removing those can corrupt legitimate RTL text.
const INVISIBLE_CHARS: &[char] = &[
    '\u{00AD}', // soft hyphen
    '\u{034F}', // combining grapheme joiner
    '\u{200B}', // zero width space
    '\u{200C}', // zero width non-joiner
    '\u{200D}', // zero width joiner
    '\u{2060}', // word joiner
    '\u{FEFF}', // zero width no-break space (BOM)
    '\u{180E}', // mongolian vowel separator
];

/// Remove the invisible padding code points above. Cheap no-op when none present.
#[must_use]
fn strip_invisibles(s: &str) -> String {
    if s.contains(INVISIBLE_CHARS) {
        s.chars().filter(|c| !INVISIBLE_CHARS.contains(c)).collect()
    } else {
        s.to_owned()
    }
}

/// Wrapper elements that are pure chrome when they hold no visible content, so
/// they're safe to drop entirely if empty. Excludes void/meaningful-empty tags
/// (`br`, `hr`, `img`).
const PRUNABLE_WHEN_EMPTY: &[&str] = &[
    "a",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "p",
    "span",
    "div",
    "b",
    "i",
    "u",
    "strong",
    "em",
    "s",
    "del",
    "sub",
    "sup",
    "code",
    "blockquote",
    "ul",
    "ol",
    "li",
    "pre",
    "details",
    "summary",
    "caption",
];

/// Drop wrapper elements whose content is "blank" (only whitespace and `&nbsp;`),
/// iterating to a fixed point so nested empties collapse outward — an empty
/// `<a>` inside an `<h1>` leaves the `<h1>` blank, which the next pass removes.
#[must_use]
fn prune_empty_elements(html: &str) -> String {
    let mut cur = html.to_owned();
    for _ in 0..32 {
        let (next, changed) = prune_empty_once(&cur);
        if !changed {
            return next;
        }
        cur = next;
    }
    cur
}

/// One left-to-right pass removing every empty prunable element it finds.
/// Returns the rewritten string and whether anything was removed.
fn prune_empty_once(html: &str) -> (String, bool) {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    let mut changed = false;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let rest = &html[i..];
        let Some(gt_rel) = rest.find('>') else {
            out.push_str(rest);
            break;
        };
        let open_end = i + gt_rel + 1;
        if let Some(name) = prunable_open_name(rest) {
            let after_blank = skip_blank_content(html, open_end);
            let tail = &html[after_blank..];
            let close = format!("</{name}");
            let matches_close = tail.len() >= close.len()
                && tail.as_bytes()[..close.len()].eq_ignore_ascii_case(close.as_bytes())
                && matches!(
                    tail.as_bytes().get(close.len()),
                    Some(b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r')
                );
            if matches_close {
                if let Some(cgt) = tail.find('>') {
                    i = after_blank + cgt + 1; // skip the whole empty element
                    changed = true;
                    continue;
                }
            }
        }
        // Not an empty prunable: emit just this tag and keep scanning its content
        // (inner empties are removed in the same pass; the outer, if it becomes
        // empty as a result, is caught on the next iteration).
        out.push_str(&html[i..open_end]);
        i = open_end;
    }
    (out, changed)
}

/// If `rest` begins with an open tag `<name …>` of a prunable element (not a
/// close tag), return that name. Boundary-checked so `<span` matches but
/// `<summary` isn't mistaken for `<sub`/`<s`, etc.
fn prunable_open_name(rest: &str) -> Option<&'static str> {
    if rest.starts_with("</") {
        return None;
    }
    PRUNABLE_WHEN_EMPTY
        .iter()
        .copied()
        .find(|n| starts_tag(rest, &format!("<{n}")))
}

/// Advance past "blank" content from byte `k`: ASCII/Unicode whitespace, the
/// non-breaking space char (U+00A0), and `&nbsp;`/`&#160;`/`&#xa0;` entities.
/// `<br>` is deliberately NOT blank here — a `<p><br></p>` is an intentional blank
/// line; only links get their lone `<br>` treated as empty (see
/// [`prune_br_only_links`]).
fn skip_blank_content(html: &str, mut k: usize) -> usize {
    loop {
        let start = k;
        while let Some(c) = html[k..].chars().next().filter(|c| c.is_whitespace()) {
            k += c.len_utf8();
        }
        for ent in ["&nbsp;", "&#160;", "&#xA0;", "&#xa0;"] {
            if html[k..].starts_with(ent) {
                k += ent.len();
            }
        }
        if k == start {
            return k;
        }
    }
}

/// Drop an `<a>` whose only content is whitespace, `&nbsp;`, or `<br>` — e.g. a
/// logo link left holding just a `<br>` once its image was dropped. Unlike
/// [`prune_empty_elements`] (which keeps `<p><br></p>`, an intentional blank line),
/// a lone break inside a *link* is never meaningful, so the whole empty `<a>` goes.
#[must_use]
fn prune_br_only_links(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &html[i..];
        if starts_tag(rest, "<a")
            && let Some(gt) = rest.find('>')
        {
            // Skip whitespace/&nbsp;/<br> after the open tag; if the next thing is
            // the matching close, the link is empty — drop it whole.
            let open_end = i + gt + 1;
            let mut k = skip_blank_content(html, open_end);
            while html[k..].starts_with("<br>") {
                k = skip_blank_content(html, k + "<br>".len());
            }
            if let Some(tail) = html.get(k..)
                && starts_tag(tail, "</a")
                && let Some(cgt) = tail.find('>')
            {
                i = k + cgt + 1;
                continue;
            }
        }
        let ch = html[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// True if `rest` begins with the tag name `name` (which includes the leading
/// `<`, e.g. `"<p"` or `"</li"`) as a *complete* tag name — i.e. the next byte
/// is a tag-name boundary (`>`, `/`, whitespace, or end). ASCII-case-insensitive.
/// Stops `<p` from matching `<pre>` and `<li` from matching `<link>`.
fn starts_tag(rest: &str, name: &str) -> bool {
    rest.len() >= name.len()
        && rest.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes())
        && matches!(
            rest.as_bytes().get(name.len()),
            None | Some(b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r')
        )
}

/// Strip `<p>`/`</p>` tags that appear inside an `<li>` (at any nesting depth),
/// leaving their text content in place. Element renders a block `<p>` on its own
/// line, so a list item whose text is wrapped in a `<p>` (a common newsletter
/// pattern) drops the text below its "1."/bullet marker. Removing the wrapper
/// makes the marker and text render inline. `<p>` outside lists — real paragraph
/// breaks — is untouched. Runs after ammonia, on normalized, balanced tags.
#[must_use]
fn unwrap_li_paragraphs(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    let mut li_depth = 0u32;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let rest = &html[i..];
            let tag_end = rest.find('>').map_or(bytes.len(), |g| i + g + 1);
            if starts_tag(rest, "<li") {
                li_depth += 1;
            } else if starts_tag(rest, "</li") {
                li_depth = li_depth.saturating_sub(1);
            } else if li_depth > 0 && (starts_tag(rest, "<p") || starts_tag(rest, "</p")) {
                // Drop the whole tag, keep scanning (its text content stays).
                i = tag_end;
                continue;
            }
            out.push_str(&html[i..tag_end]);
            i = tag_end;
        } else {
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Remove quoted-REPLY blockquotes (the client's wrapper around the prior
/// message) while KEEPING editorial blockquotes (newsletter pull-quotes etc.).
/// Reply quotes carry a marker the editorial ones don't: Apple Mail/Thunderbird
/// `type="cite"`, or a `*_quote` class (Gmail `gmail_quote`, Proton Mail
/// `protonmail_quote`, Yahoo `yahoo_quoted`). Nesting-aware, so a reply that
/// itself quotes an earlier reply is removed whole.
#[must_use]
fn strip_reply_blockquotes(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < bytes.len() {
        let Some(open) = find_ci(bytes, b"<blockquote", i) else {
            out.push_str(&html[i..]);
            break;
        };
        let Some(gt) = html[open..].find('>') else {
            out.push_str(&html[i..]);
            break;
        };
        let tag_end = open + gt + 1;
        let open_tag = html[open..tag_end].to_ascii_lowercase();
        let is_reply = open_tag.contains("type=\"cite\"")
            || open_tag.contains("type='cite'")
            || open_tag.contains("type=cite")
            || open_tag.contains("_quote");
        if !is_reply {
            // Editorial blockquote: keep its open tag and keep scanning inside
            // for any nested reply quotes.
            out.push_str(&html[i..tag_end]);
            i = tag_end;
            continue;
        }
        // Reply blockquote: emit everything before it, then skip the whole region
        // (matching close, nesting-aware).
        out.push_str(&html[i..open]);
        let mut depth = 1u32;
        let mut j = tag_end;
        while j < bytes.len() && depth > 0 {
            let next_open = find_ci(bytes, b"<blockquote", j);
            let next_close = find_ci(bytes, b"</blockquote", j);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    j = o + b"<blockquote".len();
                }
                (_, Some(c)) => {
                    depth -= 1;
                    j = html[c..].find('>').map_or(bytes.len(), |g| c + g + 1);
                }
                (Some(o), None) => {
                    depth += 1;
                    j = o + b"<blockquote".len();
                }
                (None, None) => j = bytes.len(),
            }
        }
        i = j;
    }
    out
}

/// Collapse runs of `<br>` (ammonia normalizes `<br/>`/`<br />` to `<br>`) to at
/// most two, ignoring whitespace between them, and drop a leading/trailing run —
/// so the empty paragraphs mail composers leave (and the break a leading layout
/// table now emits) don't render as a ladder of blank lines.
#[must_use]
fn collapse_breaks(s: &str) -> String {
    const BR: &str = "<br>";
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut run = 0u32;
    while i < s.len() {
        // Optional whitespace followed by a <br> counts as a break unit.
        let mut j = i;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
            j += 1;
        }
        if s[j..].starts_with(BR) {
            run += 1;
            if run <= 2 {
                out.push_str(BR);
            }
            i = j + BR.len();
        } else {
            run = 0;
            let ch = s[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    while out.ends_with(BR) {
        out.truncate(out.len() - BR.len());
    }
    let mut out = out.trim();
    while out.starts_with(BR) {
        out = out[BR.len()..].trim_start();
    }
    out.to_owned()
}

/// A remote `<img>` referenced by an email body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteImg {
    /// The `src` value exactly as it appears in the HTML (entities intact), used
    /// both as the rewrite key and — once entity-decoded — as the fetch URL.
    pub url: String,
    /// Decorative/chrome by its width/height: a 1×1 (or 0/2) tracking pixel or
    /// thin spacer, or a small square icon (social/footer icons etc.). Callers
    /// skip these so opting in doesn't load beacons or clutter with tiny icons.
    pub is_decorative: bool,
}

/// Parse an `<img>` numeric dimension attribute (`width`/`height`) to pixels.
fn img_dimension(tag: &str, name: &str) -> Option<u32> {
    tag_attr(tag, name).and_then(|(v, _)| v.trim().parse::<u32>().ok())
}

/// Substrings that identify an open-tracking pixel / analytics beacon by its
/// `src` URL. These images are invisible in a real client and never content, so
/// they get no marker and are never fetched — cleaner AND a privacy win (loading
/// one would ping the tracker). Conservative and case-insensitive: each fragment
/// is specific to a tracking endpoint, not a bare word that could match content.
/// Incomplete by nature — a new platform's tracker slips through until added.
const TRACKER_URL_MARKERS: &[&str] = &[
    "servlet.imageserver",   // Salesforce Marketing Cloud
    "/track/open",           // Mandrill and many ESPs
    "/wf/open",              // SendGrid / Twilio
    "list-manage.com/track", // Mailchimp
    "/open.aspx",
    "/open.php",
    "google-analytics.com", // GA measurement beacon
    "googleadservices.com",
    "doubleclick.net",
    "emltrk.com",  // generic email tracker
    "mailstat.us", // Mailgun opens
    "/pixel.gif",
    "/pixel.png",
    "utm.gif",
];

/// True if `tag`'s `src` looks like a tracking pixel (see [`TRACKER_URL_MARKERS`]).
fn is_tracking_pixel(tag: &str) -> bool {
    tag_attr(tag, "src").is_some_and(|(src, _)| {
        let lower = src.to_ascii_lowercase();
        TRACKER_URL_MARKERS.iter().any(|m| lower.contains(m))
    })
}

/// Decorative chrome (not worth a marker or a fetch): a tracking pixel (by URL,
/// [`is_tracking_pixel`]), a tracker/spacer (a dimension ≤ 2px), or a small square
/// icon (both dimensions present and ≤ 48px, e.g. social/footer icons). Images
/// without numeric dimensions are otherwise treated as content (sized in CSS), so
/// anything not matching stays — except a known tracker, which is dropped even
/// when it declares no dimensions (as Salesforce's `ImageServer` beacon does).
fn is_decorative_img(tag: &str) -> bool {
    let w = img_dimension(tag, "width");
    let h = img_dimension(tag, "height");
    let spacer = w.is_some_and(|v| v <= 2) || h.is_some_and(|v| v <= 2);
    // A tiny icon: both dims small, OR one small dim with the other left to CSS
    // (`height:auto`) — a `width="20"` social icon is still an icon regardless of
    // its implied height. Content images declare a larger dimension.
    let tiny_icon = matches!((w, h), (Some(a), Some(b)) if a <= 48 && b <= 48)
        || (w.is_some_and(|v| v <= 48) && h.is_none())
        || (h.is_some_and(|v| v <= 48) && w.is_none());
    spacer || tiny_icon || is_tracking_pixel(tag)
}

/// Decode the handful of HTML entities that appear inside an `src` URL (chiefly
/// `&amp;`) so the value can be fetched over HTTP.
#[must_use]
pub(crate) fn decode_src_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&#x26;", "&")
}

/// Find attribute `name`'s value within a single tag string (e.g. `<img …>`),
/// returning `(value, range-of-value-in-`tag`)`. Handles double/single/unquoted
/// values and requires a tag-name boundary before `name` so `src` doesn't match
/// `data-src`/`srcset`. ASCII-case-insensitive on the attribute name.
fn tag_attr<'a>(tag: &'a str, name: &str) -> Option<(&'a str, std::ops::Range<usize>)> {
    let bytes = tag.as_bytes();
    let mut search = 0;
    while let Some(rel) = find_ci(&bytes[search..], name.as_bytes(), 0) {
        let pos = search + rel;
        let before_ok = pos == 0
            || matches!(
                bytes[pos - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'/'
            );
        let mut k = pos + name.len();
        while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
            k += 1;
        }
        if before_ok && k < bytes.len() && bytes[k] == b'=' {
            k += 1;
            while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r') {
                k += 1;
            }
            let (start, quote) = match bytes.get(k) {
                Some(b'"') => (k + 1, Some('"')),
                Some(b'\'') => (k + 1, Some('\'')),
                _ => (k, None),
            };
            let end = quote.map_or_else(
                || {
                    start
                        + tag[start..]
                            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                            .unwrap_or(tag.len() - start)
                },
                |q| start + tag[start..].find(q).unwrap_or(tag.len() - start),
            );
            return Some((&tag[start..end], start..end));
        }
        search = pos + name.len();
    }
    None
}

/// True if `html` contains any visible text (not just tags and blank units).
/// Gates the "leading logo" chrome rule so an *image-only* email keeps its images
/// instead of having them all dropped as mastheads.
fn has_visible_text(html: &str) -> bool {
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            i = html[i..].find('>').map_or(bytes.len(), |g| i + g + 1);
        } else if let Some(n) = blank_unit_len(&html[i..]) {
            i += n;
        } else {
            return true;
        }
    }
    false
}

/// Mark a completed chain of consecutive single-image links as chrome once it is
/// at least two long (an icon strip), then reset it. A lone linked image (chain
/// of one) is left as content — a clickable 🖼️.
fn flush_link_chain(chain: &mut Vec<usize>, chrome: &mut HashSet<usize>) {
    if chain.len() >= 2 {
        chrome.extend(chain.iter().copied());
    }
    chain.clear();
}

/// Byte offsets (the `<` of each `<img`) that are structural *chrome* rather than
/// content — a masthead/leading logo, a list-item bullet icon, or a social/app
/// badge strip — so they should neither earn a 🖼️ load-marker nor be fetched on
/// demand.
///
/// [`is_decorative_img`] only sees an image's numeric `width`/`height`; newsletter
/// chrome is routinely CSS-sized with no dimensions, so it needs these structural
/// signals instead:
///   * a leading `<img>` before any visible text — a masthead logo (only when the
///     email HAS text, so an image-only email keeps its images);
///   * an `<img>` that is the first visible node inside an `<li>` — a bullet icon;
///   * an icon *strip*: two or more images in one text-free `<a>`, or a run of two
///     or more consecutive single-image text-free `<a>`s (separated only by
///     whitespace/`<br>`). A *lone* linked image is kept as a clickable marker.
///
/// Trade-off: a rare *content* image in one of those spots (a hero photo above the
/// fold, a product thumbnail leading a list item) is dropped too. In Links mode
/// images are hidden anyway, so the only loss is its load-marker; Rich mode / the
/// original email still shows it. Removing chrome is the better default.
fn chrome_image_offsets(html: &str) -> HashSet<usize> {
    let doc_has_text = has_visible_text(html);
    let bytes = html.as_bytes();
    let mut chrome = HashSet::new();
    let mut seen_text = false; // any visible text emitted so far
    let mut li_leading = false; // inside an <li>, before its first visible node
    let mut a_open = false; // currently inside an <a>…</a>
    let mut a_text = false; // …and it has held visible text
    let mut a_imgs: Vec<usize> = Vec::new(); // <img> offsets in the current <a>
    let mut chain: Vec<usize> = Vec::new(); // run of single-image text-free links
    let mut chain_live = false; // only whitespace/<br> since the last link closed
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            if let Some(n) = blank_unit_len(&html[i..]) {
                i += n; // whitespace / &nbsp; is transparent
            } else {
                flush_link_chain(&mut chain, &mut chrome); // visible text breaks a strip
                chain_live = false;
                seen_text = true;
                li_leading = false;
                if a_open {
                    a_text = true;
                }
                let ch = html[i..].chars().next().unwrap_or(' ');
                i += ch.len_utf8();
            }
            continue;
        }
        let rest = &html[i..];
        let end = rest.find('>').map_or(bytes.len(), |g| i + g + 1);
        if starts_tag(rest, "<img") {
            if (doc_has_text && !seen_text) || li_leading {
                chrome.insert(i); // masthead logo or list bullet
            }
            li_leading = false;
            if a_open {
                a_imgs.push(i);
            } else {
                flush_link_chain(&mut chain, &mut chrome); // a bare image breaks a strip
                chain_live = false;
            }
        } else if starts_tag(rest, "</a") {
            if a_open {
                if !a_text && a_imgs.len() >= 2 {
                    chrome.extend(a_imgs.iter().copied()); // strip within one link
                    flush_link_chain(&mut chain, &mut chrome);
                    chain_live = false;
                } else if !a_text && a_imgs.len() == 1 {
                    if !chain_live {
                        flush_link_chain(&mut chain, &mut chrome);
                    }
                    chain.push(a_imgs[0]); // single-image link — extend the strip run
                    chain_live = true;
                } else {
                    flush_link_chain(&mut chain, &mut chrome); // link had text — breaks it
                    chain_live = false;
                }
                a_open = false;
                a_imgs.clear();
            }
        } else if starts_tag(rest, "<a") {
            a_open = true;
            a_text = false;
            a_imgs.clear();
        } else if !starts_tag(rest, "<br") {
            // any tag other than <br> breaks a strip run; <li> also arms the bullet rule
            flush_link_chain(&mut chain, &mut chrome);
            chain_live = false;
            if starts_tag(rest, "<li") {
                li_leading = true;
            }
        }
        i = end;
    }
    flush_link_chain(&mut chain, &mut chrome);
    chrome
}

/// Collect remote (`http`/`https`) `<img>` sources from an email body, flagging
/// decorative chrome (trackers/spacers, tiny icons, and structural chrome — see
/// [`is_decorative_img`] and [`chrome_image_offsets`]). De-duplicated by `src`,
/// preserving first-seen order.
#[must_use]
pub(crate) fn extract_remote_images(html: &str) -> Vec<RemoteImg> {
    let bytes = html.as_bytes();
    let chrome = chrome_image_offsets(html);
    let mut out: Vec<RemoteImg> = Vec::new();
    let mut i = 0;
    while let Some(start) = find_ci(&bytes[i..], b"<img", 0).map(|r| i + r) {
        let end = html[start..]
            .find('>')
            .map_or(html.len(), |g| start + g + 1);
        let tag = &html[start..end];
        if let Some((src, _)) = tag_attr(tag, "src") {
            let lower = src.trim().to_ascii_lowercase();
            if (lower.starts_with("http://") || lower.starts_with("https://"))
                && !out.iter().any(|r| r.url == src)
            {
                out.push(RemoteImg {
                    url: src.to_owned(),
                    is_decorative: is_decorative_img(tag) || chrome.contains(&start),
                });
            }
        }
        i = end;
    }
    out
}

/// Replace each remote (`http`/`https`), non-tracker, non-chrome `<img>` with a
/// 🖼️ marker, and drop other images. Run only in links mode so the reader sees
/// where the loadable images sit (the marker set matches [`extract_remote_images`],
/// so the 🖼️ markers correspond 1:1 with what a 🖼️ reaction will load — both drop
/// decorative trackers and structural chrome, see [`chrome_image_offsets`]).
/// Structural chrome — logos, list-item bullet icons, and social/app badges — is
/// dropped outright rather than marked, so an image-heavy newsletter footer no
/// longer renders as a wall of 🖼️.
#[must_use]
fn placeholder_remote_images(html: &str) -> String {
    let bytes = html.as_bytes();
    let chrome = chrome_image_offsets(html);
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while let Some(start) = find_ci(&bytes[i..], b"<img", 0).map(|r| i + r) {
        out.push_str(&html[i..start]);
        let end = html[start..]
            .find('>')
            .map_or(html.len(), |g| start + g + 1);
        let tag = &html[start..end];
        let src = tag_attr(tag, "src").map_or("", |(v, _)| v);
        let lower = src.trim().to_ascii_lowercase();
        let remote = lower.starts_with("http://") || lower.starts_with("https://");
        if remote && !is_decorative_img(tag) && !chrome.contains(&start) {
            out.push_str("🖼️");
        }
        i = end;
    }
    out.push_str(&html[i..]);
    out
}

/// Rewrite each `<img>` whose `src` is a key in `url_to_mxc` to the mapped
/// `mxc://` URI, then run the Rich sanitizer (which keeps `<img>` and allows
/// only `mxc://` sources, so mapped images survive and any unmapped remote ones
/// are dropped). Used to re-render an email in place once its images are loaded.
#[must_use]
pub(crate) fn render_inline_images(html: &str, url_to_mxc: &HashMap<String, String>) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while let Some(start) = find_ci(&bytes[i..], b"<img", 0).map(|r| i + r) {
        out.push_str(&html[i..start]);
        let end = html[start..]
            .find('>')
            .map_or(html.len(), |g| start + g + 1);
        let tag = &html[start..end];
        match tag_attr(tag, "src")
            .and_then(|(src, range)| url_to_mxc.get(src).map(|mxc| (range, mxc)))
        {
            Some((range, mxc)) => {
                out.push_str(&tag[..range.start]);
                out.push_str(mxc);
                out.push_str(&tag[range.end..]);
            }
            None => out.push_str(tag),
        }
        i = end;
    }
    out.push_str(&html[i..]);
    sanitize_for_matrix(&out, RenderMode::Rich)
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
        "del",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "p",
        "a",
        "ul",
        "ol",
        "sup",
        "sub",
        "li",
        "b",
        "i",
        "u",
        "strong",
        "em",
        "s",
        "code",
        "hr",
        "br",
        "div",
        "span",
        "img",
        "pre",
        "details",
        "summary",
        "blockquote",
    ]);
    if links_mode {
        tags.remove("img"); // void → dropped entirely
        tags.remove("div"); // unwrapped (content kept)
        tags.remove("span");
    }

    let tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::from([
        ("a", HashSet::from(["href", "target"])),
        (
            "img",
            HashSet::from(["src", "width", "height", "alt", "title"]),
        ),
        ("ol", HashSet::from(["start"])),
        ("code", HashSet::from(["class"])),
    ]);

    let mut b = Builder::default();
    b.tags(tags)
        // Removed WITH their content: chrome (so `<style>` CSS does not leak as
        // text when unwrapped). Quoted-reply blockquotes are dropped separately
        // (strip_reply_blockquotes) so EDITORIAL blockquotes survive as content.
        .clean_content_tags(HashSet::from(["script", "style", "head", "title"]))
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
/// zero-width space, BOM) that marketing emails use as preheader spacers, trim
/// trailing whitespace per line, and collapse blank-line runs to a single blank
/// line. Trimming per line matters because a "blank" line that contains spaces
/// or tabs is NOT bare-newline-adjacent — `html2text` of Proton Mail's nested
/// empty HTML blocks emits a ladder of indented whitespace-only lines, which a
/// naive newline-run collapse would leave intact.
#[must_use]
fn normalize_plain(s: &str) -> String {
    let filtered = strip_invisibles(s);
    let mut out = String::with_capacity(filtered.len());
    let mut blank_run = 0u32;
    for line in filtered.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_owned()
}

/// Strip reply/forward prefixes (`Re:`, `Fwd:`, `Fw:`, repeated) from a subject
/// for use as a Matrix room NAME. In Matrix the room IS the thread, so every
/// message is a continuation and `Re:` is noise. Display-only: the outbound
/// EMAIL subject keeps its prefix (that's email convention). Falls back to the
/// original if stripping leaves nothing.
#[must_use]
pub(crate) fn clean_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(p) = ["re:", "fwd:", "fw:"]
            .iter()
            .find(|p| lower.starts_with(**p))
        else {
            break;
        };
        s = s[p.len()..].trim_start();
    }
    if s.is_empty() {
        subject.trim().to_owned()
    } else {
        s.to_owned()
    }
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

    let enc = |s: &str| utf8_percent_encode(s, URL_TEMPLATE_VALUE).to_string();
    let url = download_template
        .replace("{accountId}", &enc(account_id))
        .replace("{blobId}", &enc(blob_id))
        .replace("{name}", &enc(file_name));

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
        assert!(
            clean.contains("<p>Hello") && clean.contains("<b>world</b>"),
            "{clean}"
        );
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
        assert!(
            clean.contains("First para") && clean.contains("Second para"),
            "{clean}"
        );
        assert!(
            !clean.contains("<table") && !clean.contains("<td") && !clean.contains("<tr"),
            "table tags must be unwrapped (linearized): {clean}"
        );
    }

    #[test]
    fn table_cells_and_rows_get_separators_not_glued() {
        use super::{RenderMode, sanitize_for_matrix};
        // A layout-table row (the real Feefo/WildBounds shape) must not collapse
        // into one glued line when the disallowed table tags are unwrapped.
        let dirty = "<table><tbody>\
            <tr><td>Feefo</td><td>on behalf of</td><td>WildBounds.</td></tr>\
            <tr><td>Would you recommend?</td></tr>\
            </tbody></table><h1>Hi Thomas</h1>";
        let clean = sanitize_for_matrix(dirty, RenderMode::Rich);
        assert!(
            clean.contains("Feefo on behalf of WildBounds."),
            "cells must be space-separated, not glued: {clean}"
        );
        assert!(
            !clean.contains("Feefoon") && !clean.contains("WildBounds.Would"),
            "cells/rows must not glue: {clean}"
        );
        assert!(
            !clean.contains("recommend?Hi"),
            "the table must break before the next block: {clean}"
        );
        assert!(
            !clean.contains("<table") && !clean.contains("<td") && !clean.contains("<tr"),
            "table tags must still be unwrapped: {clean}"
        );
    }

    #[test]
    fn per_row_blocks_are_broken_by_a_br_not_foster_parented_away() {
        use super::{RenderMode, sanitize_for_matrix};
        // The real Samsonite/shipcloud shape: each language sits in its own
        // <tr><td><div>…</div></td></tr>. A <br> inserted between </tr> and <tr>
        // would be foster-parented out of the table by the HTML5 parser and lost,
        // gluing the languages; stripping the table tags ourselves keeps the break.
        let dirty = "<table><tbody>\
            <tr><td style=\"p\"><div>English here.</div></td></tr>\
            <tr><td style=\"p\"><div>German Wir haben.</div></td></tr>\
            <tr><td style=\"p\"><div>French Nous avons.</div></td></tr>\
            </tbody></table>";
        for mode in [RenderMode::Links, RenderMode::Rich] {
            let clean = sanitize_for_matrix(dirty, mode);
            // Reduce to visible lines: <br> -> newline, then drop the other tags.
            let mut plain = String::new();
            let mut in_tag = false;
            for c in clean.replace("<br>", "\n").chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => plain.push(c),
                    _ => {}
                }
            }
            let lines: Vec<&str> = plain
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            // No single line may carry two languages — that would be the glue bug.
            assert!(
                !lines
                    .iter()
                    .any(|l| l.contains("English") && l.contains("German")),
                "languages glued onto one line ({mode:?}): {lines:?}"
            );
            for lang in ["English here.", "German Wir haben.", "French Nous avons."] {
                assert!(
                    lines.iter().any(|l| l.contains(lang)),
                    "missing {lang:?} on its own line ({mode:?}): {lines:?}"
                );
            }
        }
    }

    #[test]
    fn br_adjacent_to_block_elements_is_dropped() {
        use super::{RenderMode, sanitize_for_matrix};
        // The real N26 shape: paragraphs wrapped in <p>/<h1> AND padded with <br>,
        // which renders as tall double gaps. The block already breaks the line, so
        // the adjacent <br> is dropped; a break between bare-text lines is kept.
        let html = "<p>First para.</p><br><br><h1>A Heading</h1><br><p>Second para.</p>\
            Bare line one.<br><br>Bare line two.";
        let clean = sanitize_for_matrix(html, RenderMode::Rich);
        assert!(
            !clean.contains("</p><br>")
                && !clean.contains("<br><h1")
                && !clean.contains("</h1><br>"),
            "no <br> should remain adjacent to a block element: {clean}"
        );
        assert!(
            clean.contains("Bare line one.<br>"),
            "a break between bare-text lines must survive: {clean}"
        );
    }

    #[test]
    fn link_holding_only_a_br_is_pruned() {
        use super::{RenderMode, sanitize_for_matrix};
        // An emptied logo link (image dropped, leaving just a <br>) must not leave
        // a stray blank line; a link with real text, and content, are kept.
        let html = "<p>Intro.</p>\
            <a href=\"https://x.com/\"><br></a>\
            <a href=\"https://y.com/\">Real link</a>\
            <p>Body.</p>";
        let clean = sanitize_for_matrix(html, RenderMode::Rich);
        assert!(
            !clean.contains("x.com"),
            "the <br>-only logo link must be pruned: {clean}"
        );
        assert!(
            clean.contains("Intro.")
                && clean.contains("Body.")
                && clean.contains("Real link")
                && clean.contains("y.com"),
            "real content and real links must be kept: {clean}"
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
    fn normalize_plain_collapses_whitespace_only_lines() {
        use super::normalize_plain;
        // ProtonMail's nested empty HTML renders (via html2text) to a ladder of
        // INDENTED blank lines; a bare-newline collapse would leave them intact.
        let input = "1234\n\n    \n        \n            \nbye";
        assert_eq!(normalize_plain(input), "1234\n\nbye");
        // Trailing whitespace-only lines vanish entirely.
        assert_eq!(normalize_plain("1234\n   \n      \n"), "1234");
    }

    #[test]
    fn editorial_blockquote_is_kept_but_reply_blockquote_is_dropped() {
        use super::{RenderMode, sanitize_for_matrix};
        // Editorial pull-quote (no reply marker) must survive as content — this
        // is the Pragmatic-Programmer-quote regression.
        let editorial = "<p>These are the lines that got me:</p>\
                         <blockquote><p>Conventional wisdom says…just plain wrong.</p></blockquote>\
                         <p>That's from The Pragmatic Programmer.</p>";
        let out = sanitize_for_matrix(editorial, RenderMode::Rich);
        assert!(
            out.contains("Conventional wisdom") && out.contains("<blockquote"),
            "editorial blockquote content must be preserved: {out}"
        );
        // Reply quote (Apple Mail type=cite / Gmail gmail_quote) is still dropped.
        for reply in [
            "<p>hi</p><blockquote type=\"cite\"><p>old message</p></blockquote>",
            "<p>hi</p><blockquote class=\"gmail_quote\"><p>old message</p></blockquote>",
        ] {
            let out = sanitize_for_matrix(reply, RenderMode::Rich);
            assert!(
                out.contains("hi") && !out.contains("old message") && !out.contains("<blockquote"),
                "reply blockquote must be dropped: {out}"
            );
        }
    }

    #[test]
    fn collapse_breaks_trims_and_collapses_runs() {
        use super::collapse_breaks;
        assert_eq!(collapse_breaks("1234<br><br><br><br>"), "1234");
        assert_eq!(collapse_breaks("a<br>\n<br>\n<br>\n<br>b"), "a<br><br>b");
        assert_eq!(collapse_breaks("<p>hi</p>"), "<p>hi</p>");
    }

    #[test]
    fn collapse_blank_runs_folds_filler_keeps_single_nbsp() {
        use super::collapse_blank_runs;
        // Figure-space (U+2007) ladder interleaved with spaces -> one space.
        assert_eq!(collapse_blank_runs("a\u{2007} \u{2007} \u{2007}b"), "a b");
        // A lone &nbsp; (e.g. around a button label) is preserved verbatim.
        assert_eq!(collapse_blank_runs("x&nbsp;y"), "x&nbsp;y");
        // A run of &nbsp; is folded.
        assert_eq!(collapse_blank_runs("p&nbsp;&nbsp;&nbsp;q"), "p q");
        // Single ASCII space untouched.
        assert_eq!(collapse_blank_runs("a b"), "a b");
    }

    #[test]
    fn prune_empty_elements_drops_blank_wrappers_not_content() {
        use super::prune_empty_elements;
        // Empty logo heading: <h1><a><img></a></h1> after image drop -> gone.
        assert_eq!(prune_empty_elements("<h1><a href=\"u\"> </a></h1>"), "");
        assert_eq!(prune_empty_elements("<p>keep</p>"), "<p>keep</p>");
        // <br> is meaningful-empty: a paragraph holding only <br> is NOT pruned.
        assert_eq!(prune_empty_elements("<p><br></p>"), "<p><br></p>");
    }

    #[test]
    fn sanitize_strips_newsletter_whitespace_padding() {
        use super::{RenderMode, sanitize_for_matrix};
        let html = "<h1><a href=\"https://x/logo\">  </a></h1>\
            <p>Hello\u{034F} \u{00AD}there</p>\
            \u{2007} \u{2007} \u{2007} \u{2007}\
            <p>Body.</p>";
        let out = sanitize_for_matrix(html, RenderMode::Links);
        assert!(
            !out.contains('\u{034F}') && !out.contains('\u{00AD}'),
            "invisible spacers stripped: {out:?}"
        );
        assert!(
            !out.contains('\u{2007}'),
            "figure-space ladder collapsed: {out:?}"
        );
        assert!(!out.contains("<h1>"), "empty heading pruned: {out:?}");
        assert!(out.contains("Hello there"), "real text kept: {out:?}");
        assert!(out.contains("<p>Body.</p>"), "real content kept: {out:?}");
    }

    #[test]
    fn links_mode_marks_image_spots_with_placeholder() {
        use super::{RenderMode, sanitize_for_matrix};
        let html = "<p>before</p>\
            <a href=\"https://x/go\"><img src=\"https://x/banner.png\" alt=\"Banner\"></a>\
            <img src=\"https://t/p.gif\" width=\"1\" height=\"1\">\
            <img src=\"https://x/icon.png\" width=\"24\" height=\"24\">\
            <p>after</p>";
        let out = sanitize_for_matrix(html, RenderMode::Links);
        // Exactly one 🖼️ (the content banner); the 1×1 tracker and the 24×24
        // decorative icon get none.
        assert_eq!(
            out.matches('🖼').count(),
            1,
            "one marker for the content image: {out}"
        );
        // The wrapping link is kept, so the marker is clickable.
        assert!(out.contains("href=\"https://x/go\""), "link kept: {out}");
        assert!(!out.contains("<img"), "no raw img in links mode: {out}");
        assert!(
            out.contains("before") && out.contains("after"),
            "text kept: {out}"
        );
        // Rich mode does NOT add placeholders (it keeps real images).
        assert!(!sanitize_for_matrix(html, RenderMode::Rich).contains('🖼'));
    }

    #[test]
    fn extract_remote_images_finds_remotes_and_flags_decorative() {
        use super::extract_remote_images;
        let html = "<img src=\"https://x/hero.png\" width=\"600\">\
            <img alt=\"px\" src=\"https://t/track.gif\" width=\"1\" height=\"1\">\
            <img src=\"https://x/icon.png\" width=\"24\" height=\"24\">\
            <img src=\"https://x/hero.png\">\
            <img src=\"cid:inline\"><img data-src=\"https://x/lazy.png\">";
        let imgs = extract_remote_images(html);
        // Three unique remote http(s) srcs; cid: and data-src ignored; deduped.
        assert_eq!(imgs.len(), 3, "{imgs:?}");
        assert_eq!(imgs[0].url, "https://x/hero.png");
        assert!(!imgs[0].is_decorative, "600px-wide hero is content");
        assert!(imgs[1].is_decorative, "1x1 tracker is decorative");
        assert!(imgs[2].is_decorative, "24x24 icon is decorative");
    }

    #[test]
    fn structural_chrome_images_are_dropped_not_marked() {
        use super::{RenderMode, sanitize_for_matrix};
        // A newsletter shaped like the real InPost mail: masthead logos, list
        // bullet icons, an app-badge column and a social-icon row — all CSS-sized
        // (no width/height), so is_decorative_img alone would keep them. None is a
        // content image, so none should render as a 🖼️.
        let html = "\
            <img src=\"https://x/logo.png\"><img src=\"https://x/banner.png\">\
            <p>Hola, tu paquete te espera.</p>\
            <ul>\
              <li><img src=\"https://x/check.png\"> Presenta este mensaje.</li>\
              <li><img src=\"https://x/check.png\"> Lleva tu documento.</li>\
            </ul>\
            <a href=\"https://x/ios\"><img src=\"https://x/ios.png\"><br><img src=\"https://x/and.png\"></a>\
            <a href=\"https://x/fb\"><img src=\"https://x/fb.png\"></a> \
            <a href=\"https://x/ig\"><img src=\"https://x/ig.png\"></a>\
            <p>Gracias.</p>";
        let out = sanitize_for_matrix(html, RenderMode::Links);
        assert_eq!(
            out.matches('🖼').count(),
            0,
            "every image is chrome and must be dropped, not marked: {out}"
        );
        assert!(
            out.contains("paquete") && out.contains("Presenta") && out.contains("Gracias"),
            "surrounding text must survive: {out}"
        );
    }

    #[test]
    fn lone_linked_banner_and_standalone_photo_stay_marked() {
        use super::{RenderMode, sanitize_for_matrix};
        // A single linked hero (not a strip) and a standalone content image after
        // text are NOT chrome — each keeps its clickable 🖼️ load-marker.
        let html = "<p>Check out our sale:</p>\
            <a href=\"https://x/sale\"><img src=\"https://x/hero.png\" width=\"600\"></a>\
            <p>And this photo:</p><img src=\"https://x/photo.png\" width=\"500\">";
        let out = sanitize_for_matrix(html, RenderMode::Links);
        assert_eq!(
            out.matches('🖼').count(),
            2,
            "lone linked banner + standalone photo keep their markers: {out}"
        );
    }

    #[test]
    fn image_only_email_keeps_its_images() {
        use super::{RenderMode, sanitize_for_matrix};
        // No visible text anywhere: the leading-logo rule must NOT fire, or an
        // image-only email would be stripped to nothing.
        let html = "<img src=\"https://x/a.png\" width=\"500\">\
            <img src=\"https://x/b.png\" width=\"500\">";
        let out = sanitize_for_matrix(html, RenderMode::Links);
        assert_eq!(
            out.matches('🖼').count(),
            2,
            "image-only email keeps its images: {out}"
        );
    }

    #[test]
    fn extract_flags_chrome_strip_as_decorative_so_it_is_not_loaded() {
        use super::extract_remote_images;
        // The marker set and the load set must agree: a social-icon strip is
        // chrome, so a 🖼️ reaction must not fetch it; the standalone photo loads.
        let html = "<p>hi</p>\
            <a href=\"https://x/fb\"><img src=\"https://x/fb.png\"></a> \
            <a href=\"https://x/ig\"><img src=\"https://x/ig.png\"></a>\
            <p>see this:</p><img src=\"https://x/photo.png\" width=\"600\">";
        let imgs = extract_remote_images(html);
        let decorative = |u: &str| {
            imgs.iter()
                .find(|r| r.url == u)
                .is_some_and(|r| r.is_decorative)
        };
        assert!(
            decorative("https://x/fb.png"),
            "fb icon is a strip → decorative"
        );
        assert!(
            decorative("https://x/ig.png"),
            "ig icon is a strip → decorative"
        );
        assert!(
            !decorative("https://x/photo.png"),
            "standalone photo is content → loadable"
        );
    }

    #[test]
    fn tracking_pixels_are_dropped_by_url() {
        use super::{RenderMode, extract_remote_images, sanitize_for_matrix};
        // A Salesforce ImageServer beacon (declares NO dimensions, so the size
        // rules miss it) and a Mandrill open tracker are dropped in the marker
        // path and flagged decorative in the load path; a real photo stays.
        let html = "<p>Hi.</p>\
            <img src=\"https://x.my.salesforce.com/servlet/servlet.ImageServer?id=abc\">\
            <img src=\"https://track.example.com/track/open?u=1\">\
            <img src=\"https://cdn.example.com/photo.png\" width=\"600\">";
        let out = sanitize_for_matrix(html, RenderMode::Links);
        assert_eq!(
            out.matches('🖼').count(),
            1,
            "only the real content image is marked: {out}"
        );
        let imgs = extract_remote_images(html);
        let decorative = |u: &str| {
            imgs.iter()
                .find(|r| r.url.contains(u))
                .is_some_and(|r| r.is_decorative)
        };
        assert!(decorative("ImageServer"), "salesforce beacon is a tracker");
        assert!(decorative("track/open"), "mandrill open tracker");
        assert!(
            !decorative("photo.png"),
            "a real content image must still load"
        );
    }

    #[test]
    fn single_small_dimension_is_a_decorative_icon() {
        use super::extract_remote_images;
        // The TUMI-footer shape: a social icon is width="20" with height left to
        // CSS (no numeric height), so the "both dims small" rule missed it. A
        // content image with only a large width stays content.
        let html = "<p>x</p>\
            <img src=\"https://x/social.png\" width=\"20\">\
            <img src=\"https://x/hero.png\" width=\"600\">";
        let imgs = extract_remote_images(html);
        let dec = |u: &str| {
            imgs.iter()
                .find(|r| r.url.contains(u))
                .is_some_and(|r| r.is_decorative)
        };
        assert!(
            dec("social.png"),
            "20px-wide icon (auto height) is decorative"
        );
        assert!(!dec("hero.png"), "600px-wide content image is not");
    }

    #[test]
    fn render_inline_images_rewrites_mapped_and_drops_unmapped() {
        use super::{RemoteImg, decode_src_entities, extract_remote_images, render_inline_images};
        use std::collections::HashMap;
        let html = "<p>hi</p><img src=\"https://x/a.png?u=1&amp;v=2\">\
            <img src=\"https://x/b.png\">";
        // Only a.png is "loaded" (mapped to mxc); b.png stays remote.
        let mut map = HashMap::new();
        map.insert(
            "https://x/a.png?u=1&amp;v=2".to_owned(),
            "mxc://hs/aaa".to_owned(),
        );
        let out = render_inline_images(html, &map);
        assert!(out.contains("mxc://hs/aaa"), "mapped img inlined: {out}");
        assert!(
            !out.contains("https://x/b.png"),
            "unmapped remote img dropped: {out}"
        );
        assert!(out.contains("<p>hi</p>"), "text kept: {out}");
        // Entity decode turns the rewrite key into a fetchable URL.
        assert_eq!(
            decode_src_entities("https://x/a.png?u=1&amp;v=2"),
            "https://x/a.png?u=1&v=2"
        );
        // The extractor's key matches the map key exactly (so rewrite lands).
        let imgs: Vec<RemoteImg> = extract_remote_images(html);
        assert!(map.contains_key(&imgs[0].url));
    }

    #[test]
    fn unwrap_li_paragraphs_keeps_marker_and_text_inline() {
        use super::unwrap_li_paragraphs;
        // The newsletter pattern: each list item's text wrapped in a block <p>.
        let html = "<ol><li>\n<p>Using AI to improve design systems</p>\n</li>\
                    <li><p>Making better products</p></li></ol>";
        let out = unwrap_li_paragraphs(html);
        assert!(
            !out.contains("<p>") && !out.contains("</p>"),
            "paragraphs inside list items must be unwrapped: {out}"
        );
        assert!(out.contains("<li>") && out.contains("Using AI to improve design systems"));
        // A <p> OUTSIDE any list is a real paragraph break — leave it alone.
        let para = "<p>intro</p><ol><li><p>one</p></li></ol><p>outro</p>";
        let out = unwrap_li_paragraphs(para);
        assert!(out.starts_with("<p>intro</p>"), "outer <p> kept: {out}");
        assert!(out.ends_with("<p>outro</p>"), "outer <p> kept: {out}");
        assert!(!out.contains("<li><p>"), "li <p> dropped: {out}");
        // <pre> must not be mistaken for <p>, even inside an <li>.
        let pre = "<li><pre>code</pre></li>";
        assert_eq!(unwrap_li_paragraphs(pre), pre);
    }

    #[test]
    fn full_pipeline_inlines_styled_list_item_paragraphs() {
        use super::{RenderMode, sanitize_for_matrix};
        // Verbatim shape from a real newsletter: <p style="…"> inside each <li>.
        let html = "<ol style=\"color:#000\">\
            <li style=\"font-size:16px\"><p style=\"font-size:16px\">Using AI to improve design systems</p></li>\
            <li style=\"font-size:16px\"><p style=\"font-size:16px\">Making better products</p></li></ol>";
        let out = sanitize_for_matrix(html, RenderMode::Links);
        assert!(
            out.contains("<li>Using AI to improve design systems</li>"),
            "marker and text must end up inline (no inner <p>): {out}"
        );
        assert!(
            !out.contains("<p>"),
            "list-item paragraphs unwrapped: {out}"
        );
    }

    #[test]
    fn clean_subject_strips_reply_and_forward_prefixes() {
        use super::clean_subject;
        assert_eq!(clean_subject("Re: compose test"), "compose test");
        assert_eq!(clean_subject("RE: Re:  Fwd: compose test"), "compose test");
        assert_eq!(clean_subject("Fw: hello"), "hello");
        assert_eq!(clean_subject("compose test"), "compose test");
        // "Re:" must be a prefix, not anywhere ("Carefree:" is untouched).
        assert_eq!(clean_subject("Carefree: living"), "Carefree: living");
        // Degenerate all-prefix subject falls back to the original.
        assert_eq!(clean_subject("Re:"), "Re:");
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
        assert_eq!(
            q,
            "On 2026-06-17 00:07 UTC, Thomas <t@x> wrote:\n> a\n>\n> b"
        );
    }

    #[test]
    fn format_reply_quote_is_reversed_by_strip_quoted_reply() {
        use super::{format_reply_quote, strip_quoted_reply};
        // The matched-pair invariant: a body we quote is fully removed again by
        // the inbound stripper, so the quote never leaks into the timeline.
        let quote = format_reply_quote(
            "Thomas <t@x>",
            "2026-06-17 00:07 UTC",
            "old line 1\nold line 2",
        );
        let outbound = format!("my new reply\n\n{quote}");
        assert_eq!(strip_quoted_reply(&outbound), "my new reply");
    }

    #[test]
    fn clamp_utf8_never_splits_a_codepoint() {
        use super::clamp_utf8;
        let s = "é".repeat(100); // 200 bytes, no whitespace
        let out = clamp_utf8(&s, 51); // 51 lands mid-codepoint -> backs up to 50
        assert!(out.len() <= 51 && out.is_char_boundary(out.len()));
        assert_eq!(
            out.chars().count(),
            25,
            "should keep whole 'é' chars: {out:?}"
        );
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
        assert!(
            p.len() <= MATRIX_BODY_BUDGET,
            "plain must fit the budget: {}",
            p.len()
        );
        assert!(
            p.contains("truncated"),
            "a truncation notice must be appended"
        );
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
        assert!(
            total <= MATRIX_BODY_BUDGET,
            "bridged event body must fit Matrix's limit: {total}"
        );
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
            assert!(
                !lower.contains("<script"),
                "{mode:?}: script tag survived: {out}"
            );
            assert!(
                !lower.contains("alert(1)"),
                "{mode:?}: script content survived: {out}"
            );
            assert!(
                !lower.contains("<style") && !lower.contains("secretcss"),
                "{mode:?}: style survived: {out}"
            );
            assert!(
                !lower.contains("onclick"),
                "{mode:?}: inline handler survived: {out}"
            );
            assert!(
                !lower.contains("javascript:"),
                "{mode:?}: javascript: scheme survived: {out}"
            );
            assert!(
                !lower.contains("<marquee"),
                "{mode:?}: disallowed tag survived: {out}"
            );
            // Non-mxc/non-allowed-scheme img src must be dropped (url_relative/schemes).
            assert!(
                !out.contains("http://evil"),
                "{mode:?}: bad img src survived: {out}"
            );
            // The text content of unwrapped tags is preserved.
            assert!(
                out.contains("hi") && out.contains("red"),
                "{mode:?}: text lost: {out}"
            );
        }
        // data-mx-color is a Matrix attribute and must be preserved (Rich keeps span).
        let rich = sanitize_for_matrix(dirty, RenderMode::Rich);
        assert!(
            rich.contains("data-mx-color"),
            "data-mx-* must be preserved: {rich}"
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
        let html = body
            .html
            .as_deref()
            .expect("links mode emits a formatted body");
        assert!(html.contains("href=\"https://kit.com/confirm\""), "{html}");
        assert!(!html.contains("<img"), "links mode drops images: {html}");
    }

    #[test]
    fn rich_mode_keeps_full_html_including_images() {
        let body = EmailBody::from_email(&link_email(), RenderMode::Rich);
        let html = body
            .html
            .as_deref()
            .expect("rich mode emits a formatted body");
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
