//! Synthetic benchmark: compress 7 canonical payload types with the
//! adaptive window encoder and report ratio metrics.
//!
//! Run with: `cargo test bench_adaptive_ratios -- --nocapture`

#[cfg(test)]
mod bench_tests {
    use crate::domain::window::try_encode_adaptive;

    struct Case {
        name: &'static str,
        data: Vec<u8>,
    }

    fn make_cases(size: usize) -> Vec<Case> {
        vec![
            Case {
                name: "uniform_3C",
                data: vec![0x3Cu8; size],
            },
            Case {
                name: "zeros",
                data: vec![0x00u8; size],
            },
            Case {
                name: "alternating_A5",
                data: (0..size).map(|i| if i % 2 == 0 { 0xA5 } else { 0x5A }).collect(),
            },
            Case {
                name: "sequential",
                data: (0..size).map(|i| (i % 256) as u8).collect(),
            },
            Case {
                name: "random",
                data: (0..size)
                    .map(|i| ((i.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407)) % 256) as u8)
                    .collect(),
            },
            Case {
                name: "text_like",
                data: {
                    let alphabet = b"abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ.,\n";
                    (0..size).map(|i| alphabet[i % alphabet.len()]).collect()
                },
            },
            Case {
                name: "sqlite_like",
                data: {
                    // SQLite page pattern: 100-byte header, then 4-byte aligned records
                    let mut v = vec![0u8; size];
                    // Page header
                    for (i, b) in b"SQLite format 3\x00".iter().enumerate() {
                        if i < size { v[i] = *b; }
                    }
                    // Simulate sparse record pages: mostly zeros with periodic data
                    for i in (100..size).step_by(4) {
                        v[i] = (i % 256) as u8;
                        if i + 1 < size { v[i + 1] = ((i >> 8) % 256) as u8; }
                    }
                    v
                },
            },
        ]
    }

    #[test]
    fn bench_adaptive_ratios() {
        let size = 32 * 1024; // 32 KB
        let cases = make_cases(size);

        println!("\n{:-<72}", "");
        println!(
            "{:<18} {:>10} {:>12} {:>10} {:>12}",
            "Payload", "Original", "Adaptive", "Ratio", "Saving%"
        );
        println!("{:-<72}", "");

        for case in &cases {
            // Adaptive encoder works on a single window (first window_bytes of input).
            // For bench we compress the entire input by sliding the best window.
            let (compressed, _blocks) = compress_sliding(&case.data);
            let ratio = case.data.len() as f64 / compressed.max(1) as f64;
            let saving = 100.0 * (1.0 - (compressed as f64 / case.data.len() as f64));
            println!(
                "{:<18} {:>10} {:>12} {:>9.2}x {:>11.1}%",
                case.name,
                case.data.len(),
                compressed,
                ratio,
                saving,
            );
        }
        println!("{:-<72}", "");
    }

    /// Slide the adaptive encoder over the entire input.
    /// Each accepted block contributes its wire bytes; rejected tail = raw.
    fn compress_sliding(input: &[u8]) -> (usize, usize) {
        let mut pos = 0;
        let mut compressed = 0usize;
        let mut blocks = 0usize;
        while pos < input.len() {
            let slice = &input[pos..];
            match try_encode_adaptive(slice) {
                Some(block) => {
                    compressed += block.wire.len();
                    pos += block.window_bits / 8;
                    blocks += 1;
                }
                None => {
                    // raw byte passthrough
                    compressed += 1;
                    pos += 1;
                }
            }
        }
        (compressed, blocks)
    }
}
