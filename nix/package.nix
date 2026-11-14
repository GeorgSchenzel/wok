{ lib, rustPlatform }:

# One binary: the client (`wok run`, `wok status`, a bento tool in boxes) and
# the daemon (`wok daemon`). The daemon's jail tools (bwrap, sway, grim) come
# from its unit (hm-module.nix), so the box closure stays small.
rustPlatform.buildRustPackage {
  pname = "wok";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tests
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  meta = {
    description = "GPU jobs from any box, jailed on the GPU machine";
    mainProgram = "wok";
    platforms = lib.platforms.linux;
  };
}
