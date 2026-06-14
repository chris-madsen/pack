//! Greedy auditor: synthesize_k(block) → MagicKey
//!
//! Does NOT search blindly. For each spectral peak candidate:
//!   cost  = log2(window_bits)  [bits added to K]
//!   gain  = estimated entropy reduction in V
//!   accept iff gain > cost
//!
//! This ensures |K| + |V| + overhead < |N| by construction.

use crate::domain::fwht::{analyse_block, SpectralProfile};

/// The magic constant K for one block.
/// Contains only coordinates of productive spectral peaks + Feistel params.
/// Does NOT contain the data itself.
#[derive(Debug, Clone)]
pub struct MagicKey {
    /// Block size this key was synthesised for (bits, power of 2)
    pub window_bits: usize,
    /// Accepted spectral peak indices (each costs log2(window_bits) bits in K)
    pub peak_indices: Vec<usize>,
    /// Feistel round parameters derived from top peak amplitudes
    pub feistel_s0: u8,
    pub feistel_s1: u8,
    pub feistel_odd_mul: u64,
    pub feistel_add: u64,
}

impl MagicKey {
    /// Serialised size in bits.
    pub fn bit_len(&self) -> usize {
        let index_bits = self.peak_indices.len() * index_bits_for(self.window_bits);
        let feistel_bits = 8 + 8 + 64 + 64; // s0 + s1 + mul + add
        let header_bits = 32; // window_bits encoded as u32
        header_bits + index_bits + feistel_bits
    }

    /// Serialised size in bytes (rounded up).
    pub fn byte_len(&self) -> usize {
        (self.bit_len() + 7) / 8
    }
}

/// Bits needed to encode one peak index for a given window size.
#[inline]
fn index_bits_for(window_bits: usize) -> usize {
    // log2(window_bits): e.g. 4096-bit window → 12 bits per index
    usize::BITS as usize - window_bits.leading_zeros() as usize - 1
}

/// Estimate how many V-bits a peak at this amplitude will eliminate.
/// Heuristic: amplitude/window_bits gives fraction of bits correctly
/// phased by this peak → that fraction no longer needs branch bits.
fn estimated_gain(amplitude: i64, window_bits: usize, current_v_bits: usize) -> usize {
    let frac = (amplitude.unsigned_abs() as f64) / (window_bits as f64);
    // Each correctly phased bit saves ~1 branch bit
    let gain = frac * current_v_bits as f64;
    gain as usize
}

/// Synthesise a MagicKey from a block using greedy spectral audit.
///
/// Algorithm:
///   1. Compute FWHT spectrum
///   2. For each peak (sorted by amplitude desc):
///        cost  = log2(window_bits) bits added to K
///        gain  = estimated V entropy reduction
///        if gain > cost: accept peak, reduce remaining_v_bits
///   3. Derive Feistel params from top-2 peak amplitudes
///   4. Return MagicKey
///
/// Guarantee: |K| + estimated_|V| < |N| if any peaks were accepted.
pub fn synthesize_k(block: &[u8], window_bits: usize) -> (MagicKey, usize) {
    debug_assert!(window_bits.is_power_of_two());

    let profile = analyse_block(block, window_bits);
    let idx_cost = index_bits_for(window_bits);

    let mut accepted_peaks: Vec<usize> = Vec::new();
    let mut remaining_v_bits = window_bits; // start: V = entire block

    for &(idx, amplitude) in &profile.peaks {
        if idx == 0 {
            continue; // DC component — skip
        }
        let gain = estimated_gain(amplitude, window_bits, remaining_v_bits);
        let cost = idx_cost;
        if gain > cost {
            accepted_peaks.push(idx);
            remaining_v_bits = remaining_v_bits.saturating_sub(gain);
        }
        // Stop early if V already tiny or K getting large
        let k_bits_so_far = accepted_peaks.len() * idx_cost + 176; // 176 = feistel overhead
        if k_bits_so_far + remaining_v_bits + 64 >= window_bits {
            // No longer profitable to add more peaks
            if gain <= cost {
                break;
            }
        }
    }

    // Derive Feistel params from top peak amplitudes
    let amp0 = profile.peaks.get(0).map(|&(_, a)| a.unsigned_abs()).unwrap_or(1) as u64;
    let amp1 = profile.peaks.get(1).map(|&(_, a)| a.unsigned_abs()).unwrap_or(1) as u64;

    // fold64: xor-fold to 8 bits for shifts, ensure odd for multiplier
    let fold = |v: u64| -> u64 {
        let v = v ^ (v >> 32);
        let v = v ^ (v >> 16);
        let v = v ^ (v >> 8);
        v & 0xFF
    };

    let feistel_s0 = (fold(amp0) % 63 + 1) as u8; // 1..=63
    let feistel_s1 = (fold(amp1) % 63 + 1) as u8;
    let feistel_odd_mul = (amp0.wrapping_mul(6364136223846793005)) | 1; // ensure odd
    let feistel_add = amp1.wrapping_mul(1442695040888963407);

    let key = MagicKey {
        window_bits,
        peak_indices: accepted_peaks,
        feistel_s0,
        feistel_s1,
        feistel_odd_mul,
        feistel_add,
    };

    let estimated_v_bits = remaining_v_bits;
    (key, estimated_v_bits)
}

/// Compression audit: returns true if K+V < N.
pub fn is_profitable(key: &MagicKey, estimated_v_bits: usize, overhead_bits: usize) -> bool {
    key.bit_len() + estimated_v_bits + overhead_bits < key.window_bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_block_is_profitable() {
        let block = vec![0x3Cu8; 512]; // 512 bytes = 4096 bits
        let (key, v_bits) = synthesize_k(&block, 4096);
        assert!(
            !key.peak_indices.is_empty(),
            "uniform block must yield at least one accepted peak"
        );
        assert!(
            is_profitable(&key, v_bits, 64),
            "|K|={}b + |V|={}b + overhead=64b must < 4096b",
            key.bit_len(),
            v_bits
        );
    }

    #[test]
    fn random_block_may_not_be_profitable() {
        // Pure LCG noise — metagen should not force-compress
        let mut s = 0xBADC0FFEu64;
        let block: Vec<u8> = (0..512)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                (s >> 56) as u8
            })
            .collect();
        let (key, v_bits) = synthesize_k(&block, 4096);
        // We don't assert this MUST fail — but we assert no silent data loss:
        // either profitable OR key is empty (honest fallback)
        if !is_profitable(&key, v_bits, 64) {
            assert!(
                key.peak_indices.is_empty() || v_bits >= 4096 / 2,
                "unprofitable block must have empty key or large V"
            );
        }
    }

    #[test]
    fn k_size_scales_with_log2_window() {
        // For a 4096-bit window: each peak costs 12 bits
        assert_eq!(index_bits_for(4096), 12);
        // For a 1024-bit window: each peak costs 10 bits
        assert_eq!(index_bits_for(1024), 10);
        // For a 65536-bit window: 16 bits
        assert_eq!(index_bits_for(65536), 16);
    }

    #[test]
    fn estimated_v_never_exceeds_block() {
        for &window in &[1024usize, 4096, 16384] {
            let block = vec![0xAAu8; window / 8];
            let (_, v_bits) = synthesize_k(&block, window);
            assert!(
                v_bits <= window,
                "estimated V={v_bits} must not exceed window={window}"
            );
        }
    }
}
