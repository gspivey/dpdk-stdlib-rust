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

#[cfg(test)]
mod tests {
    use super::*;
    use s2n_quic_core::inet::ExplicitCongestionNotification::*;

    #[test]
    fn extract_ecn_all_codepoints() {
        assert_eq!(extract_ecn(0b00), NotEct);
        assert_eq!(extract_ecn(0b01), Ect1);
        assert_eq!(extract_ecn(0b10), Ect0);
        assert_eq!(extract_ecn(0b11), Ce);
    }

    #[test]
    fn extract_ecn_ignores_upper_bits() {
        // DSCP bits should be masked off
        assert_eq!(extract_ecn(0b11111100), NotEct);
        assert_eq!(extract_ecn(0b11111101), Ect1);
        assert_eq!(extract_ecn(0b11111110), Ect0);
        assert_eq!(extract_ecn(0b11111111), Ce);
    }

    #[test]
    fn ecn_round_trip_all_variants() {
        for ecn in [NotEct, Ect1, Ect0, Ce] {
            let tos = ecn_to_tos_bits(ecn);
            let recovered = extract_ecn(tos);
            assert_eq!(recovered, ecn);
        }
    }

    #[test]
    fn ecn_to_tos_bits_values() {
        assert_eq!(ecn_to_tos_bits(NotEct), 0b00);
        assert_eq!(ecn_to_tos_bits(Ect1), 0b01);
        assert_eq!(ecn_to_tos_bits(Ect0), 0b10);
        assert_eq!(ecn_to_tos_bits(Ce), 0b11);
    }
}
