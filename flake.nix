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
        in
        {
          packages.jmap-matrix-bridge = bridge;
          packages.default = bridge;

          # Dev shell entered via `nix develop` / `direnv` (see .envrc). Inherits
          # the package's build inputs + Rust toolchain from crane, then layers on
          # the tooling AGENTS.md documents plus `gh` for issue-tracker workflows.
          devShells.default = craneLib.devShell {
            inputsFrom = [ bridge ];
            packages = with pkgs; [
              cargo-nextest
              bacon
              just
              gh
            ];
          };

          # The email↔Matrix round-trip VM test. nixosTest runs only on the
          # builder's platform, so gate it to x86_64 (the bridge's host).
          checks = lib.optionalAttrs (system == "x86_64-linux") {
            jmap-bridge = import ./nix/check {
              inherit pkgs inputs;
              self = inputs.self;
              homeserver = "tuwunel";
            };
          };
        };
    };
}
