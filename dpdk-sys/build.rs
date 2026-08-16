//! Build script for dpdk-sys
//!
//! This build script handles two modes:
//! 1. With DPDK installed + bindgen feature: Generates real FFI bindings
//! 2. Without DPDK / without bindgen feature: Uses stub implementations

use std::env;
use std::path::PathBuf;

fn main() {
    // Declare our custom cfg flags
    println!("cargo::rustc-check-cfg=cfg(dpdk_bindgen)");
    println!("cargo::rustc-check-cfg=cfg(dpdk_stubs)");

    println!("cargo:rerun-if-env-changed=DPDK_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    // Try to find DPDK
    let dpdk_found = try_find_dpdk();

    if dpdk_found && cfg!(feature = "bindgen") {
        println!("cargo:rustc-cfg=dpdk_bindgen");
        #[cfg(feature = "bindgen")]
        generate_bindings();
        compile_shim();
    } else {
        println!("cargo:rustc-cfg=dpdk_stubs");
        if !dpdk_found {
            println!("cargo:warning=DPDK not found, using stub implementations");
        } else {
            println!("cargo:warning=bindgen feature not enabled, using stub implementations");
        }
    }
}

/// Try to find DPDK installation via pkg-config or environment variable
fn try_find_dpdk() -> bool {
    // First, check for DPDK_PATH environment variable
    if let Ok(dpdk_path) = env::var("DPDK_PATH") {
        let lib_path = PathBuf::from(&dpdk_path).join("lib");
        let lib64_path = PathBuf::from(&dpdk_path).join("lib64");
        let include_path = PathBuf::from(&dpdk_path).join("include");

        if include_path.exists() {
            println!("cargo:rustc-link-search=native={}", lib_path.display());
            println!("cargo:rustc-link-search=native={}", lib64_path.display());
            println!("cargo:include={}", include_path.display());

            // Link common DPDK libraries
            link_dpdk_libraries();
            return true;
        }
    }

    // Try pkg-config
    match pkg_config::Config::new()
        .atleast_version("21.0")
        .probe("libdpdk")
    {
        Ok(lib) => {
            for path in &lib.include_paths {
                println!("cargo:include={}", path.display());
            }
            true
        }
        Err(_) => false,
    }
}

/// Link DPDK libraries
fn link_dpdk_libraries() {
    // Core DPDK libraries
    let libs = [
        "rte_eal",
        "rte_mempool",
        "rte_mbuf",
        "rte_ring",
        "rte_ethdev",
        "rte_net",
        "rte_kvargs",
        "rte_telemetry",
        "rte_hash",
        "rte_timer",
        "rte_cmdline",
    ];

    for lib in &libs {
        println!("cargo:rustc-link-lib={}", lib);
    }

    // System libraries that DPDK depends on
    println!("cargo:rustc-link-lib=numa");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=dl");
}

/// Compile the C shim that wraps static inline DPDK functions.
/// Called only when real DPDK is present (dpdk_bindgen mode).
fn compile_shim() {
    let mut build = cc::Build::new();
    build.file("csrc/dpdk_shim.c");

    // Feed the same include paths that bindgen uses
    let common_paths = [
        "/usr/include/dpdk",
        "/usr/local/include/dpdk",
        "/opt/dpdk/include",
        "/usr/include/aarch64-linux-gnu/dpdk",
        "/usr/include/x86_64-linux-gnu/dpdk",
    ];
    for path in &common_paths {
        if PathBuf::from(path).exists() {
            build.include(path);
        }
    }

    // pkg-config discovery for include paths
    if let Ok(lib) = pkg_config::Config::new().atleast_version("21.0").probe("libdpdk") {
        for path in &lib.include_paths {
            build.include(path);
        }
    }

    // Also honour DPDK_PATH
    if let Ok(dpdk_path) = env::var("DPDK_PATH") {
        let include_path = PathBuf::from(&dpdk_path).join("include");
        if include_path.exists() {
            build.include(&include_path);
        }
    }

    // DPDK headers on x86_64 include SSE/AVX intrinsics (e.g. _mm_alignr_epi8)
    // that require at least -mssse3. Use -march=native so the compiler enables
    // whichever extensions the build host supports.
    if cfg!(target_arch = "x86_64") {
        build.flag("-march=native");
    }

    build.compile("dpdk_shim");
    println!("cargo:rerun-if-changed=csrc/dpdk_shim.c");
}

#[cfg(feature = "bindgen")]
fn generate_bindings() {
    use std::fs;

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Get include paths from cargo
    let include_paths: Vec<PathBuf> = env::var("DEP_DPDK_INCLUDE")
        .map(|s| s.split(':').map(PathBuf::from).collect())
        .unwrap_or_default();

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // Allow DPDK types and functions
        .allowlist_function("rte_.*")
        .allowlist_type("rte_.*")
        .allowlist_var("RTE_.*")
        // Block problematic types
        .blocklist_type("max_align_t")
        // Fix E0588: packed structs that transitively contain #[repr(align)] types.
        // These are unused in our code — making them opaque turns them into [u8; N]
        // which resolves the packed/aligned conflict.
        // Note: opaque types still get #[repr(align)] from bindgen, so we must
        // also make any packed container structs opaque.
        .opaque_type("rte_arp_ipv4")               // packed, contains aligned rte_ether_addr
        .opaque_type("rte_arp_hdr")                 // packed, contains aligned rte_arp_ipv4
        .opaque_type("rte_l2tpv2_combined_msg_hdr") // packed, contains aligned L2TP inner type
        // Fix E0277: rte_gtp_psc_generic_hdr is missing Debug but is used in a
        // derive(Debug) container (rte_flow_item_gtp_psc). Making it opaque gives
        // it a byte-array body that auto-derives Debug.
        .opaque_type("rte_gtp_psc_generic_hdr")
        // Generate derives
        .derive_default(true)
        .derive_debug(true)
        // Layout tests can be noisy
        .layout_tests(false)
        // Use core instead of std where possible
        .use_core()
        .ctypes_prefix("libc");

    // Add include paths
    for path in &include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    // Add common DPDK include paths
    let common_paths = [
        "/usr/include/dpdk",
        "/usr/local/include/dpdk",
        "/opt/dpdk/include",
        "/usr/include/aarch64-linux-gnu/dpdk",
        "/usr/include/x86_64-linux-gnu/dpdk",
        "/usr/include/aarch64-linux-gnu",
    ];

    for path in &common_paths {
        if PathBuf::from(path).exists() {
            builder = builder.clang_arg(format!("-I{}", path));
        }
    }

    // pkg-config discovery for clang include paths
    if let Ok(lib) = pkg_config::Config::new().atleast_version("21.0").probe("libdpdk") {
        for path in &lib.include_paths {
            builder = builder.clang_arg(format!("-I{}", path.display()));
        }
    }

    let bindings = builder
        .generate()
        .expect("Unable to generate DPDK bindings");

    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
