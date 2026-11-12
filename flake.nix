{
  description = "Canoe, a stacking Wayland compositor with Win 3.x looks";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f: nixpkgs.lib.genAttrs supportedSystems (system: f (import nixpkgs { inherit system; }));
    in
    {
      overlays.default = final: prev: {
        canoe = final.callPackage ./package.nix { };
      };

      packages = forAllSystems (pkgs: {
        default = pkgs.callPackage ./package.nix { };
      });

      devShells = forAllSystems (
        pkgs:
        let
          canoe = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ canoe ];

            # cargo-built binaries don't get the package's rpath for the
            # libraries Canoe loads at runtime (EGL, and winit's).
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath canoe.runtimeLibraries;

            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
            ];

            shellHook = ''
              echo "Canoe development environment loaded"
            '';
          };
        }
      );
    };
}
