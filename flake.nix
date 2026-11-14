{
  description = "wok — GPU jobs from any box, jailed on the GPU machine";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        wok = pkgs.callPackage ./nix/package.nix { };
        default = self.packages.${pkgs.system}.wok;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            just
            bubblewrap
          ];
        };
      });

      homeManagerModules.default = import ./nix/hm-module.nix self;
    };
}
