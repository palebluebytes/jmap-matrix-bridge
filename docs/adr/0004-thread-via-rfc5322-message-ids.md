# Outbound replies thread via RFC 5322 Message-IDs, not JMAP ids

An outbound reply's `In-Reply-To`/`References` headers are built from the real RFC 5322 `Message-ID`s of the messages in the JMAP thread (`JmapSender::reply_headers`, `src/sender.rs`), **not** from the parent email's opaque JMAP internal id. The JMAP id is server-local and meaningless to other mail servers and to Stalwart's own thread-grouping; resolving the headers from the thread (rather than a single parent) also means a missing or deleted parent does not break threading — the parent email id is best-effort only (`src/ghost.rs`).

## Considered Options

- **Use the parent email's JMAP id as `In-Reply-To` (rejected)** — opaque and non-portable; external mail servers can't correlate it, so replies wouldn't thread for the recipient, and a stale/missing parent would drop threading entirely.
- **Resolve `Message-ID`s from the whole JMAP thread (chosen)** — portable, recipient-correct, and resilient to a missing parent.

## Consequences

- Threading is decoupled from any single message: the reply path must tolerate a `None` parent email id and still thread correctly.
