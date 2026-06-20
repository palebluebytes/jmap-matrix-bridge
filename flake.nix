{
  description = "JMAP ↔ Matrix bridge — your email account as Matrix DMs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    sops-nix = {
      url = "github:Mic92/sops-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      flake = {
        # Adds `pkgs.jmap-matrix-bridge` to a consumer's nixpkgs. The NixOS
        # module's `package` default (pkgs.jmap-matrix-bridge) resolves through
        # this, so a consumer needs only the overlay + the module.
        overlays.default = final: _prev: {
          jmap-matrix-bridge = final.callPackage ./. {
            craneLib = inputs.crane.mkLib final;
          };
        };

        # Host-agnostic systemd/packaging module (services.jmap-bridge.*).
        nixosModules.jmap-bridge = ./nix/module;
      };

      perSystem =
        {
          pkgs,
          system,
          lib,
          ...
        }:
        let
          craneLib = inputs.crane.mkLib pkgs;
          bridge = pkgs.callPackage ./. { inherit craneLib; };

          # Fully static (musl) build for the release binaries — a standalone
          # binary that runs without Nix, Docker, or a host glibc. We cross to
          # musl (NOT pkgsStatic, which force-statically-links the build-host
          # proc-macros/build-scripts against glibc and fails): under a musl
          # cross, build scripts build for the host while the final binary targets
          # musl, which defaults to crt-static. TLS is rustls throughout (no
          # openssl/native-tls in the tree) and sqlite is bundled C, so it links
          # cleanly. `pkgsCross.musl64` is x86_64-specific, so pick the matching
          # musl cross per system.
          muslPkgs =
            {
              "x86_64-linux" = pkgs.pkgsCross.musl64;
              "aarch64-linux" = pkgs.pkgsCross.aarch64-multiplatform-musl;
            }
            .${system};
          staticBridge = muslPkgs.callPackage ./. {
            craneLib = inputs.crane.mkLib muslPkgs;
            crtStatic = true;
          };

          # Minimal OCI image for non-Nix self-hosters (docker/podman/k8s). Wraps
          # the regular (dynamic) package — dockerTools bundles its Nix closure
          # into the image layers, so no static build is needed here. `:latest` is
          # a placeholder; the release workflow retags by version and pushes to
          # ghcr.io. cacert supplies the trust store for outbound JMAP/Matrix TLS.
          # See ADR-0008.
          dockerImage = pkgs.dockerTools.buildLayeredImage {
            name = "jmap-matrix-bridge";
            tag = "latest";
            contents = [ pkgs.cacert ];
            config = {
              Entrypoint = [ "${bridge}/bin/jmap-matrix-bridge" ];
              ExposedPorts."8008/tcp" = { };
              Env = [ "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt" ];
            };
          };
        in
        {
          packages.jmap-matrix-bridge = bridge;
          packages.default = bridge;
          packages.static = staticBridge;
          packages.dockerImage = dockerImage;

          # Dev shell entered via `nix develop` / `direnv` (see .envrc). Inherits
          # the package's build inputs + Rust toolchain from crane, then layers on
          # the tooling AGENTS.md documents plus `gh` for issue-tracker workflows.
          devShells.default = craneLib.devShell {
            inputsFrom = [ bridge ];

            # Use the fast `mold` linker for interactive dev builds. Set in the
            # shell ENV rather than committed to `.cargo/config.toml` on purpose:
            # a committed `-fuse-ld=mold` is read by every raw `cargo` (CI,
            # release-plz's `cargo package`) and fails wherever mold is absent.
            # Scoping it to the dev shell keeps mold a dev-only concern; the
            # hermetic package build is unaffected (it strips `.cargo`).
            CARGO_BUILD_RUSTFLAGS = "-C link-arg=-fuse-ld=mold";

            packages = with pkgs; [
              cargo-nextest
              bacon
              just
              gh
              mold
            ];
          };

          # The authoritative gate (`nix flake check`): build + clippy + fmt +
          # unit tests (from the package's passthru, both systems) plus the
          # email↔Matrix round-trip VM test. nixosTest runs only on the builder's
          # platform, so gate the VM check to x86_64 (the bridge's host).
          checks =
            bridge.passthru.checks
            // lib.optionalAttrs (system == "x86_64-linux") {
              jmap-bridge = import ./nix/check {
                inherit pkgs inputs;
                self = inputs.self;
                homeserver = "tuwunel";
              };
            };
        };
    };
}
