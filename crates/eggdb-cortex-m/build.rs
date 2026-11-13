fn main() {
    let directory = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    println!("cargo:rustc-link-search={directory}");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rerun-if-changed=link.x");
}
