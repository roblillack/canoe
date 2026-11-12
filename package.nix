{
  lib,
  rustPlatform,
  pkg-config,
  makeBinaryWrapper,
  fontconfig,
  freetype,
  libdrm,
  libgbm,
  libGL,
  libinput,
  libxkbcommon,
  pixman,
  seatd,
  udev,
  wayland,
  libX11,
  libXcursor,
  libXi,
  libXrandr,
  xwayland,
  withXwayland ? true,
}:

let
  cargoToml = lib.importTOML ./Cargo.toml;

  # Loaded with dlopen rather than linked: EGL (for every backend), and
  # what the nested winit backend needs inside a Wayland or X11 session.
  runtimeLibraries = [
    libGL
    libxkbcommon
    wayland
    libX11
    libXcursor
    libXi
    libXrandr
  ];
in
rustPlatform.buildRustPackage {
  pname = "canoe";
  inherit (cargoToml.package) version;

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    makeBinaryWrapper
  ];

  buildInputs = [
    fontconfig
    freetype
    libdrm
    libgbm
    libinput
    libxkbcommon
    pixman
    seatd
    udev
    wayland
  ];

  # Point the session file at this build of Canoe, so display managers can
  # start it without Canoe being on their PATH, and install the portal
  # routing for the "canoe" desktop next to it.
  postInstall = ''
    install -Dm644 contrib/canoe.desktop $out/share/wayland-sessions/canoe.desktop
    substituteInPlace $out/share/wayland-sessions/canoe.desktop \
      --replace-fail "Exec=canoe" "Exec=$out/bin/canoe"
    install -Dm644 contrib/canoe-portals.conf $out/share/xdg-desktop-portal/canoe-portals.conf
  '';

  # Xwayland is looked up on PATH; one installed on the system comes first.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibraries} $out/bin/canoe
  ''
  + lib.optionalString withXwayland ''
    wrapProgram $out/bin/canoe --suffix PATH : ${lib.makeBinPath [ xwayland ]}
  '';

  passthru = {
    providedSessions = [ "canoe" ];
    inherit runtimeLibraries;
  };

  meta = {
    description = "Stacking Wayland compositor with Win 3.x looks";
    homepage = cargoToml.package.homepage;
    changelog = "https://github.com/roblillack/canoe/blob/main/CHANGELOG.md";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "canoe";
  };
}
