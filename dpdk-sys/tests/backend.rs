//! Guards against a silent fallback to the stub backend.
//!
//! build.rs falls back to stubs (with only a cargo warning) when DPDK is not
//! found, so a CI job meant to compile the real-DPDK path could pass without
//! ever running bindgen. CI sets DPDK_STDLIB_EXPECT_BACKEND to the backend it
//! expects; when unset, the test only checks that exactly one backend is active.

#[test]
fn active_backend_matches_expectation() {
    assert_ne!(
        dpdk_sys::is_real_dpdk(),
        dpdk_sys::is_stub(),
        "exactly one of the bindgen and stub backends must be active"
    );

    match std::env::var("DPDK_STDLIB_EXPECT_BACKEND").as_deref() {
        Ok("bindgen") => assert!(
            dpdk_sys::is_real_dpdk(),
            "expected the real DPDK (bindgen) backend, but dpdk-stdlib-sys built its stubs; \
             check that pkg-config can find libdpdk and the `bindgen` feature is enabled"
        ),
        Ok("stubs") => assert!(dpdk_sys::is_stub(), "expected the stub backend"),
        Ok(other) => panic!("unknown DPDK_STDLIB_EXPECT_BACKEND value: {other}"),
        Err(_) => {}
    }
}
