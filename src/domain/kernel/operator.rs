use crate::domain::kernel::reversible::{
    feistel_forward, feistel_inverse, FeistelKey, FeistelRoundKey,
};
use crate::domain::kernel::spectral::normalized_fwht;

pub const OPERATOR_BLOCK_WORDS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexMix {
    pub xor_mask: usize,
    pub rotate: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhaseKey {
    pub seed: u64,
    pub mask: u64,
    pub shift_left: u8,
    pub shift_right: u8,
    pub odd_multiplier: u64,
}

impl PhaseKey {
    pub fn validate(&self) -> Result<(), String> {
        if self.shift_left == 0 || self.shift_left >= 64 {
            return Err("phase left shift must be in 1..64".to_string());
        }
        if self.shift_right == 0 || self.shift_right >= 64 {
            return Err("phase right shift must be in 1..64".to_string());
        }
        if self.odd_multiplier & 1 == 0 {
            return Err("phase multiplier must be odd".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpectralOperatorKey {
    pub mix: IndexMix,
    pub phase: PhaseKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinaryOperatorKey {
    pub feistel: FeistelKey,
    pub rotate: u8,
    pub phase_mask: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinaryWordPeak {
    pub bit: u8,
    pub positive: bool,
    pub bias: u16,
}

pub fn apply_phase_reflection(values: &mut [f64], key: &PhaseKey) -> Result<(), String> {
    key.validate()?;
    for (index, value) in values.iter_mut().enumerate() {
        if phase_bit(index as u64, key) == 1 {
            *value = -*value;
        }
    }
    Ok(())
}

pub fn apply_fk(values: &[f64], mix: IndexMix) -> Result<Vec<f64>, String> {
    validate_vector(values)?;
    let bit_width = values.len().ilog2();
    if mix.rotate as u32 >= bit_width {
        return Err("index rotation exceeds vector index width".to_string());
    }
    let mask = values.len() - 1;
    let mut out = vec![0.0; values.len()];
    for (index, value) in values.iter().copied().enumerate() {
        let target = rotate_index(index ^ (mix.xor_mask & mask), mix.rotate, bit_width);
        out[target] = value;
    }
    Ok(out)
}

pub fn apply_fk_inverse(values: &[f64], mix: IndexMix) -> Result<Vec<f64>, String> {
    validate_vector(values)?;
    let bit_width = values.len().ilog2();
    if mix.rotate as u32 >= bit_width {
        return Err("index rotation exceeds vector index width".to_string());
    }
    let mask = values.len() - 1;
    let mut out = vec![0.0; values.len()];
    for (index, value) in values.iter().copied().enumerate() {
        let unrotated = rotate_index(index, (bit_width - mix.rotate as u32) as u8, bit_width);
        let target = unrotated ^ (mix.xor_mask & mask);
        out[target] = value;
    }
    Ok(out)
}

pub fn apply_u_k(values: &[f64], key: &SpectralOperatorKey) -> Result<Vec<f64>, String> {
    let mut state = apply_fk(values, key.mix)?;
    normalized_fwht(&mut state)?;
    apply_phase_reflection(&mut state, &key.phase)?;
    normalized_fwht(&mut state)?;
    apply_fk_inverse(&state, key.mix)
}

pub fn apply_binary_u_k(value: u64, key: &BinaryOperatorKey) -> Result<u64, String> {
    key.feistel.validate()?;
    let rotate = key.rotate as u32 % 64;
    let mixed = feistel_forward(value, &key.feistel)?;
    let spectral = binary_hadamard_forward(mixed.rotate_left(rotate));
    let reflected = spectral ^ key.phase_mask;
    let restored = binary_hadamard_inverse(reflected).rotate_right(rotate);
    feistel_inverse(restored, &key.feistel)
}

pub fn strongest_binary_word_peaks(
    words: &[u64],
    count: usize,
) -> Result<Vec<BinaryWordPeak>, String> {
    if words.is_empty() {
        return Err("binary word peak analysis requires at least one word".to_string());
    }

    let transformed = words
        .iter()
        .copied()
        .map(binary_hadamard_forward)
        .collect::<Vec<_>>();
    let mut peaks = (0..64_u8)
        .map(|bit| {
            let ones = transformed
                .iter()
                .filter(|word| ((*word >> bit) & 1) == 1)
                .count();
            let zeros = transformed.len() - ones;
            BinaryWordPeak {
                bit,
                positive: ones >= zeros,
                bias: ones.abs_diff(zeros) as u16,
            }
        })
        .collect::<Vec<_>>();
    peaks.sort_by(|left, right| {
        right
            .bias
            .cmp(&left.bias)
            .then_with(|| left.bit.cmp(&right.bit))
    });
    peaks.truncate(count.min(peaks.len()));
    Ok(peaks)
}

pub fn cnot_stage(mut value: u64, half_width: usize) -> u64 {
    let block_width = half_width * 2;
    for block_start in (0..64).step_by(block_width) {
        for offset in 0..half_width {
            let control = block_start + offset;
            let target = control + half_width;
            let bit = (value >> control) & 1;
            value ^= bit << target;
        }
    }
    value
}

pub fn binary_hadamard_forward(mut value: u64) -> u64 {
    for half_width in [1_usize, 2, 4, 8, 16, 32] {
        value = cnot_stage(value, half_width);
    }
    value
}

pub fn binary_hadamard_inverse(mut value: u64) -> u64 {
    for half_width in [32_usize, 16, 8, 4, 2, 1] {
        value = cnot_stage(value, half_width);
    }
    value
}

pub fn apply_block_butterfly_forward(words: &mut [u64; OPERATOR_BLOCK_WORDS]) {
    for half_width in [1_usize, 2, 4, 8, 16, 32] {
        word_cnot_stage(words, half_width);
    }
}

pub fn apply_block_butterfly_inverse(words: &mut [u64; OPERATOR_BLOCK_WORDS]) {
    for half_width in [32_usize, 16, 8, 4, 2, 1] {
        word_cnot_stage(words, half_width);
    }
}

pub fn word_cnot_stage(words: &mut [u64; OPERATOR_BLOCK_WORDS], half_width: usize) {
    let block_width = half_width * 2;
    for block_start in (0..OPERATOR_BLOCK_WORDS).step_by(block_width) {
        for offset in 0..half_width {
            let control = block_start + offset;
            let target = control + half_width;
            words[target] ^= words[control];
        }
    }
}

pub fn default_feistel_from_seed(seed: u64, rounds: u8) -> FeistelKey {
    let round_count = rounds.max(1);
    let mut keys = Vec::with_capacity(round_count as usize);
    for round in 0..round_count {
        let stripe = seed.rotate_left((round as u32 * 9 + 7) % 64);
        let hi = (stripe >> 32) as u32;
        let lo = stripe as u32;
        keys.push(FeistelRoundKey {
            shift_left: ((stripe & 0x0F) as u8).max(1),
            shift_right: (((stripe >> 4) & 0x0F) as u8).max(1),
            rotate: ((stripe >> 8) & 0x1F) as u8,
            odd_multiplier: lo | 1,
            add: hi ^ 0x9E37_79B9_u32.rotate_left(round as u32),
            mask: hi.rotate_left(13) ^ lo.rotate_right(7) ^ 0xA5A5_5A5A,
        });
    }
    FeistelKey { rounds: keys }
}

fn phase_bit(index: u64, key: &PhaseKey) -> u8 {
    let mut value = index ^ key.seed;
    value ^= value.wrapping_shl(key.shift_left as u32);
    value = value.wrapping_mul(key.odd_multiplier);
    value ^= value.wrapping_shr(key.shift_right as u32);
    parity(value & key.mask)
}

fn parity(mut value: u64) -> u8 {
    value ^= value >> 32;
    value ^= value >> 16;
    value ^= value >> 8;
    value ^= value >> 4;
    ((0x6996_u16 >> (value & 0xF)) & 1) as u8
}

fn rotate_index(value: usize, rotate: u8, bit_width: u32) -> usize {
    let mask = (1_usize << bit_width) - 1;
    let shift = rotate as u32 % bit_width;
    if shift == 0 {
        return value & mask;
    }
    ((value << shift) | (value >> (bit_width - shift))) & mask
}

fn validate_vector(values: &[f64]) -> Result<(), String> {
    if values.len() < 2 || !values.len().is_power_of_two() {
        return Err("operator vector length must be a power of two >= 2".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        apply_binary_u_k, apply_block_butterfly_forward, apply_block_butterfly_inverse, apply_fk,
        apply_fk_inverse, apply_phase_reflection, apply_u_k, binary_hadamard_forward,
        default_feistel_from_seed, strongest_binary_word_peaks, BinaryOperatorKey, IndexMix,
        PhaseKey, SpectralOperatorKey, OPERATOR_BLOCK_WORDS,
    };

    fn phase_key() -> PhaseKey {
        PhaseKey {
            seed: 0x0123_4567_89AB_CDEF,
            mask: 0xF0F0_0F0F_AAAA_5555,
            shift_left: 13,
            shift_right: 7,
            odd_multiplier: 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn assert_close(left: &[f64], right: &[f64]) {
        assert_eq!(left.len(), right.len());
        for (index, (a, b)) in left.iter().zip(right).enumerate() {
            assert!((a - b).abs() < 1e-8, "index {index}: {a} != {b}");
        }
    }

    #[test]
    fn fk_index_mix_has_an_explicit_inverse() {
        let input = (0..16).map(|value| value as f64).collect::<Vec<_>>();
        let mix = IndexMix {
            xor_mask: 0b1011,
            rotate: 2,
        };
        let mixed = apply_fk(&input, mix).unwrap();
        let restored = apply_fk_inverse(&mixed, mix).unwrap();
        assert_eq!(restored, input);
    }

    #[test]
    fn d_k_phase_reflection_is_an_involution() {
        let original = (0..16).map(|value| value as f64 - 8.0).collect::<Vec<_>>();
        let mut reflected = original.clone();
        apply_phase_reflection(&mut reflected, &phase_key()).unwrap();
        apply_phase_reflection(&mut reflected, &phase_key()).unwrap();
        assert_eq!(reflected, original);
    }

    #[test]
    fn u_k_is_self_inverse_for_multiple_keys_and_states() {
        for seed in [1_u64, 7, 0xDEAD_BEEF] {
            let key = SpectralOperatorKey {
                mix: IndexMix {
                    xor_mask: seed as usize,
                    rotate: (seed % 3 + 1) as u8,
                },
                phase: PhaseKey {
                    seed,
                    ..phase_key()
                },
            };
            let input = (0..32)
                .map(|index| {
                    (((index as u64).wrapping_mul(seed | 1)).rotate_left(index % 17) & 0xFF) as f64
                        - 127.0
                })
                .collect::<Vec<_>>();
            let once = apply_u_k(&input, &key).unwrap();
            let twice = apply_u_k(&once, &key).unwrap();
            assert_close(&twice, &input);
        }
    }

    #[test]
    fn binary_u_k_is_an_exact_involution_for_word_codec() {
        let key = BinaryOperatorKey {
            feistel: default_feistel_from_seed(0x0123_4567_89AB_CDEF, 3),
            rotate: 17,
            phase_mask: 0xF0F0_0F0F_AAAA_5555,
        };
        for value in [0, 1, u64::MAX, 0xDEAD_BEEF_CAFE_BABE] {
            assert_eq!(
                apply_binary_u_k(apply_binary_u_k(value, &key).unwrap(), &key).unwrap(),
                value
            );
        }
    }

    #[test]
    fn binary_u_k_is_not_a_constant_xor_translation() {
        let key = BinaryOperatorKey {
            feistel: default_feistel_from_seed(0xA5A5_5A5A_DEAD_BEEF, 4),
            rotate: 9,
            phase_mask: 0x00FF_F0F0_0F0F_FF00,
        };
        let a = apply_binary_u_k(0x0123_4567_89AB_CDEF, &key).unwrap() ^ 0x0123_4567_89AB_CDEF;
        let b = apply_binary_u_k(0x1123_4567_89AB_CDEE, &key).unwrap() ^ 0x1123_4567_89AB_CDEE;
        assert_ne!(a, b);
    }

    #[test]
    fn block_butterfly_has_an_explicit_inverse() {
        let mut state = [0_u64; OPERATOR_BLOCK_WORDS];
        for (index, word) in state.iter_mut().enumerate() {
            *word = (index as u64).rotate_left((index % 17) as u32);
        }
        let original = state;
        apply_block_butterfly_forward(&mut state);
        apply_block_butterfly_inverse(&mut state);
        assert_eq!(state, original);
    }

    #[test]
    fn binary_word_peaks_are_computed_in_the_same_runtime_space() {
        let words = [0_u64, 0_u64, u64::MAX, u64::MAX];
        let peaks = strongest_binary_word_peaks(&words, 4).unwrap();
        assert!(!peaks.is_empty());
        assert!(peaks[0].bias > 0);
        assert_ne!(
            binary_hadamard_forward(words[0]),
            binary_hadamard_forward(words[2])
        );
    }
}
