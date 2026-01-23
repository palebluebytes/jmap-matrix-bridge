# JMAP Matrix Bridge

A Rust-based Matrix Application Service (Bridge) that synchronizes emails from a JMAP server (like Stalwart or Fastmail) into Matrix rooms.

## Features

-   **Multi-User Support**: Dynamic session management allows multiple Matrix users to bridge their individual JMAP accounts.
-   **Bi-directional Sync**:
    -   **Inbound (Email -> Matrix)**: Polls JMAP for new emails and creates Matrix threads.
    -   **Outbound (Matrix -> Email)**: Listens for Matrix messages and sends them as replies or new emails via JMAP.
-   **Commands**: Built-in bot commands (e.g., `!login`) for self-service authentication.
-   **Multi-Mailbox Support**: Maps JMAP Mailboxes (Folders) to distinct Matrix Rooms.
-   **Threading**: Preserves email threading by using Matrix threads.
-   **Persistence**: Uses a local SQLite database to track state, user credentials, and synchronization mapping.

## Architecture

*   **ClientManager**: persistent process that manages active JMAP sessions. Loads users from the database on startup.
*   **Ingest Loop**: Per-user `JmapPoller` tasks that fetch changes from JMAP.
*   **Store**: SQLite database (`bridge.db`) managing:
    *   `users`: Registered JMAP credentials linked to Matrix IDs.
    *   `mailbox_mapping`: `jmap_id` <-> `matrix_id`.
    *   `thread_mapping`: `jmap_thread_id` <-> `matrix_event_id`.
*   **Matrix Client**: Manages "ghost" users (e.g., `@_jmap_user_domain.com:server`) and sends events.
*   **Web Server**: `Axum` server listens for Matrix Application Service transactions and command events.

## Usage

### Build
```bash
nix build .#jmap-matrix-bridge
# or
cargo build --release
```

### Configuration
The bridge requires a `registration.yaml` to be registered with the Matrix homeserver (Synapse/Conduit/Dendrite).

**Generate Registration**:
```bash
jmap-matrix-bridge generate-registration --url http://localhost:8008 --output registration.yaml
```

### Run
The bridge operates as a daemon. It is typically deployed via the NixOS module (see `modules/nixos/jmap-bridge`).

Manual execution:
```bash
jmap-matrix-bridge run \
  --db sqlite:bridge.db \
  --port 8008 \
  --matrix-url http://localhost:6167 \
  --matrix-as-token "AS_TOKEN_FROM_REGISTRATION" \
  # Optional: Legacy single-user args (deprecated)
  # --jmap-username user@example.com \
  # --jmap-token "SECRET" \
  # --jmap-url http://localhost:8080/jmap/session
```

## User Guide

### Logging In
Users authenticate directly from their Matrix client by opening a Direct Message (DM) with the bridge bot (usually `@_jmap_bot:yourserver.com`).

Send the command:
```
!login <jmap_username> <jmap_password> <jmap_session_url>
```

Example:
```
!login user@palebluebytes.space mysecretpassword https://mail.palebluebytes.space/jmap/session
```

On success, the bridge will begin syncing existing emails.

### Sending Emails
- **Reply**: Reply to a bridged message in Matrix to send an email reply.
- **New Email**: (Coming Soon) `!email <to> <subject> <body>`

## Development
Run tests (including integration tests with mock servers):
```bash
nix build .#jmap-matrix-bridge --check
# or
cargo test
```
