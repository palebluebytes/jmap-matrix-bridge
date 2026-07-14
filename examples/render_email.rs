//! Render a raw email HTML body through the bridge's real content pipeline, so
//! you can see exactly what `formatted_body` (and the plain fallback) a Matrix
//! client would get — without a homeserver, JMAP, or a deploy.
//!
//! Usage:
//!   `cargo run --example render_email -- path/to/email.html`
//!   `cat email.html | cargo run --example render_email`
//!
//! Pair it with a JMAP fetch of a real email's HTML to inspect a specific
//! production message under the current code.

use std::io::Read;

use jmap_matrix_bridge::services::content::{EmailBody, RenderMode};

fn main() {
    let html = std::env::args().nth(1).map_or_else(
        || {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .expect("read stdin");
            buf
        },
        |path| std::fs::read_to_string(&path).expect("read HTML file"),
    );

    // Wrap the raw HTML in the minimal JMAP Email shape `from_email` expects.
    let email: jmap_client::email::Email = serde_json::from_value(serde_json::json!({
        "id": "demo",
        "threadId": "t",
        "subject": "render_email demo",
        "htmlBody": [{ "partId": "1", "type": "text/html" }],
        "bodyValues": { "1": { "value": html, "isTruncated": false } }
    }))
    .expect("build Email from JSON");

    for mode in [RenderMode::Links, RenderMode::Rich] {
        let body = EmailBody::from_email(&email, mode);
        println!("\n══════════ {mode:?} — formatted_body (what a client renders) ══════════");
        println!("{}", body.html.as_deref().unwrap_or("(no formatted body)"));
        println!("\n────────── {mode:?} — plain body (notification / fallback) ──────────");
        println!("{}", body.plain);
    }
}
