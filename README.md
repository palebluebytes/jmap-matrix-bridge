# JMAP Matrix Bridge

A Rust-based Matrix Application Service (Bridge) that synchronizes emails from a JMAP server (like Stalwart or Fastmail) into Matrix rooms.

## Features

-   **Bi-directional Sync**:
    -   **Inbound (Email -> Matrix)**: Polls JMAP for new emails and creates Matrix threads.
    -   **Outbound (Matrix -> Email)**: Listens for Matrix messages and sends them as replies or new emails via JMAP.
-   **Multi-Mailbox Support**: Maps JMAP Mailboxes (Folders) to distinct Matrix Rooms.
-   **Threading**: Preserves email threading by using Matrix threads.
-   **Idempotency**: Uses a local SQLite database to track state and prevent duplicates.
-   **Native Rust**: Built with `ruma`, `jmap-client`, and `axum` for high performance and type safety.

## Architecture

*   **Ingest**: `JmapPoller` runs a loop to fetch changes from the JMAP server.
*   **Store**: SQLite database (`bridge.db`) maps `jmap_id` <-> `matrix_id`.
*   **Matrix Client**: Manages "ghost" users (e.g., `@_jmap_user_domain.com:server`) and sends events.
*   **Web Server**: `Axum` server listens for Matrix Application Service transactions (incoming events).

## Usage

### Build
```bash
cargo build --release
```

### Configuration
The bridge requires a `registration.yaml` to be registered with the Matrix homeserver.

**Generate Registration**:
```bash
cargo run -- generate-registration --url http://localhost:8008 --output registration.yaml
```

### Run
The bridge is configured via CLI arguments and Environment Variables.

```bash
jmap-matrix-bridge run \
  --db sqlite:bridge.db \
  --jmap-username user@example.com \
  --jmap-token "SECRET" \
  --jmap-url http://localhost:8080 \
  --matrix-url http://localhost:6167 \
  --matrix-as-token "AS_TOKEN_FROM_REGISTRATION"
```

## Database Schema
-   `mailbox_mapping`: Maps JMAP Mailbox IDs to Matrix Room IDs.
-   `thread_mapping`: Maps JMAP Thread IDs to the Matrix Root Event ID (for threading).
-   `message_mapping`: Maps JMAP Email IDs to Matrix Event IDs (to prevent re-importing).

## Development
Run tests (including integration tests with `wiremock`):
```bash
cargo test
```
