//! Integration tests for the codec layer.
//!
//! Run with: `cargo test`
//!
//! These tests live in a dedicated file so the main codec.rs stays clean.
//! Each test is annotated with the bug it covers from the code review.

#[cfg(test)]
mod tests {
    use crate::domain::bitstream::bit_width_for_cardinality;
    use crate::domain::codec::{compress_bytes, decompress_bytes, inspect_archive};

    // ─── Roundtrip ────────────────────────────────────────────────────────────

    /// Basic sanity: compress → decompress must be identity for all inputs.
    #[test]
    fn compress_decompress_roundtrip_sequential() {
        let input: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        let compressed = compress_bytes(&input, None).expect("compress failed");
        let restored = decompress_bytes(&compressed).expect("decompress failed");
        assert_eq!(restored, input, "roundtrip failed for sequential 1 KB input");
    }

    #[test]
    fn compress_decompress_roundtrip_random_looking() {
        // Pseudo-random via xorshift — no external deps needed.
        let mut state: u64 = 0xDEAD_BEEF_CAFE_1234;
        let input: Vec<u8> = (0..2048)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect();
        let compressed = compress_bytes(&input, None).expect("compress failed");
        let restored = decompress_bytes(&compressed).expect("decompress failed");
        assert_eq!(restored, input, "roundtrip failed for pseudo-random 2 KB input");
    }

    /// WHITE NOISE TEST: 4 KB xorshift pseudo-random — closest to adversarial input.
    /// Verifies codec correctness on data with no exploitable structure.
    #[test]
    fn compress_decompress_roundtrip_white_noise_4kb() {
        let mut state: u64 = 0x0123_4567_89AB_CDEF;
        let input: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect();
        let compressed = compress_bytes(&input, None).expect("compress failed on white noise");
        let restored = decompress_bytes(&compressed).expect("decompress failed on white noise");
        assert_eq!(restored, input, "roundtrip failed for 4 KB white noise");
        // White noise should not compress — archive must not be smaller than input
        // (it may be slightly larger due to header overhead, that's expected).
        assert!(
            compressed.len() >= input.len().saturating_sub(64),
            "white noise compressed suspiciously well: {} -> {} bytes",
            input.len(),
            compressed.len()
        );
    }

    #[test]
    fn compress_decompress_roundtrip_empty() {
        let input: Vec<u8> = vec![];
        let compressed = compress_bytes(&input, None).expect("compress failed on empty input");
        let restored = decompress_bytes(&compressed).expect("decompress failed on empty input");
        assert_eq!(restored, input);
    }

    #[test]
    fn compress_decompress_roundtrip_single_byte() {
        let input = vec![0x42u8];
        let compressed = compress_bytes(&input, None).expect("compress failed");
        let restored = decompress_bytes(&compressed).expect("decompress failed");
        assert_eq!(restored, input);
    }

    // ─── Uniform input (triggers bit_width_for_cardinality(1) == 0 edge case) ─

    /// BUG #7 (review): uniform input → alphabet size 1 → bit_width == 0.
    /// pack_indices(&indices, 0) must not panic or corrupt data.
    #[test]
    fn compress_decompress_uniform_input() {
        let input = vec![0x42u8; 512];
        let compressed = compress_bytes(&input, None).expect("compress failed on uniform input");
        let restored = decompress_bytes(&compressed).expect("decompress failed on uniform input");
        assert_eq!(restored, input, "roundtrip failed for uniform input");
    }

    #[test]
    fn compress_uniform_is_smaller() {
        let input = vec![0x00u8; 512];
        let compressed = compress_bytes(&input, None).expect("compress failed");
        assert!(
            compressed.len() < input.len(),
            "uniform input should compress: got {} bytes from {} bytes",
            compressed.len(),
            input.len()
        );
    }

    // ─── bit_width_for_cardinality edge cases ─────────────────────────────────

    #[test]
    fn bit_width_for_cardinality_one() {
        assert_eq!(bit_width_for_cardinality(1), 0);
    }

    #[test]
    fn bit_width_for_cardinality_two() {
        assert_eq!(bit_width_for_cardinality(2), 1);
    }

    #[test]
    fn bit_width_for_cardinality_256() {
        assert_eq!(bit_width_for_cardinality(256), 8);
    }

    // ─── inspect_archive ──────────────────────────────────────────────────────

    /// BUG #5 (review): inspect_archive returned input_size: 0 for single-layer archives.
    #[test]
    fn inspect_archive_single_layer_input_size_is_correct() {
        let input = vec![0xABu8; 256];
        let compressed = compress_bytes(&input, None).expect("compress failed");
        let layers = inspect_archive(&compressed).expect("inspect failed");
        assert!(!layers.is_empty(), "expected at least one layer");
        assert_eq!(
            layers[0].input_size, 256,
            "single-layer archive must report correct input_size"
        );
    }

    #[test]
    fn inspect_archive_output_size_does_not_exceed_compressed_len() {
        let input: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        let compressed = compress_bytes(&input, None).expect("compress failed");
        let compressed_len = compressed.len() as u64;
        let layers = inspect_archive(&compressed).expect("inspect failed");
        assert!(
            layers[0].output_size <= compressed_len,
            "output_size {} should not exceed compressed length {}",
            layers[0].output_size,
            compressed_len
        );
    }

    // ─── Block-size hint ──────────────────────────────────────────────────────

    #[test]
    fn compress_with_explicit_block_size_hint_roundtrips() {
        let input: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        for hint in [16usize, 32, 64, 128, 256] {
            let compressed = compress_bytes(&input, Some(hint)).expect("compress failed");
            let restored = decompress_bytes(&compressed).expect("decompress failed");
            assert_eq!(restored, input, "roundtrip failed with block hint {hint}");
        }
    }

    // ─── Determinism ──────────────────────────────────────────────────────────

    /// Compress twice with the same input → same archive bytes.
    /// Ensures the codec is deterministic (no hidden randomness).
    #[test]
    fn compress_is_deterministic() {
        let input: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        let a = compress_bytes(&input, None).expect("compress failed");
        let b = compress_bytes(&input, None).expect("compress failed");
        assert_eq!(a, b, "compress must be deterministic");
    }
}
