{
  lib,
  rustPlatform,
  pkg-config,
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

  nativeBuildInputs = [ pkg-config ];

  buildInputs = [
    fontconfig
    freetype
    libxkbcommon
    wayland
  ];

  # Point the session file at this build of Canoe, so display managers can
  # start it without Canoe being on their PATH. River is still looked up on
  # PATH, like other compositors' session files do: it comes from the system
  # (NixOS, the distribution, or a Nix profile with working graphics drivers).
  postInstall = ''
    install -Dm644 contrib/canoe.desktop $out/share/wayland-sessions/canoe.desktop
    substituteInPlace $out/share/wayland-sessions/canoe.desktop \
      --replace-fail "Exec=river -c canoe" "Exec=river -c $out/bin/canoe"
  '';

  passthru.providedSessions = [ "canoe" ];

  meta = {
    description = "Stacking window manager for the River Wayland compositor";
    homepage = cargoToml.package.homepage;
    changelog = "https://github.com/roblillack/canoe/blob/main/CHANGELOG.md";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "canoe";
  };
}
