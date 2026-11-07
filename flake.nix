{
  description = "A configuration tool for the River Wayland compositor";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs supportedSystems (
          system:
          f (import nixpkgs { inherit system; })
        );
    in
    {
      overlays.default = final: prev: {
        canoe = final.callPackage ./package.nix { };
      };

      packages = forAllSystems (pkgs: {
        default = pkgs.callPackage ./package.nix { };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.system}.default ];

          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
          ];

          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

          shellHook = ''
            echo "Canoe development environment loaded"
          '';
        };
      });
    };
}
