//! ECN extraction and TOS construction helpers.

use s2n_quic_core::inet::ExplicitCongestionNotification;

/// Extract the ECN codepoint from an IPv4 TOS byte (low 2 bits).
///
/// s2n-quic's `ExplicitCongestionNotification` is `#[repr(u8)]` with values
/// matching the wire bits: NotEct=0, Ect1=1, Ect0=2, Ce=3.
#[inline]
pub fn extract_ecn(tos_byte: u8) -> ExplicitCongestionNotification {
    let ecn_bits = tos_byte & 0x03;
    // Safety: ecn_bits is 0..=3, matching all enum variants of the #[repr(u8)] enum.
    unsafe { std::mem::transmute(ecn_bits) }
}

/// Convert an ECN codepoint to TOS bits for outbound frames.
#[inline]
pub fn ecn_to_tos_bits(ecn: ExplicitCongestionNotification) -> u8 {
    ecn as u8
}
