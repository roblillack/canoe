{
  lib,
  rustPlatform,
  pkg-config,
  llvmPackages,
  fontconfig,
  freetype,
  libxkbcommon,
  wayland,
}:

let
  cargoToml = lib.importTOML ./Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "canoe";
  inherit (cargoToml.package) version;

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    llvmPackages.clang
  ];

  buildInputs = [
    fontconfig
    freetype
    libxkbcommon
    wayland
  ];

  LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";

  meta = {
    description = "Stacking window manager for the River Wayland compositor";
    homepage = cargoToml.package.homepage;
    changelog = "https://github.com/roblillack/canoe/blob/main/CHANGELOG.md";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "canoe";
  };
}
