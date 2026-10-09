{
  description = "Unofficial Proton Drive client for Linux: files-on-demand FUSE mount, CLI, GTK4 app and tray";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

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
      packages = forAllSystems (pkgs: rec {
        proton-drive-linux = pkgs.callPackage ./packaging/nix/package.nix { };
        default = proton-drive-linux;
      });

      overlays.default = final: _: {
        proton-drive-linux = final.callPackage ./packaging/nix/package.nix { };
      };

      nixosModules.default = import ./packaging/nix/module.nix self;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.proton-drive-linux ];
          packages = with pkgs; [
            cargo
            clippy
            rustfmt
            rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
