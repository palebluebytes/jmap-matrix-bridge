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

Because this binary ingests Matrix `as`/`hs` tokens and a JMAP credential and holds the
AES-GCM key, the artifacts carry **build-provenance attestations** (`actions/attest-build-provenance`,
keyless via OIDC/Sigstore — no key to manage). The `.sha256` sidecar only proves *integrity*;
the attestation proves *authenticity* — that the bytes came from this repo/commit/workflow.
Operators verify with `gh attestation verify <binary> --repo palebluebytes/jmap-matrix-bridge`
or `gh attestation verify oci://ghcr.io/palebluebytes/jmap-matrix-bridge:vX.Y.Z`. The OCI
attestation is on the **multi-arch index digest** (what a tag pull resolves to). `latest`
stays mutable for convenience; **security-sensitive deployments should pin the immutable
`vX.Y.Z` (or a digest), not `latest`.** (SBOM publication is possible later but deferred.)

## Versioning policy

The version is **SemVer driven by Conventional Commit subjects**, owned end-to-end by
release-plz. Two rules make it predictable:

- **What "breaking" means for this artifact.** This is a self-hosted appservice *binary*,
  not a library, so there is no public API to break. "Breaking" is instead defined by the
  operator's upgrade experience: a change is breaking (mark it `feat!:` / `BREAKING CHANGE:`)
  when an operator **cannot just drop in the new binary** — they must change config, run a
  manual migration step, or accept altered Matrix state. Concretely: a **non-backward-safe DB
  migration** (the primary, most likely breaker), a renamed/removed **CLI flag or env var** or
  a changed **default** that alters behaviour, a change to the **registration/config contract**,
  a renamed/removed **NixOS module option**, or a change to **Matrix-visible room/thread/ghost
  layout for existing users**. Purely additive features are *not* breaking.
- **Pre-1.0, the minor is the breaking-change slot.** Following Cargo's SemVer convention
  (release-plz's default for `0.x`), pre-1.0 a breaking change bumps the **minor** (`0.x → 0.(x+1)`)
  and `feat:`/`fix:` bump the **patch**. So features accumulate as patches, and **a minor bump is
  the operator's one-glance "read the changelog / migration may be needed" signal** while a patch
  is always a safe drop-in. (At `1.0` this flips to standard SemVer: breaking→major, feat→minor.)

**The `Cargo.toml` version line is release-plz-owned — never hand-edit it.** The release job
runs on *every* push to `main` and cuts a release whenever it finds a version with no matching
tag; a manual bump in an ordinary PR would therefore **auto-ship** on merge (and break the
pre-1.0 patch/minor semantic above). Let the release PR own the bump.

**When to cut `1.0`.** Stay in `0.x` deliberately — it honestly signals "expect breakage, read
changelogs," and release-plz never leaves `0.x` on its own. Cut `1.0` only when all three hold:
(a) the **config/registration contract is settled** — the leading indicator is that *the minor
has stopped moving* (a long run of patch-only releases with no `0.x.0` bump means the
operator-facing contract has de-facto stabilised, visible at a glance in `CHANGELOG.md`);
(b) there is a real **DB migration + rollback story** — a migration test in the VM suite, a
documented downgrade procedure, and **at least one real schema migration survived in the wild**
with no data loss; and (c) there is **at least one operator other than the author** (an external
bug report, PR, downstream packaging, or non-author `ghcr` pulls). The `1.0.0` bump is then the
*one* sanctioned exception to the "never hand-edit the version" rule (or set it via release-plz
config so it still flows through the PR).

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
- **Unsigned artifacts / `.sha256` only (rejected)** — a checksum living on the same
  Release proves integrity but not authenticity; for a credential-handling binary that is
  too weak. Keyless OIDC/Sigstore provenance closes the gap with no key management, so it
  is worth the few extra workflow lines. (cosign with a managed key, and SBOM publication,
  were considered heavier and deferred.)
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
