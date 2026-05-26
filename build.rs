fn main() {
    // Just rebuild when protocols change
    println!("cargo:rerun-if-changed=protocol/river-window-management-v1.xml");
    println!("cargo:rerun-if-changed=protocol/river-xkb-bindings-v1.xml");
    println!("cargo:rerun-if-changed=protocol/river-layer-shell-v1.xml");
    println!("cargo:rerun-if-changed=protocol/river-input-management-v1.xml");
    println!("cargo:rerun-if-changed=protocol/river-libinput-config-v1.xml");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "openbsd") {
        println!("cargo:rustc-link-search=native=/usr/local/lib");
    }
}
