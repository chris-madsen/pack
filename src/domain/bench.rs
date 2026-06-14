//! Expanded benchmark: 10 canonical payload types.
//! Run: cargo test domain::bench::bench_tests::bench_adaptive_ratios -- --nocapture

#[cfg(test)]
mod bench_tests {
    use crate::domain::window::try_encode_adaptive;

    struct Case {
        name: &'static str,
        data: Vec<u8>,
    }

    fn lcg(seed: u64) -> impl Iterator<Item = u8> {
        let mut s = seed;
        std::iter::from_fn(move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            Some((s >> 56) as u8)
        })
    }

    fn make_cases(size: usize) -> Vec<Case> {
        vec![
            Case { name: "uniform_3C",
                   data: vec![0x3Cu8; size] },
            Case { name: "zeros",
                   data: vec![0x00u8; size] },
            Case { name: "alternating_A5",
                   // Byte-level alternation: 0xA5 0x5A 0xA5 0x5A ...
                   // In 64-bit words: 0xA5A5A5A5A5A5A5A5 / 0x5A5A5A5A5A5A5A5A
                   // Two Walsh peaks of equal amplitude → V sparse
                   data: (0..size).map(|i| if i % 2 == 0 { 0xA5 } else { 0x5A }).collect() },
            Case { name: "stride8_pattern",
                   // Same byte every 8 bytes, zeros in between → Walsh peak at freq=8
                   data: (0..size).map(|i| if i % 8 == 0 { 0xFF } else { 0x00 }).collect() },
            Case { name: "sequential",
                   data: (0..size).map(|i| (i % 256) as u8).collect() },
            Case { name: "random_lcg",
                   data: lcg(0xDEADBEEFCAFEBABE).take(size).collect() },
            Case { name: "text_like",
                   data: {
                       let a = b"abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ.,\n";
                       (0..size).map(|i| a[i % a.len()]).collect()
                   }},
            Case { name: "float64_array",
                   // IEEE 754 doubles: incrementing mantissa, fixed exponent 0x3FF
                   data: {
                       let mut v = Vec::with_capacity(size);
                       for i in 0..(size / 8) {
                           let f: f64 = i as f64 * 0.001;
                           v.extend_from_slice(&f.to_le_bytes());
                       }
                       v.resize(size, 0);
                       v
                   }},
            Case { name: "log_file",
                   // Simulated syslog: timestamp prefix + repeated ASCII lines
                   data: {
                       let line = b"2026-06-14T12:00:00Z INFO  pack encoder accepted block offset=000000 ratio=4.92\n";
                       (0..size).map(|i| line[i % line.len()]).collect()
                   }},
            Case { name: "sqlite_like",
                   data: {
                       let mut v = vec![0u8; size];
                       for (i, b) in b"SQLite format 3\x00".iter().enumerate() {
                           if i < size { v[i] = *b; }
                       }
                       for i in (100..size).step_by(4) {
                           v[i] = (i % 256) as u8;
                           if i + 1 < size { v[i + 1] = ((i >> 8) % 256) as u8; }
                       }
                       v
                   }},
        ]
    }

    /// Compress entire input by sliding the adaptive encoder.
    /// Returns (total_compressed_bytes, blocks_accepted, blocks_raw).
    fn compress_sliding(input: &[u8]) -> (usize, usize, usize) {
        let mut pos = 0;
        let mut compressed = 0usize;
        let mut accepted = 0usize;
        let mut raw_bytes = 0usize;
        while pos < input.len() {
            match try_encode_adaptive(&input[pos..]) {
                Some(block) => {
                    compressed += block.wire.len();
                    pos += block.window_bits / 8;
                    accepted += 1;
                }
                None => {
                    compressed += 1;
                    raw_bytes += 1;
                    pos += 1;
                }
            }
        }
        (compressed, accepted, raw_bytes)
    }

    #[test]
    fn bench_adaptive_ratios() {
        let size = 32 * 1024;
        let cases = make_cases(size);

        println!("\n{:=<80}", "");
        println!("  ADAPTIVE WALSH ENCODER — Compression Report (32 KB per payload)");
        println!("{:=<80}", "");
        println!(
            "{:<18} {:>8} {:>10} {:>8} {:>9} {:>7} {:>7}",
            "Payload", "Orig", "Compressed", "Ratio", "Saving%", "Blocks", "Raw-B"
        );
        println!("{:-<80}", "");

        for case in &cases {
            let (comp, blocks, raw_b) = compress_sliding(&case.data);
            let ratio = case.data.len() as f64 / comp.max(1) as f64;
            let saving = 100.0 * (1.0 - comp as f64 / case.data.len() as f64);
            let winner = if ratio >= 2.0 { "✓ WINS" } else if ratio >= 1.0 { "  tie" } else { "✗ loss" };
            println!(
                "{:<18} {:>8} {:>10} {:>7.2f}x {:>8.1f}% {:>7} {:>7}  {}",
                case.name, case.data.len(), comp, ratio, saving, blocks, raw_b, winner
            );
        }
        println!("{:=<80}", "");
    }

    // Individual correctness smoke-tests
    #[test]
    fn uniform_compresses_and_roundtrip_tag_present() {
        use crate::domain::window::decode_adaptive;
        let input = vec![0x3Cu8; 512];
        let block = try_encode_adaptive(&input).expect("uniform must compress");
        assert!(block.wire.len() < 512, "wire must be smaller than input");
        assert_eq!(block.wire[0], 0xAD, "tag byte must be 0xAD");
        let (decoded, consumed) = decode_adaptive(&block.wire).unwrap();
        assert_eq!(consumed, block.wire.len());
        assert_eq!(decoded.window_bits, block.window_bits);
    }

    #[test]
    fn random_does_not_expand_more_than_one_percent() {
        let input: Vec<u8> = lcg(0xCAFEBABE_DEADBEEF).take(4096).collect();
        let (comp, _, _) = compress_sliding(&input);
        let ratio = comp as f64 / input.len() as f64;
        assert!(ratio <= 1.01, "random must not expand >1%: got {ratio:.4}");
    }

    #[test]
    fn zeros_achieves_at_least_30x() {
        let input = vec![0u8; 4096];
        let (comp, _, _) = compress_sliding(&input);
        let ratio = input.len() as f64 / comp.max(1) as f64;
        assert!(ratio >= 30.0, "zeros must achieve >=30x: got {ratio:.2}x");
    }

    #[test]
    fn alternating_achieves_at_least_10x() {
        let input: Vec<u8> = (0..4096).map(|i| if i % 2 == 0 { 0xA5 } else { 0x5A }).collect();
        let (comp, _, _) = compress_sliding(&input);
        let ratio = input.len() as f64 / comp.max(1) as f64;
        assert!(ratio >= 10.0, "alternating must achieve >=10x: got {ratio:.2}x");
    }
}
