//! Render many emails' original HTML through the bridge's real content pipeline
//! and emit a single self-contained HTML gallery — so you can eyeball how every
//! bridged email looks under the *current* code (both render modes), without a
//! homeserver or a deploy.
//!
//! Input: a JSON array of `{ "subject": ..., "from": ..., "html": ... }` objects
//! (produce it with a JMAP fetch of each email's `htmlBody`). Reads the file named
//! in argv, or stdin. Writes the gallery HTML to stdout.
//!
//! Usage:
//!   `cargo run --example render_gallery -- emails_html.json > new_rendering.html`

use std::io::Read;

use jmap_matrix_bridge::services::content::{EmailBody, RenderMode};

const CSS: &str = "\
:root{--bg:#15171b;--card:#1e2126;--fg:#e6e8eb;--mut:#9aa0aa;--line:#2b2f36;--accent:#0dbd8b}\
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);\
font:15px/1.55 -apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif}\
header{position:sticky;top:0;background:var(--bg);border-bottom:1px solid var(--line);padding:14px 20px;z-index:5}\
h1{font-size:18px;margin:0}.sub{color:var(--mut);font-size:13px;margin-top:4px}\
main{max-width:960px;margin:0 auto;padding:20px}\
.card{border:1px solid var(--line);border-radius:12px;margin:0 0 20px;overflow:hidden;background:var(--card)}\
.hd{padding:10px 14px;border-bottom:1px solid var(--line);display:flex;gap:10px;align-items:baseline;flex-wrap:wrap}\
.subj{font-weight:600}.from{color:var(--mut);font-size:13px}\
.body{padding:12px 14px}.lbl{font-size:11px;text-transform:uppercase;letter-spacing:.04em;color:var(--mut);margin:0 0 6px}\
.render{border:1px dashed var(--line);border-radius:8px;padding:10px;background:var(--bg)}\
.render img{max-width:100%}.render blockquote{border-left:3px solid var(--line);margin:.3em 0;padding-left:.8em;color:var(--mut)}\
.render ul{padding-left:1.3em}";

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
        |path| std::fs::read_to_string(&path).expect("read JSON file"),
    );

    let emails: Vec<serde_json::Value> = serde_json::from_str(&input).expect("parse JSON array");

    let mut out = String::new();
    out.push_str("<!doctype html><meta charset=utf-8><title>New rendering — all emails</title>");
    out.push_str("<style>");
    out.push_str(CSS);
    out.push_str("</style><header><h1>All emails — current rendering code</h1>");
    out.push_str("<div class=sub>");
    out.push_str(&emails.len().to_string());
    out.push_str(
        " emails · Links-mode <code>formatted_body</code> (what a client shows) · \
         mxc images absent (no homeserver auth)</div></header><main>",
    );

    for e in &emails {
        let subject = e.get("subject").and_then(serde_json::Value::as_str).unwrap_or("(no subject)");
        let from = e.get("from").and_then(serde_json::Value::as_str).unwrap_or("");
        let html = e.get("html").and_then(serde_json::Value::as_str).unwrap_or("");

        let email: jmap_client::email::Email = serde_json::from_value(serde_json::json!({
            "id": "g", "threadId": "t",
            "htmlBody": [{ "partId": "1", "type": "text/html" }],
            "bodyValues": { "1": { "value": html, "isTruncated": false } }
        }))
        .expect("build Email");
        let rendered = EmailBody::from_email(&email, RenderMode::Links);
        let formatted = rendered.html.unwrap_or_else(|| esc(&rendered.plain));

        out.push_str("<div class=card><div class=hd><span class=subj>");
        out.push_str(&esc(subject));
        out.push_str("</span><span class=from>");
        out.push_str(&esc(from));
        out.push_str("</span></div><div class=body><div class=lbl>formatted_body (rendered)</div>");
        out.push_str("<div class=render>");
        out.push_str(&formatted);
        out.push_str("</div></div></div>");
    }
    out.push_str("</main>");
    print!("{out}");
}
