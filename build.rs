fn main() {
    println!("cargo:rustc-check-cfg=cfg(drm_backend)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // The udev/DRM/KMS session backend is always built on Linux. On OpenBSD
    // (libudev-openbsd + seatd + libinput-openbsd + mesa) it needs a patched
    // smithay, so it is opt-in through the `openbsd-drm` feature. Other
    // platforms get only the winit and headless backends.
    let openbsd_drm = std::env::var_os("CARGO_FEATURE_OPENBSD_DRM").is_some();
    if target_os == "linux" || (target_os == "openbsd" && openbsd_drm) {
        println!("cargo:rustc-cfg=drm_backend");
    }
}
