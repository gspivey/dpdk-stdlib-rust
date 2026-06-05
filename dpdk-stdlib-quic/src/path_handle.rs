//! Path handle carrying local and remote socket addresses.

use s2n_quic_core::path;

/// Path handle for DPDK-based QUIC connections.
///
/// Carries both local and remote addresses as `SocketAddress` values (IPv4/IPv6).
/// IPv6 is rejected at construction boundaries with `UnsupportedAddressFamily`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DpdkPathHandle {
    remote: path::RemoteAddress,
    local: path::LocalAddress,
}

impl DpdkPathHandle {
    /// Create a new path handle.
    pub fn new(remote: path::RemoteAddress, local: path::LocalAddress) -> Self {
        Self { remote, local }
    }
}

impl path::Handle for DpdkPathHandle {
    fn from_remote_address(remote_address: path::RemoteAddress) -> Self {
        Self {
            remote: remote_address,
            local: path::LocalAddress::default(),
        }
    }

    fn remote_address(&self) -> path::RemoteAddress {
        self.remote
    }

    fn set_remote_address(&mut self, remote_address: path::RemoteAddress) {
        self.remote = remote_address;
    }

    fn local_address(&self) -> path::LocalAddress {
        self.local
    }

    fn set_local_address(&mut self, local_address: path::LocalAddress) {
        self.local = local_address;
    }

    fn unmapped_eq(&self, other: &Self) -> bool {
        self.remote.unmapped_eq(&other.remote) && self.local.unmapped_eq(&other.local)
    }

    fn strict_eq(&self, other: &Self) -> bool {
        self.remote == other.remote && self.local == other.local
    }

    fn maybe_update(&mut self, other: &Self) {
        self.remote = other.remote;
        self.local = other.local;
    }
}
