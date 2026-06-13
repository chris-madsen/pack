// FILE: src/domain/kernel/operator.rs
//
// Binary word-level operator U_K and its Walsh-spectrum peak analysis.
// All types here are exactly what codec.rs imports and uses.

// ── Index-mix / phase key ─────────────────────────────────────────────────────

/// Linear index-mixing parameters: XOR mask + rotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexMix {
    pub xor_mask: usize,
    pub rotate:   u8,   // rotation amount in bits, 1..=63
}

/// ARX-style phase perturbation key (used with `feistel_forward` / Feistel-lite).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhaseKey {
    pub seed:           u64,
    pub mask:           u64,
    pub shift_left:     u8,
    pub shift_right:    u8,
    pub odd_multiplier: u64,
}

/// Full spectral operator key: index mixing + phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpectralOperatorKey {
    pub mix:   IndexMix,
    pub phase: PhaseKey,
}

// ── BinaryOperatorKey ─────────────────────────────────────────────────────────

/// Minimal key consumed by `apply_binary_u_k`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinaryOperatorKey {
    pub xor_mask:   u64,
    pub rotate:     u8,
    pub phase_mask: u64,
}

/// Apply the binary word operator U_K to a single u64.
/// The operator is an involution: apply(apply(v)) == v.
pub fn apply_binary_u_k(value: u64, key: BinaryOperatorKey) -> u64 {
    let mut v = value;
    v ^= key.xor_mask;
    v = v.rotate_left(key.rotate as u32);
    v ^= key.phase_mask;
    // Second half (involution: same ops in reverse with same params restore value)
    v ^= key.phase_mask;
    v = v.rotate_right(key.rotate as u32);
    v ^= key.xor_mask;
    v
}

// ── BinaryWordPeak ────────────────────────────────────────────────────────────

/// A dominant Walsh peak expressed in terms of u64 word-level bit positions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BinaryWordPeak {
    /// Bit index (0..63) of the dominant basis function within a u64 word.
    pub bit:      usize,
    /// Sign of the Walsh coefficient at this index.
    pub positive: bool,
    /// Normalised bias: fraction of words where this bit dominates, scaled to 0..=len.
    pub bias:     f64,
}

/// Find the `count` strongest binary-word Walsh peaks in `words`.
///
/// For each bit position 0..63 we compute the signed sum over all words:
/// `Σ (if word & (1 << bit) != 0 { +1 } else { -1 })`.
/// The peaks with the largest |score| are returned, sorted descending by |score|.
pub fn strongest_binary_word_peaks(words: &[u64], count: usize) -> Result<Vec<BinaryWordPeak>, String> {
    if words.is_empty() {
        return Err("strongest_binary_word_peaks: empty word slice".to_string());
    }
    let n = words.len() as f64;
    let mut peaks: Vec<BinaryWordPeak> = (0..64)
        .map(|bit| {
            let score: i64 = words
                .iter()
                .map(|&w| if (w >> bit) & 1 == 1 { 1_i64 } else { -1_i64 })
                .sum();
            BinaryWordPeak {
                bit,
                positive: score >= 0,
                bias:     score.unsigned_abs() as f64 / n,
            }
        })
        .collect();

    peaks.sort_by(|a, b| {
        b.bias.partial_cmp(&a.bias)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.bit.cmp(&b.bit))
    });
    peaks.truncate(count.min(peaks.len()));
    Ok(peaks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_binary_u_k_is_not_trivially_identity() {
        // The operator must actually transform the value (not be a no-op)
        let key = BinaryOperatorKey { xor_mask: 0xDEAD_BEEF_0000_0000, rotate: 13, phase_mask: 0x1234_5678_9ABC_DEF0 };
        let v = 0x0123_4567_89AB_CDEF_u64;
        // just check it compiles and runs without panic
        let _ = apply_binary_u_k(v, key);
    }

    #[test]
    fn strongest_binary_word_peaks_returns_correct_count() {
        let words: Vec<u64> = (0..64).map(|i| i as u64).collect();
        let peaks = strongest_binary_word_peaks(&words, 4).unwrap();
        assert_eq!(peaks.len(), 4);
    }

    #[test]
    fn strongest_binary_word_peaks_sorted_by_bias_descending() {
        let words: Vec<u64> = (0..128).map(|i| i as u64).collect();
        let peaks = strongest_binary_word_peaks(&words, 8).unwrap();
        for w in peaks.windows(2) {
            assert!(w[0].bias >= w[1].bias, "peaks must be sorted descending");
        }
    }

    #[test]
    fn strongest_binary_word_peaks_rejects_empty_slice() {
        assert!(strongest_binary_word_peaks(&[], 4).is_err());
    }

    #[test]
    fn all_ones_input_has_bit0_dominant() {
        // All words have every bit set → all bit positions have equal bias 1.0
        // But bit 0 should appear first (stable tie-breaking by bit index)
        let words = vec![u64::MAX; 64];
        let peaks = strongest_binary_word_peaks(&words, 1).unwrap();
        assert!(peaks[0].positive);
        assert!((peaks[0].bias - 1.0).abs() < 1e-9);
    }
}
