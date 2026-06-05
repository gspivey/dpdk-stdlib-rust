//! DPDK-accelerated TCP stack
//!
//! This crate provides a drop-in replacement for `std::net::TcpStream` and
//! `std::net::TcpListener` using DPDK userspace networking.
//!
//! Depends on `dpdk-stdlib-net` for `PacketBackend` — does NOT depend on `dpdk-udp`.
