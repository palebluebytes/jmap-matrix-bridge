//! Render one email through the bridge's real content pipeline, so you can see
//! exactly what `formatted_body` (and the plain fallback) a Matrix client would
//! get — without a homeserver, JMAP, or a deploy.
//!
//! Two input shapes are accepted:
//!   * A raw JMAP `Email/get` object (`{ "subject": …, "textBody": …,
//!     "htmlBody": …, "bodyValues": … }`). This is the faithful path: the object
//!     is deserialized into the very type the bridge renders, so an email with no
//!     genuine `text/html` part correctly renders as plain text (quotes stripped)
//!     rather than being forced through the HTML pipeline.
//!   * Raw HTML (anything that isn't a JSON email object). Treated as an HTML
//!     body — handy for pasting a snippet, but by construction it always takes
//!     the HTML path, so use a JMAP object when content-type fidelity matters.
//!
//! Usage:
//!   `cargo run --example render_email -- path/to/email.json`
//!   `cargo run --example render_email -- path/to/email.html`
//!   `cat email.html | cargo run --example render_email`

use std::io::Read;

use jmap_matrix_bridge::services::content::{EmailBody, RenderMode};

/// Parse the input as a raw JMAP Email object; fall back to wrapping it as a
/// single `text/html` body when it isn't a JSON email.
fn parse_email(input: &str) -> jmap_client::email::Email {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(input) {
        let is_email = value.is_object()
            && ["textBody", "htmlBody", "bodyValues"]
                .iter()
                .any(|k| value.get(k).is_some());
        if is_email {
            return serde_json::from_value(value).expect("deserialize JMAP Email object");
        }
    }
    // Not a JMAP object — treat the whole input as a raw HTML body.
    serde_json::from_value(serde_json::json!({
        "id": "demo",
        "threadId": "t",
        "subject": "render_email demo",
        "htmlBody": [{ "partId": "1", "type": "text/html" }],
        "bodyValues": { "1": { "value": input, "isTruncated": false } }
    }))
    .expect("build Email from raw HTML")
}

fn main() {
    let input = std::env::args().nth(1).map_or_else(
        || {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .expect("read stdin");
            buf
        },
        |path| std::fs::read_to_string(&path).expect("read input file"),
    );

    let email = parse_email(&input);

    for mode in [RenderMode::Links, RenderMode::Rich] {
        let body = EmailBody::from_email(&email, mode);
        println!("\n══════════ {mode:?} — formatted_body (what a client renders) ══════════");
        println!("{}", body.html.as_deref().unwrap_or("(no formatted body)"));
        println!("\n────────── {mode:?} — plain body (notification / fallback) ──────────");
        println!("{}", body.plain);
    }
}
