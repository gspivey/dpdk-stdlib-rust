//! Property-based tests for TCP codec (tasks 3.10, 3.11).

use proptest::prelude::*;

use dpdk_stdlib_tcp::codec::*;
use dpdk_stdlib_tcp::seq::SeqNum;

// --- Strategy helpers ---

fn arb_ipv4_addr() -> impl Strategy<Value = std::net::SocketAddr> {
    (
        any::<[u8; 4]>(),
        1u16..=65535u16,
    )
        .prop_map(|(ip, port)| {
            std::net::SocketAddr::from((ip, port))
        })
}

fn arb_mac() -> impl Strategy<Value = [u8; 6]> {
    any::<[u8; 6]>()
}

fn arb_non_syn_flags() -> impl Strategy<Value = TcpFlags> {
    // Generate flags that do NOT include SYN to test non-SYN paths
    any::<u8>().prop_map(|v| TcpFlags(v & 0x3D)) // mask out SYN bit
}

fn arb_payload(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=max_len)
}

fn arb_tcp_options() -> impl Strategy<Value = TcpOptions> {
    (
        proptest::option::of(1u16..=65535u16), // mss
        proptest::option::of(0u8..=14u8),      // window_scale
        any::<bool>(),                          // sack_permitted
        proptest::option::of((any::<u32>(), any::<u32>())), // timestamps
    )
        .prop_map(|(mss, window_scale, sack_permitted, timestamps)| TcpOptions {
            mss,
            window_scale,
            sack_permitted,
            timestamps,
            sack_blocks: vec![],
        })
}

fn arb_frame_params(max_payload: usize) -> impl Strategy<Value = TcpFrameParams> {
    (
        arb_mac(),
        arb_mac(),
        arb_ipv4_addr(),
        arb_ipv4_addr(),
        any::<u32>(),
        any::<u32>(),
        arb_non_syn_flags(),
        any::<u16>(),
        arb_tcp_options(),
        arb_payload(max_payload),
        1u8..=255u8,
    )
        .prop_map(
            |(src_mac, dst_mac, src, dst, seq, ack, flags, window, options, payload, ttl)| {
                TcpFrameParams {
                    src_mac,
                    dst_mac,
                    src,
                    dst,
                    seq: SeqNum(seq),
                    ack: SeqNum(ack),
                    flags,
                    window,
                    options,
                    payload,
                    ttl,
                }
            },
        )
}

fn arb_syn_frame_params() -> impl Strategy<Value = TcpFrameParams> {
    (
        arb_mac(),
        arb_mac(),
        arb_ipv4_addr(),
        arb_ipv4_addr(),
        any::<u32>(),
        any::<u32>(),
        any::<u16>(),
        arb_tcp_options(),
        1u8..=255u8,
        prop::bool::ANY,
    )
        .prop_map(
            |(src_mac, dst_mac, src, dst, seq, ack, window, options, ttl, is_syn_ack)| {
                let flags = if is_syn_ack {
                    TcpFlags::SYN | TcpFlags::ACK
                } else {
                    TcpFlags::SYN
                };
                TcpFrameParams {
                    src_mac,
                    dst_mac,
                    src,
                    dst,
                    seq: SeqNum(seq),
                    ack: SeqNum(ack),
                    flags,
                    window,
                    options,
                    payload: vec![], // SYN frames have no payload
                    ttl,
                }
            },
        )
}

// --- Property 1: Codec round-trip ---
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_codec_roundtrip(params in arb_frame_params(100)) {
        let frame = build_tcp_frame(&params).unwrap();
        let parsed = parse_tcp_packet(&frame).unwrap();

        prop_assert_eq!(parsed.src, params.src);
        prop_assert_eq!(parsed.dst, params.dst);
        prop_assert_eq!(parsed.seq, params.seq);
        prop_assert_eq!(parsed.ack, params.ack);
        prop_assert_eq!(parsed.flags, params.flags);
        prop_assert_eq!(parsed.window, params.window);
        prop_assert_eq!(parsed.payload, params.payload);
    }
}

// --- Property 2: Codec Mbuf equivalence ---
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_codec_mbuf_equivalence(params in arb_frame_params(100)) {
        use dpdk::mbuf::Mempool;

        let frame_vec = build_tcp_frame(&params).unwrap();

        // Build via Mbuf path (stubs provide real memory)
        let pool = Mempool::create("tcp_test", 128, 32, 2048, -1).unwrap();
        let mut mbuf = pool.alloc().unwrap();
        build_tcp_packet(&mut mbuf, &params).unwrap();
        let mbuf_data = mbuf.data_mut().unwrap();
        let mbuf_frame = &mbuf_data[..frame_vec.len()];

        prop_assert_eq!(frame_vec.as_slice(), mbuf_frame);
    }
}

// --- Property 3: Invalid frame rejection ---
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_invalid_frame_rejected(len in 0usize..54) {
        let frame = vec![0u8; len];
        prop_assert!(parse_tcp_packet(&frame).is_err());
    }

    #[test]
    fn prop_invalid_data_offset_rejected(
        params in arb_frame_params(10),
        bad_offset in 0u8..5u8,
    ) {
        let mut frame = build_tcp_frame(&params).unwrap();
        let tcp_off = 14 + 20; // ETH + IPv4
        frame[tcp_off + 12] = bad_offset << 4;
        prop_assert!(parse_tcp_packet(&frame).is_err());
    }
}

// --- Property 4: SYN required options ---
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_syn_required_options(params in arb_syn_frame_params()) {
        let frame = build_tcp_frame(&params).unwrap();
        let parsed = parse_tcp_packet(&frame).unwrap();

        // SYN/SYN-ACK must always contain MSS, WScale, SACK-Perm, Timestamps
        prop_assert!(parsed.options.mss.is_some(), "SYN missing MSS");
        prop_assert!(parsed.options.window_scale.is_some(), "SYN missing WScale");
        prop_assert!(parsed.options.sack_permitted, "SYN missing SACK-Perm");
        prop_assert!(parsed.options.timestamps.is_some(), "SYN missing Timestamps");
    }
}

// --- Property 5: MSS segment bound ---
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_mss_segment_bound(
        mtu in 40u16..=9000u16,
        ip_hdr_len in (20u16..=60u16).prop_filter("must be multiple of 4", |v| v % 4 == 0),
    ) {
        let mss = compute_mss(mtu, ip_hdr_len);
        // MSS must never exceed MTU - IP header - TCP header
        let expected_max = mtu.saturating_sub(ip_hdr_len).saturating_sub(20);
        prop_assert!(mss <= expected_max);
        // MSS should equal the expected value
        prop_assert_eq!(mss, expected_max);
    }
}

// --- Property 6: TCP checksum validity ---
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_checksum_valid(params in arb_frame_params(100)) {
        let frame = build_tcp_frame(&params).unwrap();

        let ip = 14usize; // ETH_HEADER_LEN
        let src_ip = &frame[ip + 12..ip + 16];
        let dst_ip = &frame[ip + 16..ip + 20];
        let tcp_start = 14 + 20; // ETH + IPv4
        let tcp_segment = &frame[tcp_start..];

        // Verifying a correct checksum: sum including the checksum field yields 0
        let check = tcp_checksum(src_ip, dst_ip, tcp_segment);
        prop_assert_eq!(check, 0, "checksum verification failed");
    }

    #[test]
    fn prop_checksum_flip_fails(
        params in arb_frame_params(50),
        bit_pos in 0usize..400usize,
    ) {
        let frame = build_tcp_frame(&params).unwrap();
        let tcp_start = 14 + 20;
        if tcp_start >= frame.len() {
            return Ok(());
        }

        let tcp_segment = &frame[tcp_start..];
        let byte_idx = bit_pos / 8;
        let bit_idx = bit_pos % 8;

        if byte_idx >= tcp_segment.len() {
            return Ok(());
        }

        // Flip one bit in the TCP segment
        let mut corrupted = tcp_segment.to_vec();
        corrupted[byte_idx] ^= 1 << bit_idx;

        let ip = 14usize;
        let src_ip = &frame[ip + 12..ip + 16];
        let dst_ip = &frame[ip + 16..ip + 20];

        let check = tcp_checksum(src_ip, dst_ip, &corrupted);
        // After a single-bit flip, checksum should NOT verify to 0
        prop_assert_ne!(check, 0, "checksum should fail after bit flip at byte {} bit {}", byte_idx, bit_idx);
    }
}

// --- Property 7: Sequence arithmetic transitivity ---
proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn prop_seq_transitivity(
        base in any::<u32>(),
        offset1 in 1u32..=(1 << 30),
        offset2 in 1u32..=(1 << 30),
    ) {
        // Ensure offsets fit in half-space for transitivity to hold
        let total = (offset1 as u64) + (offset2 as u64);
        prop_assume!(total < (1u64 << 31));

        let a = SeqNum(base);
        let b = a.add(offset1);
        let c = b.add(offset2);

        prop_assert!(a.lt(b), "a < b must hold");
        prop_assert!(b.lt(c), "b < c must hold");
        prop_assert!(a.lt(c), "transitivity: a < b < c => a < c");
    }

    #[test]
    fn prop_seq_n_lt_n_plus_1(n in any::<u32>()) {
        prop_assert!(SeqNum(n).lt(SeqNum(n).add(1)));
    }
}
