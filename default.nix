{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
  sqlite,
}:

rustPlatform.buildRustPackage {
  pname = "jmap-matrix-bridge";
  version = "0.1.0";

  src = ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
  };

  nativeBuildInputs = [
    pkg-config
  ];

  buildInputs = [
    openssl
    sqlite
  ];

  meta = with lib; {
    description = "JMAP to Matrix Bridge";
    homepage = "https://github.com/palebluebytes/jmap-matrix-bridge";
    license = licenses.mit;
    maintainers = with maintainers; [ inkpotmonkey ];
    mainProgram = "jmap-matrix-bridge";
  };
}
