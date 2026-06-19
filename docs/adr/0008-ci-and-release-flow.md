# CI is `nix flake check`; releases are on-demand via release-plz

The flake is the single source of build truth (see [AGENTS.md](../../AGENTS.md) §1),
so CI runs exactly one gate — `nix flake check` — on every pull request and push to
`main`, across `x86_64-linux` and `aarch64-linux`. The flake's `checks` were extended
so that one command covers everything: the release build, `clippy` (warnings denied),
`rustfmt --check`, the `cargo-nextest` unit suite, and the email↔Matrix round-trip VM
test (`nix/check/`, x86_64 only — `nixosTest` runs only on the builder's platform).
CI adds nothing the maintainer can't run locally with the same command.

Releases are automated with [release-plz](https://release-plz.dev). The commit history
already follows Conventional Commits, so on every push to `main` release-plz keeps a
"release vX.Y.Z" PR up to date — bumping `Cargo.toml` and regenerating `CHANGELOG.md`.
**Cutting a release is merging that PR**, which tags `vX.Y.Z` and creates the GitHub
Release. Cadence is therefore on-demand: the changelog is always ready, and a version
is cut whenever enough has accumulated. Pre-1.0, breaking changes bump the minor
(`0.x.0`) and feat/fix bump the patch (`0.x.y`).

The release tag fans out (`release-artifacts.yml`) to artifacts for non-Nix users: a
fully static (musl) binary per arch attached to the Release, and a multi-arch OCI image
on `ghcr.io`. Both are plain Nix builds (`.#static`, `.#dockerImage`) — the static link
is clean because TLS is rustls throughout (no openssl/native-tls in the tree) and sqlite
is bundled C.

## Considered Options

- **A bespoke `cargo` CI matrix (rejected)** — would duplicate the toolchain, system
  deps, and offline/sandbox setup the flake already pins, and drift from the local
  `nix flake check` developers actually run. The flake is the strict environment
  boundary; CI should honour it.
- **Manual `cargo release` / hand-written tags + changelog (rejected)** — error-prone
  and discards the value of the existing Conventional Commit history.
- **Publishing to crates.io (rejected for now)** — this is a self-hosted appservice
  binary, not a library; distribution is the Nix flake (overlay + NixOS module) plus
  GitHub Releases and the container image. `release-plz.toml` sets `publish = false`.
- **`nix flake check` gate + release-plz on-demand PR + Nix-built static/OCI artifacts
  (chosen)** — one gate, an always-ready changelog, reproducible artifacts, no extra
  toolchains.

## Consequences

- **`main` must stay `nix flake check`-green**, including `clippy` (warnings denied)
  and `rustfmt` — the tree was brought to a clean `cargo fmt` + clippy baseline when
  these gates were introduced (the lints in `Cargo.toml` were previously unenforced).
  Run `just lint` (or `nix flake check`) before pushing.
- **Two one-time secrets are required** and cannot live in the repo (set them as GitHub
  Actions repository secrets — a `.env` file is *not* read by Actions): a Cachix cache +
  `CACHIX_AUTH_TOKEN` (CI would otherwise rebuild the matrix-sdk closure every run); and a
  `RELEASE_PLZ_TOKEN` PAT/App token used by both `release-plz` and `update-flake-lock` so
  their PRs trigger CI and the pushed release tag triggers the artifact build (the default
  `GITHUB_TOKEN` triggers none of these). Because every PR-opening workflow uses that PAT,
  the org/repo "Allow GitHub Actions to create … pull requests" setting is **not** needed
  (it only governs the default token). One non-secret step remains: make the package
  public on the first `ghcr.io` push. These are listed in the CI/Releases section of
  `AGENTS.md`.
- **The VM round-trip test runs on every PR.** It is the slow part; with Cachix the
  bridge build is warm and only the VM boot remains. If PR latency becomes a problem,
  gate the VM check to `push: main` rather than dropping it.
