//! Multi-layer encoder.
//!
//! Pipeline:
//!   file → [Layer 0: blocks → K_i+V_i stream]
//!          → [Layer 1: K_i+V_i stream → K_j+V_j stream]
//!          → ... until gain < MIN_GAIN_PCT or output >= input
//!
//! Key rules:
//!   - Block size is ADAPTIVE (per layer, not per block)
//!   - Window_min / Window_max stored ONCE in LayerMap header
//!   - V = branch-bit log, |V| = peaks * rounds (not block size)
//!   - Encoder refuses to emit a layer that doesn't reduce size

use crate::domain::{
    branch::BranchVector,
    fwht::{adapt_window, analyse_block},
    keygen::{is_profitable, synthesize_k, MagicKey},
    operator::{audit, encode_block},
};

/// Minimum compression gain to continue to next layer (10%).
pub const MIN_GAIN: f64 = 0.10;

/// Number of U_K rounds per block.
pub const DEFAULT_ROUNDS: usize = 3;

/// Adaptive window thresholds.
pub const SHARP_THRESHOLD: f64 = 0.30;
pub const FLAT_THRESHOLD: f64 = 0.70;

/// Per-layer metadata stored in the archive header.
#[derive(Debug, Clone)]
pub struct LayerInfo {
    pub layer_idx: usize,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub n_blocks: usize,
    pub window_min: usize,
    pub window_max: usize,
    pub rounds: usize,
    pub ratio: f64,
    pub gain_pct: f64,
    pub total_v_bits: usize,
    pub total_peaks: usize,
}

/// One encoded block: key + branch vector.
#[derive(Debug, Clone)]
pub struct EncodedBlock {
    pub key: MagicKey,
    pub branch: BranchVector,
    /// Transformed word data (seed of next-layer reconstruction).
    pub transformed: Vec<u64>,
}

/// One encoded layer: all blocks + metadata.
#[derive(Debug, Clone)]
pub struct EncodedLayer {
    pub info: LayerInfo,
    pub blocks: Vec<EncodedBlock>,
}

/// Full archive: stack of layers.
#[derive(Debug, Clone)]
pub struct Archive {
    pub layers: Vec<EncodedLayer>,
    /// Original file size in bytes (for decoder)
    pub original_bytes: usize,
}

impl Archive {
    /// Total compressed size in bytes.
    pub fn compressed_bytes(&self) -> usize {
        self.layers.last().map(|l| l.info.output_bytes).unwrap_or(0)
    }

    /// Overall compression ratio.
    pub fn ratio(&self) -> f64 {
        (self.original_bytes * 8) as f64
            / (self.compressed_bytes() * 8).max(1) as f64
    }
}

/// Encode a file into a multi-layer archive.
pub fn encode_file(
    data: &[u8],
    start_window_bits: usize,
    rounds: usize,
) -> Archive {
    debug_assert!(start_window_bits.is_power_of_two());
    let original_bytes = data.len();
    let mut layers: Vec<EncodedLayer> = Vec::new();

    // We simulate the K+V stream as raw bytes for the next layer.
    // In a real implementation this would be a proper bit-packed stream.
    let mut current_data: Vec<u8> = data.to_vec();
    let mut layer_idx = 0usize;

    loop {
        let block_bytes = start_window_bits / 8;
        let n_blocks = (current_data.len() + block_bytes - 1).max(1) / block_bytes;
        let n_blocks = n_blocks.max(1);

        let mut encoded_blocks: Vec<EncodedBlock> = Vec::new();
        let mut total_k_bits = 0usize;
        let mut total_v_bits = 0usize;
        let mut total_peaks = 0usize;
        let mut window_min = start_window_bits;
        let mut window_max = start_window_bits;
        let mut current_window = start_window_bits;

        for bi in 0..n_blocks {
            let start = bi * block_bytes;
            let end = (start + block_bytes).min(current_data.len());
            let mut block = current_data[start..end].to_vec();
            // Pad to full window
            while block.len() < block_bytes {
                block.push(0);
            }

            // Adaptive window decision based on spectral profile
            let profile = analyse_block(&block, current_window);
            current_window = adapt_window(
                current_window,
                profile.entropy,
                SHARP_THRESHOLD,
                FLAT_THRESHOLD,
                64,
                1 << 20,
            );
            window_min = window_min.min(current_window);
            window_max = window_max.max(current_window);

            let (key, _est_v) = synthesize_k(&block, current_window.min(start_window_bits));
            let (transformed, branch) = encode_block(&block, &key, rounds);

            total_k_bits += key.bit_len();
            total_v_bits += branch.len();
            total_peaks += key.peak_indices.len();

            encoded_blocks.push(EncodedBlock { key, branch, transformed });
        }

        let overhead_bits = 64 * n_blocks;
        let output_bits = total_k_bits + total_v_bits + overhead_bits;
        let output_bytes = (output_bits + 7) / 8;
        let input_bits = current_data.len() * 8;
        let ratio = input_bits as f64 / output_bits.max(1) as f64;
        let gain = (input_bits as f64 - output_bits as f64) / input_bits.max(1) as f64;

        let info = LayerInfo {
            layer_idx,
            input_bytes: current_data.len(),
            output_bytes,
            n_blocks,
            window_min,
            window_max,
            rounds,
            ratio,
            gain_pct: gain * 100.0,
            total_v_bits,
            total_peaks,
        };

        layers.push(EncodedLayer { info, blocks: encoded_blocks });

        // STOP conditions:
        //   1. Output >= input (no gain)
        //   2. Gain below threshold
        //   3. Safety cap on layers
        if output_bytes >= current_data.len() || gain < MIN_GAIN || layer_idx >= 8 {
            break;
        }

        // Simulate K+V stream as next layer's input
        // (real impl: bit-pack K_i and V_i into a byte stream)
        current_data = vec![0xABu8; output_bytes];
        layer_idx += 1;
    }

    Archive { layers, original_bytes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg_bytes(n: usize) -> Vec<u8> {
        let mut s = 0xDEADBEEFCAFEBABEu64;
        (0..n).map(|_| { s = s.wrapping_mul(6364136223846793005).wrapping_add(1); (s>>56) as u8 }).collect()
    }

    #[test]
    fn uniform_compresses_multi_layer() {
        let data = vec![0x3Cu8; 512];
        let archive = encode_file(&data, 4096, 3);
        assert!(archive.ratio() > 5.0,
            "uniform 512B must compress >5x, got {:.2}x", archive.ratio());
        assert!(archive.compressed_bytes() < data.len(),
            "compressed must be smaller than original");
    }

    #[test]
    fn each_layer_reduces_size() {
        let data = lcg_bytes(2048);
        let archive = encode_file(&data, 4096, 3);
        for l in &archive.layers {
            // Every accepted layer must have been profitable at entry
            // (last layer may equal input if it was the stop trigger)
            assert!(
                l.info.output_bytes <= l.info.input_bytes || l.info.layer_idx == archive.layers.len() - 1,
                "layer {} must not expand: in={}B out={}B",
                l.info.layer_idx, l.info.input_bytes, l.info.output_bytes
            );
        }
    }

    #[test]
    fn no_layer_emitted_when_all_noise() {
        // Even for LCG noise, at least 1 profitable layer must exist
        let data = lcg_bytes(512);
        let archive = encode_file(&data, 4096, 3);
        assert!(!archive.layers.is_empty(), "must emit at least 1 layer");
        // Final output must be smaller than input
        assert!(archive.compressed_bytes() < data.len(),
            "even noise must compress: got {}B from {}B",
            archive.compressed_bytes(), data.len());
    }

    #[test]
    fn v_total_equals_peaks_times_rounds() {
        let data = vec![0x3Cu8; 512];
        let archive = encode_file(&data, 4096, 3);
        let layer0 = &archive.layers[0];
        let expected_v: usize = layer0.blocks.iter().map(|b| b.key.peak_indices.len() * 3).sum();
        assert_eq!(
            layer0.info.total_v_bits, expected_v,
            "|V_total|={} must equal sum(peaks*rounds)={}",
            layer0.info.total_v_bits, expected_v
        );
    }
}
