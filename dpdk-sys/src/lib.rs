//! Raw FFI bindings to DPDK (Data Plane Development Kit)
//!
//! This crate provides low-level bindings to DPDK. It operates in two modes:
//!
//! ## With DPDK installed (bindgen feature)
//!
//! When compiled with the `bindgen` feature and DPDK is installed, this crate
//! generates real FFI bindings using bindgen:
//!
//! ```toml
//! [dependencies]
//! dpdk-sys = { version = "0.1", features = ["bindgen"] }
//! ```
//!
//! ## Without DPDK (default)
//!
//! By default, this crate provides stub implementations that allow development
//! and testing without requiring DPDK to be installed. The stubs provide the
//! same API but don't perform actual packet I/O.
//!
//! ## Environment Variables
//!
//! - `DPDK_PATH`: Path to DPDK installation (e.g., `/usr/local/dpdk`)
//! - `PKG_CONFIG_PATH`: Can be set to help pkg-config find DPDK

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

// When bindgen generates real bindings, include them
#[cfg(dpdk_bindgen)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// When using stubs (default), use our manual definitions
#[cfg(dpdk_stubs)]
mod stubs;

#[cfg(dpdk_stubs)]
pub use stubs::*;

// Re-export libc types commonly used with DPDK
pub use libc::{c_char, c_int, c_uint, c_void, size_t, ssize_t};

/// Check if real DPDK bindings are being used
#[inline]
pub const fn is_real_dpdk() -> bool {
    cfg!(dpdk_bindgen)
}

/// Check if stub implementations are being used
#[inline]
pub const fn is_stub() -> bool {
    cfg!(dpdk_stubs)
}
