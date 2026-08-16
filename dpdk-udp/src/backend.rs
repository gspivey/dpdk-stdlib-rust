//! Abstract backend trait for packet I/O — re-exported from `dpdk-stdlib-net`.
//!
//! This module re-exports the `PacketBackend` trait, `BackendConfig`, and
//! `BackendType` from the shared `dpdk-stdlib-net` crate for backward
//! compatibility.

pub use dpdk_stdlib_net::backend::{BackendConfig, BackendType, PacketBackend, RxReadiness};
