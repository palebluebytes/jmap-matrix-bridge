{ pkgs, ... }:

let
  bridge = pkgs.callPackage ../default.nix { };
in
pkgs.testers.testVersion {
  package = bridge;
  command = "jmap-matrix-bridge --help";
  version = "0.1.0";
}
