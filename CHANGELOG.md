# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
From v0.3.0 onward this file is maintained automatically by
[release-plz](https://release-plz.dev) from the Conventional Commit history
(see [ADR-0008](docs/adr/0008-ci-and-release-flow.md)).

## [0.4.0](https://github.com/palebluebytes/jmap-matrix-bridge/compare/v0.3.2...v0.4.0) - 2026-07-16

### Added

- *(send-delay)* [**breaking**] disable the hold window by default

## [0.3.2](https://github.com/palebluebytes/jmap-matrix-bridge/compare/v0.3.1...v0.3.2) - 2026-07-16

### Added

- drop tracking-pixel images by URL (no marker, never fetched)
- separate table cells/rows so linearized layouts don't glue
- drop structural chrome images instead of marking them

### Fixed

- *(examples)* render emails with real content types, not forced html
- blank line between sibling <div> sections (title vs greeting)
- keep <br> breathing room around headings
- treat a single small img dimension (auto other side) as a decorative icon
- prune links whose only content is a <br>
- drop <br> adjacent to block elements to remove double gaps
- keep table/section breaks that HTML5 foster-parenting dropped

### Other

- rustfmt render_gallery example
- add render_email/render_gallery examples for offline HTML review

## [0.3.1](https://github.com/palebluebytes/jmap-matrix-bridge/compare/v0.3.0...v0.3.1) - 2026-07-08

### Other

- *(deps)* bump aes-gcm 0.11; document sha2/hmac pin ([#52](https://github.com/palebluebytes/jmap-matrix-bridge/pull/52))
- update flake.lock ([#19](https://github.com/palebluebytes/jmap-matrix-bridge/pull/19))
- bump ammonia from 4.1.2 to 4.1.3 ([#50](https://github.com/palebluebytes/jmap-matrix-bridge/pull/50))
- bump rand from 0.10.1 to 0.10.2 ([#49](https://github.com/palebluebytes/jmap-matrix-bridge/pull/49))
- bump jiff from 0.2.28 to 0.2.31 ([#47](https://github.com/palebluebytes/jmap-matrix-bridge/pull/47))
- bump uuid from 1.23.3 to 1.23.4 ([#43](https://github.com/palebluebytes/jmap-matrix-bridge/pull/43))
- bump anyhow from 1.0.102 to 1.0.103 ([#42](https://github.com/palebluebytes/jmap-matrix-bridge/pull/42))

## [0.3.0](https://github.com/palebluebytes/jmap-matrix-bridge/compare/v0.2.1...v0.3.0) - 2026-06-25

### Added

- *(double-puppet)* automatic setup via shared-secret-auth ([#40](https://github.com/palebluebytes/jmap-matrix-bridge/pull/40))
- *(read-state)* mirror JMAP $seen to Matrix via puppet receipt ([#41](https://github.com/palebluebytes/jmap-matrix-bridge/pull/41))
- *(send-state)* ⏳→✅/❌ reaction indicator + one-time hint ([#39](https://github.com/palebluebytes/jmap-matrix-bridge/pull/39))
- *(trash-junk)* delete-room/🗑 → Trash, spam/🚫 → Junk ([#38](https://github.com/palebluebytes/jmap-matrix-bridge/pull/38))
- *(sync)* add sync command and email-space repair ([#37](https://github.com/palebluebytes/jmap-matrix-bridge/pull/37))
- *(send-delay)* hold outbound mail with redact/edit undo window ([#36](https://github.com/palebluebytes/jmap-matrix-bridge/pull/36))
- *(images)* add show-images command, twin of the 🖼️ reaction ([#35](https://github.com/palebluebytes/jmap-matrix-bridge/pull/35))
- *(commands)* add status (ping) and logout ([#34](https://github.com/palebluebytes/jmap-matrix-bridge/pull/34))
- *(permissions)* default-deny access map with user/admin levels ([#33](https://github.com/palebluebytes/jmap-matrix-bridge/pull/33))

### Other

- *(adr)* record feature-gap decisions (ADR-0009..0015) ([#31](https://github.com/palebluebytes/jmap-matrix-bridge/pull/31))

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
