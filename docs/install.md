# Installing the bridge

The bridge is a **Linux server daemon**. It is built, tested, and released for
**`x86_64-linux`** and **`aarch64-linux`** — there is no native macOS or Windows
build (on those it runs only inside a Linux container).

Pick whichever delivery method fits your host:

| Deployment method | x86_64-linux | aarch64-linux | Requires | Best for |
| --- | :---: | :---: | --- | --- |
| **NixOS module** (`nixosModules.jmap-bridge` + `overlays.default`) | ✅ | ✅ | Nix · NixOS | Declarative production deploy |
| **Nix package** (`nix build .#jmap-matrix-bridge`) | ✅ | ✅ | Nix | Nix on a non-NixOS Linux host |
| **Container image** (`ghcr.io/palebluebytes/jmap-matrix-bridge`) | ✅ | ✅ | Docker / Podman / k8s | Containerised / Kubernetes self-host |
| **Static binary** (release asset) | ✅ | ✅ | — *(no runtime deps)* | Any Linux host, no Nix or Docker |
| **From source** (`cargo build --release`) | ✅ | ✅ | Rust ≥ 1.85 · `pkg-config` · sqlite | Development |

Notes:

- The **container image** is a multi-arch manifest (`linux/amd64` + `linux/arm64`);
  on macOS/Windows it runs only through Docker's Linux VM, like any Linux container.
- The **static binary** is a fully static musl executable (no dynamic loader, no
  glibc) — drop it onto any Linux machine of the matching architecture and run it.
- CI runs `nix flake check` on **both** architectures, but the end-to-end
  Matrix↔email **VM round-trip test runs on `x86_64-linux` only** (`nixosTest` runs
  on the builder's platform); `aarch64-linux` gets build + clippy + rustfmt + the
  unit-test suite. See [ADR-0008](adr/0008-ci-and-release-flow.md).

Once installed, see [configuration.md](configuration.md) for the flags and the
[README](../README.md#configure--run) for the registration and first-run steps.

## NixOS module

The recommended production deploy. The flake exposes `overlays.default` and
`nixosModules.jmap-bridge`; the options are documented in
[`nix/module/README.md`](../nix/module/README.md).

## Nix package

```bash
nix build .#jmap-matrix-bridge
```

## Container image (Docker / Podman / k8s)

The image is public on `ghcr.io` — no login required.

```bash
# Pin a version (recommended for production) — or use :latest to track newest
docker pull ghcr.io/palebluebytes/jmap-matrix-bridge:v0.5.2
docker run --rm ghcr.io/palebluebytes/jmap-matrix-bridge:v0.5.2 run --help
```

It is a multi-arch manifest, so `docker`/`podman` auto-selects `linux/amd64` or
`linux/arm64`. `:latest` is mutable (overwritten each release); pin `:vX.Y.Z` (or a
digest) for reproducible or security-sensitive deployments.

In a container the homeserver usually connects from another host, so set
`LISTEN_ADDRESS=0.0.0.0` — the default `127.0.0.1` will not accept it.

## Static binary (no Nix or Docker)

Each tagged release (see
[Releases](https://github.com/palebluebytes/jmap-matrix-bridge/releases)) ships a
standalone static binary per architecture. Pick your tag and arch:

```bash
TAG=v0.5.2 ARCH=x86_64-linux
base="https://github.com/palebluebytes/jmap-matrix-bridge/releases/download/$TAG"
curl -fsSL -O "$base/jmap-matrix-bridge-$TAG-$ARCH"
curl -fsSL -O "$base/jmap-matrix-bridge-$TAG-$ARCH.sha256"
sha256sum -c "jmap-matrix-bridge-$TAG-$ARCH.sha256"   # integrity check
install -m755 "jmap-matrix-bridge-$TAG-$ARCH" jmap-matrix-bridge
```

## From source

```bash
cargo build --release
```

## Verify provenance (optional, recommended)

Both artifact types carry a keyless
[build-provenance attestation](https://docs.github.com/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds)
— proof the bytes were built by this repo's release workflow (the `.sha256` only
proves integrity). Verify with the [`gh` CLI](https://cli.github.com/):

```bash
# Binary
gh attestation verify jmap-matrix-bridge \
  --repo palebluebytes/jmap-matrix-bridge

# Container image (by tag or digest)
gh attestation verify oci://ghcr.io/palebluebytes/jmap-matrix-bridge:v0.5.2 \
  --repo palebluebytes/jmap-matrix-bridge
```
