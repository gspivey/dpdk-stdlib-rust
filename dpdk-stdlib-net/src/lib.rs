//! Shared networking traits and backends for dpdk-stdlib crates
//!
//! This crate provides the `PacketBackend` trait and concrete implementations
//! (DPDK, AF_PACKET, AF_PACKET+MMAP) shared by `dpdk-stdlib-udp` and
//! `dpdk-stdlib-tcp`. It also provides checksum helpers and the
//! `NeighborResolver` abstraction for MAC address resolution.

use std::io;
use std::sync::Arc;

/// Data room size for jumbo-frame-capable mbufs (9KB + RTE_PKTMBUF_HEADROOM).
/// ENA always supports 9001 MTU; oversized mbufs don't hurt small packets.
pub(crate) const JUMBO_DATA_ROOM_SIZE: u16 = 9216 + 128;

pub mod backend;
pub mod backend_dpdk;
pub mod backend_raw;
pub mod ring_buffer;
pub mod checksum;
pub mod neighbor;

pub use backend::{BackendConfig, BackendType, PacketBackend, RxReadiness};
pub use backend_dpdk::DpdkBackend;
pub use backend_raw::RawSocketBackend;
pub use checksum::{ipv4_checksum, udp_pseudo_header_checksum};
pub use neighbor::{ArpResolver, NeighborResolver};

/// Create a backend from configuration.
///
/// Tries backends in order based on `BackendType`:
/// - `Dpdk` — DPDK userspace networking
/// - `RawSocketMmap` — AF_PACKET + PACKET_MMAP
/// - `RawSocket` — AF_PACKET basic
/// - `Auto` — tries DPDK first, then mmap, then basic raw socket
pub fn create_backend(config: &BackendConfig) -> io::Result<Arc<dyn PacketBackend>> {
    match config.backend_type {
        BackendType::Dpdk => {
            let backend = DpdkBackend::new(config.dpdk_port_id)?;
            Ok(Arc::new(backend))
        }
        BackendType::RawSocketMmap => {
            let iface = config
                .interface_name
                .as_deref()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Interface name required for raw socket backend",
                    )
                })?;
            let ring_config = ring_buffer::RingConfig {
                frame_size: config.ring_frame_size,
                frame_count: config.ring_frame_count,
            };
            let backend = RawSocketBackend::with_mmap(iface, true, &ring_config)?;
            Ok(Arc::new(backend))
        }
        BackendType::RawSocket => {
            let iface = config
                .interface_name
                .as_deref()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Interface name required for raw socket backend",
                    )
                })?;
            let backend = RawSocketBackend::new(iface)?;
            Ok(Arc::new(backend))
        }
        BackendType::Auto => {
            // Try DPDK first
            if let Ok(backend) = DpdkBackend::new(config.dpdk_port_id) {
                return Ok(Arc::new(backend));
            }
            // Fall back to raw socket with mmap if interface is specified
            if let Some(ref iface) = config.interface_name {
                let ring_config = ring_buffer::RingConfig {
                    frame_size: config.ring_frame_size,
                    frame_count: config.ring_frame_count,
                };
                if let Ok(backend) = RawSocketBackend::with_mmap(iface, true, &ring_config) {
                    return Ok(Arc::new(backend));
                }
                // Fall back to basic raw socket
                if let Ok(backend) = RawSocketBackend::new(iface) {
                    return Ok(Arc::new(backend));
                }
            }
            Err(io::Error::new(
                io::ErrorKind::Other,
                "No packet backend available (tried DPDK, AF_PACKET+mmap, AF_PACKET)",
            ))
        }
    }
}
