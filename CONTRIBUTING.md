# Contributing

Thanks for looking at the bridge. This page is the short version for humans;
[`AGENTS.md`](AGENTS.md) holds the full conventions and is the file to read
before you change anything non-trivial. It is written as directives for AI
agents, but the rules are the project's rules and apply to everyone.

## Getting set up

The repository is managed by a Nix flake, and the dev shell provides the whole
toolchain — Rust, `just`, `cargo-nextest`, `bacon`:

```bash
nix develop      # or let direnv do it
```

Don't install toolchain pieces another way. No `rustup`, no `cargo install`, no
system package manager — if something is missing, add it to `flake.nix`. That
boundary is what keeps the build reproducible and the CI sandbox honest.

## The loop

`just` is the interface; run it bare to list everything.

```bash
just check       # cargo check — after every logical edit
just nextest     # the test suite; always use this, not cargo test
just lint        # clippy + rustfmt --check
just fix         # auto-fix the safe clippy lints
just playground  # boot the sandbox VM and click through the bridge
```

Before you push: `just lint && just nextest`. The authoritative gate is
`nix flake check`, which is what CI runs — it builds the package, runs clippy,
`rustfmt --check`, the test suite, and (on x86_64) an end-to-end VM round-trip
test. Running it locally takes a while but it is the same thing CI will do.

## Things that will come up in review

- **Use the domain vocabulary.** Ghost, puppet, bot, thread, room, space,
  submission, backfill all have specific meanings, defined in
  [`CONTEXT.md`](CONTEXT.md). Use them in code, tests and issues.
- **Check the ADRs.** Decisions of record live in [`docs/adr/`](docs/adr/README.md).
  If your change contradicts one, say so and argue the case — don't quietly
  override it. New decisions get a new ADR.
- **`is_ok()` is not a test oracle.** The bridge degrades gracefully on purpose:
  failed sends notify the user rather than crash, and the sync loop swallows
  per-email errors so one bad message can't stall everything. That means
  `assert!(result.is_ok())` on those paths passes by construction. Pin the
  requests the behaviour must make with `.expect(N)` and assert the user-visible
  outcome. `AGENTS.md` §4 covers this in full — it's the section most worth
  reading before you write a test.
- **Prove a new test can fail.** Break the code it covers, watch it go red,
  revert. This suite has shipped tests that could not fail.
- **Integration tests mock HTTP with `wiremock`.** They must pass offline, inside
  the Nix sandbox. No live network.

## Commits and releases

Commits follow [Conventional Commits](https://www.conventionalcommits.org)
(`type(scope): subject`) — the changelog is generated from them. Releases are
automated with release-plz: merging to `main` keeps a release PR up to date, and
cutting a release is merging that PR
([ADR-0008](docs/adr/0008-ci-and-release-flow.md)).

## Licensing

Contributions are dual licensed under MIT and Apache-2.0, matching the project —
see [the README](README.md#license).
