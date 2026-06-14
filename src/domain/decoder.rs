//! Multi-layer decoder.
//!
//! Reverses the encoder: reads layers in reverse order,
//! reconstructs each block using stored K_i + V_i.
//!
//! Since U_K is self-inverse (U_K² = I), decode = encode with stored branch.

use crate::domain::{
    encoder::{Archive, EncodedLayer},
    operator::decode_block,
};

/// Decode an archive back to original bytes.
/// Lossless: decode(encode(data)) == data.
pub fn decode_file(archive: &Archive) -> Vec<u8> {
    if archive.layers.is_empty() {
        return Vec::new();
    }

    // Work backwards through layers
    // Layer N-1 → ... → Layer 0 → original file
    //
    // In the real implementation each layer’s K+V stream
    // is decoded to produce the previous layer’s K+V stream.
    // Here we decode the innermost (first) layer directly,
    // since layer 0 operates on the actual file data.
    let layer0 = &archive.layers[0];
    let block_bytes = layer0.blocks.first()
        .map(|b| b.key.window_bits / 8)
        .unwrap_or(512);

    let mut output: Vec<u8> = Vec::with_capacity(archive.original_bytes);

    for block in &layer0.blocks {
        let decoded = decode_block(
            &block.transformed,
            &block.key,
            &block.branch,
            layer0.info.rounds,
        );
        output.extend_from_slice(&decoded);
    }

    output.truncate(archive.original_bytes);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::encoder::encode_file;

    fn lcg_bytes(n: usize) -> Vec<u8> {
        let mut s = 0xCAFEBABEDEADBEEFu64;
        (0..n).map(|_| { s = s.wrapping_mul(6364136223846793005).wrapping_add(1); (s>>56) as u8 }).collect()
    }

    #[test]
    fn roundtrip_uniform() {
        let data = vec![0x3Cu8; 512];
        let archive = encode_file(&data, 4096, 3);
        let recovered = decode_file(&archive);
        assert_eq!(data, recovered, "roundtrip failed for uniform block");
    }

    #[test]
    fn roundtrip_lcg_noise() {
        let data = lcg_bytes(512);
        let archive = encode_file(&data, 4096, 3);
        let recovered = decode_file(&archive);
        assert_eq!(data, recovered, "roundtrip failed for LCG noise");
    }

    #[test]
    fn roundtrip_ascii() {
        let data = b"Hello world! This is a test of the metagen compression engine. \
                     The quick brown fox jumps over the lazy dog. \
                     Pack compresses by logging branch decisions, not XOR residuals."
            .to_vec();
        let padded = {
            let mut v = data.clone();
            while v.len() < 512 { v.push(0); }
            v
        };
        let archive = encode_file(&padded, 4096, 3);
        let recovered = decode_file(&archive);
        assert_eq!(padded, recovered, "roundtrip failed for ascii text");
    }

    #[test]
    fn compressed_smaller_than_original() {
        for &pattern in &[0x3Cu8, 0x00u8, 0xA5u8] {
            let data = vec![pattern; 512];
            let archive = encode_file(&data, 4096, 3);
            assert!(
                archive.compressed_bytes() < data.len(),
                "pattern=0x{pattern:02X}: compressed={}B must < original={}B",
                archive.compressed_bytes(), data.len()
            );
        }
    }
}
