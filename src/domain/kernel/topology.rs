// FILE: src/domain/kernel/topology.rs
//
// Derives codec keys deterministically from a block's Walsh spectrum.
// This is the true meta-generator: block statistics → key, no search required.

use crate::domain::kernel::key::{MagicKey, OperatorBlueprint, SpectralPeakCode, SpectralProgram};
use crate::domain::kernel::spectral::{normalized_fwht, strongest_walsh_peaks};

// ── Public types ─────────────────────────────────────────────────────────────

/// Cached result of the O(n log n) Walsh transform + derived statistics.
/// Computed once per block in `encode_block` and threaded through all encoders.
#[derive(Clone, Debug, PartialEq)]
pub struct TopologySignature {
    /// Top Walsh peaks sorted by |coefficient|, strongest first.
    pub walsh_peaks: Vec<WalshPeak>,
    /// Per-shift XOR-spread scores, used to pick `primary_shift`.
    /// `shift_scores[k]` = quality score for shift amount k+1.
    pub shift_scores: Vec<u8>,
    /// 48-bit fingerprint: XOR-fold of block bytes for trajectory key seeding.
    pub fingerprint: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WalshPeak {
    pub index:       usize,
    pub coefficient: f64,
}

// ── Core analysis ─────────────────────────────────────────────────────────────

/// Run the normalised FWHT on `block` (interpreted as ±1 signal) and return
/// the derived `TopologySignature`.  `block.len() * 8` must be a non-zero
/// power of two.
pub fn analyze_topology(block: &[u8]) -> Result<TopologySignature, String> {
    let bit_len = block.len() * 8;
    if bit_len == 0 || !bit_len.is_power_of_two() {
        return Err(format!(
            "analyze_topology: bit_len {bit_len} must be a non-zero power of two"
        ));
    }

    // Convert bytes to ±1 f64 signal
    let mut signal: Vec<f64> = block
        .iter()
        .flat_map(|&b| (0..8u8).map(move |i| if (b >> i) & 1 == 1 { 1.0 } else { -1.0 }))
        .collect();

    normalized_fwht(&mut signal)?;

    let raw_peaks = strongest_walsh_peaks(&signal, 8);
    let walsh_peaks: Vec<WalshPeak> = raw_peaks
        .iter()
        .map(|p| WalshPeak { index: p.index, coefficient: p.coefficient })
        .collect();

    // Shift scores: for shift k+1, score = popcount spread heuristic
    let shift_scores: Vec<u8> = (1_u8..=32).map(|shift| {
        let spread: u64 = block
            .iter()
            .zip(block.iter().cycle().skip(shift as usize))
            .map(|(&a, &b)| u64::from((a ^ b).count_ones() as u8))
            .sum();
        // Normalise into 0..=255
        (spread.min(255 * block.len() as u64) / block.len().max(1) as u64) as u8
    }).collect();

    // 48-bit XOR-fold fingerprint
    let fingerprint = block
        .chunks(6)
        .fold(0_u64, |acc, chunk| {
            let mut word = 0_u64;
            for (i, &b) in chunk.iter().enumerate() {
                word |= (b as u64) << (i * 8);
            }
            acc ^ word
        })
        & 0x0000_FFFF_FFFF_FFFF;

    Ok(TopologySignature { walsh_peaks, shift_scores, fingerprint })
}

// ── Key compilers ─────────────────────────────────────────────────────────────

/// Derive an `OperatorBlueprint`-tagged `MagicKey` from a `TopologySignature`.
/// Called by the operator encoder path.
pub fn compile_topology_to_key(sig: &TopologySignature) -> Result<MagicKey, String> {
    if sig.walsh_peaks.is_empty() {
        return Err("compile_topology_to_key: signature has no Walsh peaks".to_string());
    }

    let p0 = &sig.walsh_peaks[0];
    let p1 = sig.walsh_peaks.get(1).unwrap_or(p0);

    let primary_shift = sig
        .shift_scores
        .iter()
        .copied()
        .enumerate()
        .max_by_key(|&(_, s)| s)
        .map(|(i, _)| (i + 1).min(31).max(1) as u8)
        .unwrap_or(1);

    let secondary_delta = ((p1.index.wrapping_sub(p0.index)) % 32) as u8;
    let tertiary_delta  = ((p0.index.wrapping_add(p1.index)) % 32) as u8;

    let bp = OperatorBlueprint {
        dominant_index:     (p0.index & 0x3F) as u16,
        dominant_positive:  p0.coefficient > 0.0,
        primary_shift,
        shift_match:        (p1.index & 0x1FF) as u16,
        derivative_density: ((sig.fingerprint >> 8)  & 0xFF) as u8,
        popcnt_density:     ((sig.fingerprint >> 16) & 0xFF) as u8,
        secondary_delta,
        tertiary_delta,
        fingerprint_bias:   (sig.fingerprint & 0x1F) as u8,
    };

    MagicKey::from_operator_blueprint(&bp)
}

/// Derive a `Spectral`-tagged `MagicKey` from a `TopologySignature`.
/// Called by the spectral encoder path.
pub fn compile_spectral_key(sig: &TopologySignature) -> Result<MagicKey, String> {
    if sig.walsh_peaks.is_empty() {
        return Err("compile_spectral_key: signature has no Walsh peaks".to_string());
    }

    // Determine bit_len from fingerprint / peaks — we need it to build the program.
    // Caller guarantees block length is known; we recover log2 from shift_scores length.
    // shift_scores has 32 entries always; bit_len is block.len()*8.
    // We embed it via the peak indices directly (they are < bit_len by construction).
    let bit_len_log2 = {
        // The largest peak index gives a lower bound; we round up to next power of two.
        let max_idx = sig.walsh_peaks.iter().map(|p| p.index).max().unwrap_or(0);
        let lower = (max_idx + 1).next_power_of_two().max(8);
        lower.ilog2() as usize
    };
    let bit_len = 1_usize << bit_len_log2;

    let peaks: Vec<SpectralPeakCode> = sig.walsh_peaks
        .iter()
        .take(2)
        .map(|p| SpectralPeakCode {
            index:     p.index.min(bit_len - 1),
            positive:  p.coefficient > 0.0,
            amplitude: (p.coefficient.abs().min(63.0) as u8).max(1),
        })
        .collect();

    let tie_bit = sig.walsh_peaks.first().map(|p| p.coefficient > 0.0).unwrap_or(false);

    MagicKey::from_spectral_program(&SpectralProgram { bit_len, peaks, tie_bit })
}

/// Derive a `Trajectory`-tagged `MagicKey` from a raw block.
/// (Block fingerprint + step count.)
pub fn block_fingerprint(block: &[u8]) -> Result<u64, String> {
    if block.is_empty() {
        return Err("block_fingerprint: empty block".to_string());
    }
    let fp = block
        .chunks(6)
        .fold(0_u64, |acc, chunk| {
            let mut word = 0_u64;
            for (i, &b) in chunk.iter().enumerate() {
                word |= (b as u64) << (i * 8);
            }
            acc ^ word
        })
        & 0x0000_FFFF_FFFF_FFFF;
    Ok(fp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::kernel::key::MagicKeyKind;

    fn uniform_block(byte: u8, len: usize) -> Vec<u8> {
        vec![byte; len]
    }

    fn sequential_block(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    #[test]
    fn analyze_topology_rejects_non_power_of_two_bit_len() {
        assert!(analyze_topology(&[0u8; 3]).is_err());
        assert!(analyze_topology(&[0u8; 0]).is_err());
    }

    #[test]
    fn analyze_topology_accepts_valid_blocks() {
        for &len in &[1usize, 8, 64, 512] {
            let block = sequential_block(len);
            assert!(analyze_topology(&block).is_ok(), "failed for len={len}");
        }
    }

    #[test]
    fn compile_topology_to_key_returns_operator_kind() {
        let block = sequential_block(64);
        let sig = analyze_topology(&block).unwrap();
        let key = compile_topology_to_key(&sig).unwrap();
        assert!(key.require_kind(MagicKeyKind::Operator).is_ok());
    }

    #[test]
    fn compile_spectral_key_returns_spectral_kind() {
        let block = sequential_block(64);
        let sig = analyze_topology(&block).unwrap();
        let key = compile_spectral_key(&sig).unwrap();
        assert!(key.require_kind(MagicKeyKind::Spectral).is_ok());
    }

    #[test]
    fn different_blocks_produce_different_topology_keys() {
        let left  = sequential_block(64);
        let right: Vec<u8> = (0..64).map(|i| (i as u8).wrapping_mul(7)).collect();
        let lk = compile_topology_to_key(&analyze_topology(&left).unwrap()).unwrap();
        let rk = compile_topology_to_key(&analyze_topology(&right).unwrap()).unwrap();
        assert_ne!(lk, rk);
    }

    #[test]
    fn block_fingerprint_is_deterministic() {
        let b = sequential_block(64);
        assert_eq!(block_fingerprint(&b).unwrap(), block_fingerprint(&b).unwrap());
    }

    #[test]
    fn block_fingerprint_rejects_empty() {
        assert!(block_fingerprint(&[]).is_err());
    }

    #[test]
    fn topology_shift_scores_has_32_entries() {
        let sig = analyze_topology(&sequential_block(64)).unwrap();
        assert_eq!(sig.shift_scores.len(), 32);
    }
}
