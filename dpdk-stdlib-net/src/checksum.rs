//! IPv4 and UDP checksum helpers

/// Calculate IPv4 header checksum
pub fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    for i in (0..header.len()).step_by(2) {
        let word = if i + 1 < header.len() {
            ((header[i] as u32) << 8) | (header[i + 1] as u32)
        } else {
            (header[i] as u32) << 8
        };
        sum = sum.wrapping_add(word);
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !(sum as u16)
}

/// Calculate UDP pseudo-header checksum, folded to 16 bits (NOT one's-complemented).
pub fn udp_pseudo_header_checksum(src_ip: &[u8; 4], dst_ip: &[u8; 4], udp_len: u16) -> u16 {
    let mut sum: u32 = 0;
    sum = sum.wrapping_add(((src_ip[0] as u32) << 8) | (src_ip[1] as u32));
    sum = sum.wrapping_add(((src_ip[2] as u32) << 8) | (src_ip[3] as u32));
    sum = sum.wrapping_add(((dst_ip[0] as u32) << 8) | (dst_ip[1] as u32));
    sum = sum.wrapping_add(((dst_ip[2] as u32) << 8) | (dst_ip[3] as u32));
    sum = sum.wrapping_add(17); // IP_PROTO_UDP
    sum = sum.wrapping_add(udp_len as u32);

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    sum as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipv4_checksum_zeros() {
        // All zeros should produce 0xFFFF
        let header = [0u8; 20];
        assert_eq!(ipv4_checksum(&header), 0xFFFF);
    }

    #[test]
    fn test_ipv4_checksum_valid() {
        // A valid IPv4 header with correct checksum should produce 0x0000
        let mut header = [0u8; 20];
        header[0] = 0x45; // Version + IHL
        header[2] = 0x00; header[3] = 0x3C; // Total length = 60
        header[8] = 64; // TTL
        header[9] = 17; // Protocol = UDP
        // src = 10.0.0.1
        header[12] = 10; header[13] = 0; header[14] = 0; header[15] = 1;
        // dst = 10.0.0.2
        header[16] = 10; header[17] = 0; header[18] = 0; header[19] = 2;
        // Compute and store checksum
        let cksum = ipv4_checksum(&header);
        header[10] = (cksum >> 8) as u8;
        header[11] = (cksum & 0xFF) as u8;
        // Re-verify: checksum of header with valid checksum should be 0
        assert_eq!(ipv4_checksum(&header), 0);
    }

    #[test]
    fn test_udp_pseudo_header_checksum() {
        let src = [10, 0, 0, 1];
        let dst = [10, 0, 0, 2];
        let result = udp_pseudo_header_checksum(&src, &dst, 20);
        // Non-zero result for non-trivial input
        assert_ne!(result, 0);
    }
}
