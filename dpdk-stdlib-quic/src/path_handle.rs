//! Path handle carrying local and remote socket addresses.

use crate::error::DpdkQuicError;
use s2n_quic_core::inet::SocketAddress;
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
    /// Create a new path handle, rejecting IPv6 addresses.
    pub fn try_new(
        remote: path::RemoteAddress,
        local: path::LocalAddress,
    ) -> Result<Self, DpdkQuicError> {
        if matches!(*remote, SocketAddress::IpV6(_)) {
            return Err(DpdkQuicError::UnsupportedAddressFamily);
        }
        if matches!(*local, SocketAddress::IpV6(_)) {
            return Err(DpdkQuicError::UnsupportedAddressFamily);
        }
        Ok(Self { remote, local })
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

#[cfg(test)]
mod tests {
    use super::*;
    use s2n_quic_core::inet::{IpV4Address, IpV6Address};
    use s2n_quic_core::path::Handle as _;

    fn ipv4_remote() -> path::RemoteAddress {
        let addr = IpV4Address::from([127, 0, 0, 1]).with_port(4433);
        path::RemoteAddress::from(addr)
    }

    fn ipv4_local() -> path::LocalAddress {
        let addr = IpV4Address::from([0, 0, 0, 0]).with_port(4433);
        path::LocalAddress::from(addr)
    }

    fn ipv6_remote() -> path::RemoteAddress {
        let addr = IpV6Address::from([0u8; 16]).with_port(4433);
        path::RemoteAddress::from(addr)
    }

    #[test]
    fn from_remote_address_round_trip() {
        let remote = ipv4_remote();
        let handle = DpdkPathHandle::from_remote_address(remote);
        assert_eq!(handle.remote_address(), remote);
    }

    #[test]
    fn try_new_ipv4_succeeds() {
        let result = DpdkPathHandle::try_new(ipv4_remote(), ipv4_local());
        assert!(result.is_ok());
        let handle = result.unwrap();
        assert_eq!(handle.remote_address(), ipv4_remote());
        assert_eq!(handle.local_address(), ipv4_local());
    }

    #[test]
    fn try_new_ipv6_remote_rejected() {
        let result = DpdkPathHandle::try_new(ipv6_remote(), ipv4_local());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DpdkQuicError::UnsupportedAddressFamily
        ));
    }

    #[test]
    fn try_new_ipv6_local_rejected() {
        let local_v6 = path::LocalAddress::from(IpV6Address::from([0u8; 16]).with_port(4433));
        let result = DpdkPathHandle::try_new(ipv4_remote(), local_v6);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DpdkQuicError::UnsupportedAddressFamily
        ));
    }

    #[test]
    fn set_and_get_addresses() {
        let mut handle = DpdkPathHandle::from_remote_address(ipv4_remote());
        let new_local = ipv4_local();
        handle.set_local_address(new_local);
        assert_eq!(handle.local_address(), new_local);

        let new_remote = path::RemoteAddress::from(
            IpV4Address::from([192, 168, 1, 1]).with_port(5000),
        );
        handle.set_remote_address(new_remote);
        assert_eq!(handle.remote_address(), new_remote);
    }

    #[test]
    fn strict_eq_same_addresses() {
        let h1 = DpdkPathHandle::try_new(ipv4_remote(), ipv4_local()).unwrap();
        let h2 = DpdkPathHandle::try_new(ipv4_remote(), ipv4_local()).unwrap();
        assert!(path::Handle::strict_eq(&h1, &h2));
    }
}
