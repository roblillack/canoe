fn main() {
    println!("cargo:rustc-check-cfg=cfg(drm_backend)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // The udev/DRM/KMS session backend compiles on Linux and on OpenBSD
    // (via libudev-openbsd + seatd + libinput-openbsd + mesa). Other
    // platforms get only the winit and headless backends.
    if matches!(target_os.as_str(), "linux" | "openbsd") {
        println!("cargo:rustc-cfg=drm_backend");
    }
}
