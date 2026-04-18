//! Build script for dpdk crate
//!
//! Propagates the dpdk_bindgen / dpdk_stubs cfg flags from dpdk-sys so that
//! downstream code in this crate can use `#[cfg(dpdk_bindgen)]` to handle
//! struct layout differences between real bindgen output and stub types.

fn main() {
    // Declare the cfg flags so rustc doesn't warn about unexpected cfgs
    println!("cargo::rustc-check-cfg=cfg(dpdk_bindgen)");
    println!("cargo::rustc-check-cfg=cfg(dpdk_stubs)");

    // Detect whether real DPDK is available via pkg-config
    let dpdk_found = pkg_config::Config::new()
        .atleast_version("21.0")
        .probe("libdpdk")
        .is_ok();

    if dpdk_found {
        println!("cargo:rustc-cfg=dpdk_bindgen");
    } else {
        println!("cargo:rustc-cfg=dpdk_stubs");
    }
}
