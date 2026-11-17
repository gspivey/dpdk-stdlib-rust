//! Build script for dpdk-stdlib
//!
//! Mirrors the backend chosen by dpdk-stdlib-sys (real DPDK via bindgen, or
//! stubs) as a local `dpdk_stubs` cfg, so stub-only code — the EAL lifecycle
//! tests that call `dpdk_sys::stub_eal_*` — is compiled only when the stub
//! backend is active. dpdk-stdlib-sys publishes the choice through its
//! `links = "dpdk"` metadata, which Cargo exposes here as DEP_DPDK_BACKEND.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(dpdk_stubs)");
    println!("cargo:rerun-if-env-changed=DEP_DPDK_BACKEND");

    if std::env::var("DEP_DPDK_BACKEND").as_deref() == Ok("stubs") {
        println!("cargo:rustc-cfg=dpdk_stubs");
    }
}
