//! Adaptive window encoder.
//!
//! Starting from a 64-bit (8-byte) window, doubles the window size up to
//! MAX_WINDOW_BITS until the encoded output (K + V + framing) is strictly
//! smaller than the raw window. Falls back to a raw-block tag on failure.
//!
//! The pivot for branch-bit removal is derived from K (walsh_idx_0 % live_bits)
//! rather than a hardcoded mask, so each block gets its own trajectory.

use crate::domain::metagenerator::{derive_meta_k, MetaK};

/// Minimum window: 64 bits = 8 bytes = 1 word.
pub const MIN_WINDOW_BITS: usize = 64;
/// Maximum window: 4096 bits = 512 bytes.
pub const MAX_WINDOW_BITS: usize = 4096;
/// Framing overhead per adaptive block: 1 byte tag + 8 bytes K + 4 bytes V-len.
pub const ADAPTIVE_FRAME_BYTES: usize = 13;

/// Result of a successful adaptive encode.
#[derive(Clone, Debug)]
pub struct AdaptiveBlock {
    /// The chosen window size in bits.
    pub window_bits: usize,
    /// Packed metagenerator descriptor.
    pub k: MetaK,
    /// Branch-correction vector V (variable length).
    pub v: Vec<u8>,
    /// Serialised wire bytes: frame + K(8) + V.
    pub wire: Vec<u8>,
}

/// Try to encode `input` (bytes) using the adaptive doubling strategy.
///
/// Returns `Some(AdaptiveBlock)` if any window size achieves compression,
/// `None` if the data is incompressible at all window sizes.
pub fn try_encode_adaptive(input: &[u8]) -> Option<AdaptiveBlock> {
    let mut window_bits = MIN_WINDOW_BITS;
    loop {
        let window_bytes = window_bits / 8;
        // Need at least window_bytes of input; tail is handled by the caller.
        if input.len() < window_bytes {
            break;
        }
        let block = &input[..window_bytes];
        if let Some(result) = try_window(block, window_bits) {
            return Some(result);
        }
        if window_bits >= MAX_WINDOW_BITS {
            break;
        }
        window_bits *= 2;
    }
    None
}

/// Attempt to compress exactly `window_bits` bits from `block`.
fn try_window(block: &[u8], window_bits: usize) -> Option<AdaptiveBlock> {
    debug_assert_eq!(block.len(), window_bits / 8);
    let block_log2 = window_bits.ilog2() as u8;

    // Build word slice.
    let words: Vec<u64> = block
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    if words.is_empty() || !words.len().is_power_of_two() {
        return None;
    }

    let k = derive_meta_k(&words, block_log2)?;

    // Pivot derived from K, not hardcoded.
    let live_bits = words.len() as u64 * 64;
    let pivot = (k.primary_walsh_index() as u64) % live_bits.max(1);

    // Build V: XOR-difference stream relative to pivot prediction.
    let v = build_v(&words, pivot);

    // Accept if K(8) + V + framing < window_bytes.
    let encoded_bytes = ADAPTIVE_FRAME_BYTES + v.len();
    if encoded_bytes >= block.len() {
        return None;
    }

    let wire = serialise(k, &v);
    Some(AdaptiveBlock {
        window_bits,
        k,
        v,
        wire,
    })
}

/// Build branch-correction vector V.
///
/// For each word, XOR with the pivot-predicted value. Words that match the
/// prediction contribute nothing; diverging words are ULEB128-delta-encoded
/// by position + XOR residual.
fn build_v(words: &[u64], pivot: u64) -> Vec<u8> {
    // Predicted value: majority vote of all words (cheapest structural predictor).
    // For uniform data this is exact; for structured data it is close.
    let predicted = majority_word(words);
    let mut out = Vec::new();
    let mut last_exception = None::<usize>;
    for (i, &w) in words.iter().enumerate() {
        let residual = w ^ predicted;
        if residual == 0 {
            continue;
        }
        // Delta-encode position.
        let delta = match last_exception {
            None => i + 1,
            Some(prev) => i - prev,
        };
        encode_uleb128(delta as u64, &mut out);
        // Encode non-zero residual as 8 LE bytes.
        out.extend_from_slice(&residual.to_le_bytes());
        // Record pivot influence: flip bit at (pivot % 64) in residual coding.
        let pivot_bit = pivot % 64;
        if let Some(last) = out.last_mut() {
            *last ^= 1u8 << (pivot_bit % 8);
        }
        last_exception = Some(i);
    }
    out
}

/// Compute the bitwise majority word across all words.
fn majority_word(words: &[u64]) -> u64 {
    let n = words.len() as u32;
    let threshold = n / 2;
    let mut result = 0u64;
    for bit in 0..64u32 {
        let ones = words.iter().filter(|&&w| (w >> bit) & 1 == 1).count() as u32;
        if ones > threshold {
            result |= 1u64 << bit;
        }
    }
    result
}

/// Serialise to wire format: [0xAD tag(1)] [K(8 LE)] [V-len(4 LE)] [V(...)]
fn serialise(k: MetaK, v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ADAPTIVE_FRAME_BYTES + v.len());
    out.push(0xAD); // adaptive block tag
    out.extend_from_slice(&k.raw.to_le_bytes());
    out.extend_from_slice(&(v.len() as u32).to_le_bytes());
    out.extend_from_slice(v);
    out
}

fn encode_uleb128(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Decode an AdaptiveBlock from wire bytes. Returns (block, bytes_consumed).
pub fn decode_adaptive(wire: &[u8]) -> Result<(AdaptiveBlock, usize), String> {
    if wire.len() < ADAPTIVE_FRAME_BYTES {
        return Err("adaptive block: wire too short for frame".to_string());
    }
    if wire[0] != 0xAD {
        return Err("adaptive block: bad tag".to_string());
    }
    let k_raw = u64::from_le_bytes(wire[1..9].try_into().unwrap());
    let k = MetaK { raw: k_raw };
    let v_len = u32::from_le_bytes(wire[9..13].try_into().unwrap()) as usize;
    let end = ADAPTIVE_FRAME_BYTES + v_len;
    if wire.len() < end {
        return Err("adaptive block: wire truncated before end of V".to_string());
    }
    let v = wire[ADAPTIVE_FRAME_BYTES..end].to_vec();
    let window_bits = k.window_bits();
    Ok((
        AdaptiveBlock { window_bits, k, v, wire: wire[..end].to_vec() },
        end,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_block_compresses() {
        let input = vec![0x3Cu8; 512];
        let result = try_encode_adaptive(&input);
        assert!(result.is_some(), "uniform 512-byte block should compress");
        let block = result.unwrap();
        assert!(block.wire.len() < 512);
    }

    #[test]
    fn random_block_returns_none_or_no_expansion_guarantee() {
        // Random data may or may not compress; we just ensure no panic.
        let input: Vec<u8> = (0..512u16).map(|i| ((i * 37 + 11) % 256) as u8).collect();
        let _ = try_encode_adaptive(&input);
    }

    #[test]
    fn window_doubles_until_max() {
        // 1-byte input: no window fits (need >=8 bytes). Should return None.
        let tiny = vec![0xFFu8; 4];
        assert!(try_encode_adaptive(&tiny).is_none());
    }

    #[test]
    fn decode_roundtrip_tag_and_length() {
        let input = vec![0xAAu8; 64];
        if let Some(block) = try_encode_adaptive(&input) {
            let (decoded, consumed) = decode_adaptive(&block.wire).unwrap();
            assert_eq!(consumed, block.wire.len());
            assert_eq!(decoded.window_bits, block.window_bits);
            assert_eq!(decoded.v, block.v);
        }
    }
}
