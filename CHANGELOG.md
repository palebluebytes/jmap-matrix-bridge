# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
From v0.3.0 onward this file is maintained automatically by
[release-plz](https://release-plz.dev) from the Conventional Commit history
(see [ADR-0008](docs/adr/0008-ci-and-release-flow.md)).

## [0.2.1] - 2026-06-20

### Fixed

- The bot's Matrix profile (display name + avatar) is now applied idempotently.
  Previously the bridge re-uploaded its avatar on every startup, minting a fresh
  `mxc` each time and orphaning the prior media on the homeserver. The applied
  display name and the avatar's content hash are now persisted; the profile is
  set once and re-applied only when the embedded asset changes (#15).

### Changed

- The bot avatar is now the 📨 emoji as a genuine 512×512 PNG; the previous
  `logo.png` was a JPEG misnamed `.png` while uploaded as `image/png` (#15).

### Internal

- The dev shell now provides the `mold` linker that `.cargo/config.toml`
  requires, so `cargo build` links inside `nix develop` (#16).

## [0.2.0] - 2026-06-19

Initial tagged release: a Matrix Application Service bridging a JMAP email
account into Matrix — one room per email thread, ghost-represented
correspondents, optional double-puppeting, push-driven sync, and verified
outbound send with a durable retry queue.
