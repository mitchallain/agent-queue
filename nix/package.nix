{
  lib,
  rustPlatform,
}:
let
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  meta = {
    description = cargoToml.package.description or "TUI and CLI queue manager for tmux-hosted AI coding agent sessions";
    mainProgram = "agent-queue";
    platforms = lib.platforms.unix;
  };
}
