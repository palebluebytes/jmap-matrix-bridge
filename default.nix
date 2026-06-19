{
  lib,
  craneLib,
  pkg-config,
  sqlite,
  openssl,
  cargo-nextest,
  cacert,
  # When true (the release `static` build), statically link musl libc + libgcc so
  # the binary is a portable standalone executable. A plain musl cross only links
  # *against* musl and leaves a PT_INTERP pointing at the Nix-store ld-musl, which
  # is useless off-Nix. Must apply to BOTH the deps cache and the final crate, so
  # it lives in commonArgs. See ADR-0008.
  crtStatic ? false,
}:

let
  commonArgs = {
    src = lib.cleanSourceWith {
      src = ./.;
      filter =
        path: type:
        let
          base = baseNameOf path;
        in
        !(type == "directory" && base == ".cargo")
        && (
          (craneLib.filterCargoSources path type)
          || (lib.hasSuffix ".png" path)
          || (lib.hasSuffix ".sql" path)
        );
    };
    strictDeps = true;

    nativeBuildInputs = [
      pkg-config
    ];

    buildInputs = [
      sqlite
      openssl
    ];

    meta = with lib; {
      description = "JMAP to Matrix Bridge";
      homepage = "https://github.com/palebluebytes/jmap-matrix-bridge";
      license = licenses.mit;
      maintainers = with maintainers; [ inkpotmonkey ];
      mainProgram = "jmap-matrix-bridge";
    };
  }
  // lib.optionalAttrs crtStatic {
    CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
  };

  # Build only the cargo dependencies (fully cached by Nix). Release profile —
  # this feeds the production `package` (and thus the VM check).
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  # Separate dev-profile dependency build for the test suite. The tests don't
  # need release optimization (the LLVM opt passes are what make a release build
  # of the matrix-sdk/sqlx tree so slow), and compiled artifacts are NOT shared
  # across profiles — so the test build gets its own unoptimized deps cache that
  # compiles far faster and is reused on every subsequent `nix build …checks`.
  cargoArtifactsDev = craneLib.buildDepsOnly (
    commonArgs
    // {
      CARGO_PROFILE = "dev";
    }
  );

  # Build the production package (disable check phase to make installs/deploys instant!)
  package = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;
      doCheck = false;
    }
  );

  # Build and run the test suite separately in the sandbox using cargo-nextest.
  # Compiled with the dev profile (off release) for fast feedback — see
  # cargoArtifactsDev above.
  cargoNextest = craneLib.cargoNextest (
    commonArgs
    // {
      cargoArtifacts = cargoArtifactsDev;
      CARGO_PROFILE = "dev";
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ cargo-nextest ];

      # The JMAP client (reqwest) eagerly loads system CA roots when it builds,
      # even for plain-HTTP wiremock targets on 127.0.0.1. The hermetic sandbox
      # has no trust store, so `ClientBuilder::build()` fails with "No CA
      # certificates were loaded from the system". Point it at the cacert bundle
      # so these self-contained tests (full bridge cycle, backfill) can run.
      SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";

      # We skip external network integrations and specify individual exclusions with the
      # nextest filterset DSL (-E / --filterset; the old `--filter` flag was removed upstream).
      cargoNextestExtraArgs = "-E 'not (test(test_matrix_login_payload) or test(test_sender_flow) or test(test_multi_user_login_integration) or test(test_poll_hits_jmap_and_matrix_endpoints))'";
    }
  );

  # Clippy gate. Mirrors `cargo clippy-all` (.cargo/config.toml): all targets,
  # all features, warnings denied — so the lint groups in Cargo.toml
  # (all/pedantic/nursery/cargo) become hard build failures in CI. Reuses the
  # dev-profile dependency cache for fast, cached runs.
  cargoClippy = craneLib.cargoClippy (
    commonArgs
    // {
      cargoArtifacts = cargoArtifactsDev;
      CARGO_PROFILE = "dev";
      cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings";
    }
  );

  # Formatting gate: `cargo fmt --all -- --check`. Only needs the sources.
  cargoFmt = craneLib.cargoFmt { inherit (commonArgs) src; };

  # All non-VM checks, surfaced to the flake so a single `nix flake check`
  # runs build + clippy + fmt + unit tests (the VM round-trip is added in
  # flake.nix, gated to x86_64). See ADR-0008.
  checks = {
    inherit cargoNextest cargoClippy cargoFmt;
  };

in
# Return the package with the check derivations bound via passthru.
package.overrideAttrs (oldAttrs: {
  passthru = (oldAttrs.passthru or { }) // {
    inherit checks;
  };
})
