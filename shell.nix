{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    # Rust toolchain
    rustc
    cargo
    clippy
    rustfmt

    # Build tools
    pkg-config
    clang

    # Native dependencies
    wayland
    libxkbcommon
    freetype
    fontconfig
  ];

  # Use clang as the C compiler
  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  CC = "clang";

  shellHook = ''
    echo "Canoe development environment loaded"
  '';
}
