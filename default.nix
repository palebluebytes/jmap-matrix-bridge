{
  lib,
  craneLib,
  pkg-config,
  sqlite,
  openssl,
  cargo-nextest,
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
  };

  # Build only the cargo dependencies (fully cached by Nix)
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  # Build the production package (disable check phase to make installs/deploys instant!)
  package = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;
      doCheck = false;
    }
  );

  # Build and run the test suite separately in the sandbox using cargo-nextest
  checks = craneLib.cargoNextest (
    commonArgs
    // {
      inherit cargoArtifacts;
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ cargo-nextest ];

      # We skip external network integrations and specify individual exclusions with nextest filter DSL
      cargoNextestExtraArgs = "--filter 'not (test(test_matrix_login_payload) or test(test_sender_flow) or test(test_multi_user_login_integration) or test(test_poll_hits_jmap_and_matrix_endpoints))'";
    }
  );

in
# Return the package with nextest checks bound via passthru
package.overrideAttrs (oldAttrs: {
  passthru = (oldAttrs.passthru or { }) // {
    inherit checks;
  };
})
