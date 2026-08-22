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

rustPlatform.buildRustPackage {
  pname = "canoe";
  version = (lib.importTOML ./Cargo.toml).package.version;

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

  meta = with lib; {
    description = "A configuration tool for the River Wayland compositor";
    homepage = "https://github.com/roblillack/canoe";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "canoe";
  };
}
