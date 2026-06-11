fn main() {
    // build.rs runs on the HOST: #[cfg(target_os)] here picks the host OS
    // and leaks -install_name into cross-builds (mac -> windows-gnu fails
    // to link). Dispatch on the TARGET instead.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let major = std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap();
    let minor = std::env::var("CARGO_PKG_VERSION_MINOR").unwrap();
    match target_os.as_str() {
        "linux" => println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libkrun.so.{major}"),
        "macos" => println!(
            "cargo:rustc-cdylib-link-arg=-Wl,-install_name,libkrun.{major}.dylib,-compatibility_version,{major}.0.0,-current_version,{major}.{minor}.0"
        ),
        _ => {}
    }
}
