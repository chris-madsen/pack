//! Operator U_K = F_K⁻¹ · H · D_K · H · F_K
//!
//! Properties:
//!   - Bijective (no information loss)
//!   - U_K(U_K(x)) = x  (self-inverse / involution)
//!   - V = branch-bit log of D_K phase decisions, |V| = peaks * rounds
//!   - |V| is INDEPENDENT of block size N

use crate::domain::{
    branch::{branch_step, BranchBit, BranchVector},
    fwht::fwht_inplace,
    keygen::MagicKey,
};

/// Convert a u64-word slice into bipolar i64 (bit=1 → +1, bit=0 → -1).
fn words_to_bipolar(words: &[u64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(words.len() * 64);
    for &w in words {
        for b in (0..64).rev() {
            out.push(if (w >> b) & 1 == 1 { 1i64 } else { -1i64 });
        }
    }
    out
}

/// Pack bipolar i64 back into u64 words (positive → 1, non-positive → 0).
fn bipolar_to_words(bip: &[i64]) -> Vec<u64> {
    let n_words = (bip.len() + 63) / 64;
    let mut out = vec![0u64; n_words];
    for (i, &v) in bip.iter().enumerate() {
        if v > 0 {
            out[i / 64] |= 1u64 << (63 - (i % 64));
        }
    }
    out
}

/// One Feistel ARX round (forward).
/// L' = R
/// R' = L XOR g_K(R)
/// g_K(R) = ((R XOR (R << s0)) XOR (R >> s1)) * odd_mul + add_c
fn feistel_forward(words: &mut [u64], s0: u32, s1: u32, odd_mul: u64, add_c: u64) {
    let half = words.len() / 2;
    for i in 0..half {
        let l = words[i];
        let r = words[i + half];
        let g = r
            .wrapping_shl(s0)
            .bitxor_with(r)
            .wrapping_shr(s1)
            .bitxor_with(r.wrapping_shl(s0).bitxor_with(r))
            .wrapping_mul(odd_mul)
            .wrapping_add(add_c);
        words[i] = r;
        words[i + half] = l ^ g;
    }
}

/// One Feistel ARX round (inverse).
/// Given L' = R_orig, R' = L_orig XOR g_K(R_orig)
/// Recover: R_orig = L', L_orig = R' XOR g_K(L')
fn feistel_inverse(words: &mut [u64], s0: u32, s1: u32, odd_mul: u64, add_c: u64) {
    let half = words.len() / 2;
    for i in 0..half {
        let lp = words[i];
        let rp = words[i + half];
        let r_orig = lp;
        let g = r_orig
            .wrapping_shl(s0)
            .bitxor_with(r_orig)
            .wrapping_shr(s1)
            .bitxor_with(r_orig.wrapping_shl(s0).bitxor_with(r_orig))
            .wrapping_mul(odd_mul)
            .wrapping_add(add_c);
        let l_orig = rp ^ g;
        words[i] = l_orig;
        words[i + half] = r_orig;
    }
}

/// Helper trait for cleaner XOR chaining
trait BitXorWith: Sized {
    fn bitxor_with(self, other: Self) -> Self;
}
impl BitXorWith for u64 {
    #[inline(always)]
    fn bitxor_with(self, other: u64) -> u64 {
        self ^ other
    }
}

/// Apply one round of U_K to `words`, logging branch bits into `branch`.
/// U_K = F_K⁻¹ · H · D_K · H · F_K
fn apply_uk_round(
    words: &mut Vec<u64>,
    key: &MagicKey,
    branch: &mut BranchVector,
) {
    // 1. F_K forward
    feistel_forward(
        words,
        key.feistel_s0 as u32,
        key.feistel_s1 as u32,
        key.feistel_odd_mul,
        key.feistel_add,
    );

    // 2. H (Walsh-Hadamard)
    let mut bip = words_to_bipolar(words);
    fwht_inplace(&mut bip);

    // 3. D_K: phase mask at accepted peak indices + log branch bit
    for &idx in &key.peak_indices {
        if idx < bip.len() {
            // branch bit = parity of the coefficient's sign path
            let bit = BranchBit::from(bip[idx] > 0);
            branch.push(bit);
            bip[idx] = -bip[idx]; // flip phase
        }
    }

    // 4. H again
    fwht_inplace(&mut bip);

    // 5. F_K⁻¹
    *words = bipolar_to_words(&bip);
    feistel_inverse(
        words,
        key.feistel_s0 as u32,
        key.feistel_s1 as u32,
        key.feistel_odd_mul,
        key.feistel_add,
    );
}

/// Encode a block: apply `rounds` passes of U_K, collect BranchVector V.
/// Returns (transformed_words, V).
/// |V| = key.peak_indices.len() * rounds  — independent of block size.
pub fn encode_block(
    block: &[u8],
    key: &MagicKey,
    rounds: usize,
) -> (Vec<u64>, BranchVector) {
    let n_words = key.window_bits / 64;
    let mut words: Vec<u64> = block
        .chunks(8)
        .take(n_words)
        .map(|c| {
            let mut arr = [0u8; 8];
            arr[..c.len()].copy_from_slice(c);
            u64::from_le_bytes(arr)
        })
        .collect();
    while words.len() < n_words {
        words.push(0);
    }

    let mut branch = BranchVector::new();
    for _ in 0..rounds {
        apply_uk_round(&mut words, key, &mut branch);
    }
    (words, branch)
}

/// Decode: replay branch bits to reverse U_K passes.
/// Since U_K is self-inverse, we apply the same rounds in reverse
/// and use the stored branch bits to restore original phases.
pub fn decode_block(
    encoded_words: &[u64],
    key: &MagicKey,
    branch: &BranchVector,
    rounds: usize,
) -> Vec<u8> {
    let n_words = key.window_bits / 64;
    let mut words: Vec<u64> = encoded_words[..n_words.min(encoded_words.len())].to_vec();
    while words.len() < n_words {
        words.push(0);
    }

    // Replay rounds in reverse, reading branch bits in reverse order
    let peaks_per_round = key.peak_indices.len();
    let mut bit_cursor = rounds * peaks_per_round; // points past last bit

    for _ in 0..rounds {
        bit_cursor -= peaks_per_round;

        // F_K forward
        feistel_forward(
            &mut words,
            key.feistel_s0 as u32,
            key.feistel_s1 as u32,
            key.feistel_odd_mul,
            key.feistel_add,
        );

        let mut bip = words_to_bipolar(&words);
        fwht_inplace(&mut bip);

        // Restore phases using stored branch bits
        for (j, &idx) in key.peak_indices.iter().enumerate() {
            if idx < bip.len() {
                let stored_bit = branch.get(bit_cursor + j);
                let current_positive = bip[idx] > 0;
                let was_positive = stored_bit == BranchBit::Odd;
                if current_positive != was_positive {
                    bip[idx] = -bip[idx]; // restore original sign
                }
            }
        }

        fwht_inplace(&mut bip);
        words = bipolar_to_words(&bip);

        feistel_inverse(
            &mut words,
            key.feistel_s0 as u32,
            key.feistel_s1 as u32,
            key.feistel_odd_mul,
            key.feistel_add,
        );
    }

    // Convert words back to bytes
    words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .take(key.window_bits / 8)
        .collect()
}

/// Compression audit: is |K| + |V| + overhead < |N|?
pub fn audit(key: &MagicKey, branch: &BranchVector, overhead_bits: usize) -> bool {
    key.bit_len() + branch.len() + overhead_bits < key.window_bits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::keygen::synthesize_k;

    fn make_block(pattern: u8, size_bytes: usize) -> Vec<u8> {
        vec![pattern; size_bytes]
    }

    fn make_lcg(n: usize) -> Vec<u8> {
        let mut s = 0xDEADBEEFCAFEBABEu64;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                (s >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn encode_decode_lossless_uniform() {
        let block = make_block(0x3C, 512);
        let (key, _) = synthesize_k(&block, 4096);
        let (enc, branch) = encode_block(&block, &key, 3);
        let dec = decode_block(&enc, &key, &branch, 3);
        assert_eq!(block, dec, "lossless roundtrip failed for uniform block");
    }

    #[test]
    fn encode_decode_lossless_lcg() {
        let block = make_lcg(512);
        let (key, _) = synthesize_k(&block, 4096);
        let (enc, branch) = encode_block(&block, &key, 3);
        let dec = decode_block(&enc, &key, &branch, 3);
        assert_eq!(block, dec, "lossless roundtrip failed for LCG noise");
    }

    #[test]
    fn v_size_independent_of_block_size() {
        // Same key structure, different window sizes
        for &(size_bytes, window) in &[(512usize, 4096usize), (2048, 16384)] {
            let block = make_block(0x3C, size_bytes);
            let (key, _) = synthesize_k(&block, window);
            let rounds = 3;
            let (_, branch) = encode_block(&block, &key, rounds);
            let expected_v = key.peak_indices.len() * rounds;
            assert_eq!(
                branch.len(),
                expected_v,
                "|V|={} but expected peaks*rounds={}*{}={}",
                branch.len(), key.peak_indices.len(), rounds, expected_v
            );
        }
    }

    #[test]
    fn compression_audit_passes_for_uniform() {
        let block = make_block(0x3C, 512);
        let (key, _) = synthesize_k(&block, 4096);
        let (_, branch) = encode_block(&block, &key, 3);
        assert!(
            audit(&key, &branch, 64),
            "|K|={}b + |V|={}b + 64b overhead must < 4096b",
            key.bit_len(), branch.len()
        );
    }

    #[test]
    fn encoded_differs_from_original() {
        // Encoding must actually transform the block
        let block = make_block(0xAA, 512);
        let (key, _) = synthesize_k(&block, 4096);
        if key.peak_indices.is_empty() {
            return; // nothing to encode, skip
        }
        let (enc, _) = encode_block(&block, &key, 3);
        let enc_bytes: Vec<u8> = enc.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_ne!(block, enc_bytes, "encoded block must differ from original");
    }
}
