use crate::domain::kernel::spectral::normalized_fwht;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinaryOperatorKey {
    pub xor_mask: u64,
    pub rotate: u8,
    pub phase_mask: u64,
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

pub fn apply_binary_u_k(value: u64, key: BinaryOperatorKey) -> u64 {
    let rotate = key.rotate as u32 % 64;
    let mixed = (value ^ key.xor_mask).rotate_left(rotate);
    let spectral = binary_hadamard_forward(mixed);
    let reflected = spectral ^ key.phase_mask;
    binary_hadamard_inverse(reflected).rotate_right(rotate) ^ key.xor_mask
}

fn binary_hadamard_forward(mut value: u64) -> u64 {
    for half_width in [1_usize, 2, 4, 8, 16, 32] {
        value = cnot_stage(value, half_width);
    }
    value
}

fn binary_hadamard_inverse(mut value: u64) -> u64 {
    for half_width in [32_usize, 16, 8, 4, 2, 1] {
        value = cnot_stage(value, half_width);
    }
    value
}

fn cnot_stage(mut value: u64, half_width: usize) -> u64 {
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
        apply_binary_u_k, apply_fk, apply_fk_inverse, apply_phase_reflection, apply_u_k,
        BinaryOperatorKey, IndexMix, PhaseKey, SpectralOperatorKey,
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
    fn changing_k_changes_the_operator() {
        let input = (0..16).map(|value| value as f64).collect::<Vec<_>>();
        let left = apply_u_k(
            &input,
            &SpectralOperatorKey {
                mix: IndexMix {
                    xor_mask: 1,
                    rotate: 1,
                },
                phase: phase_key(),
            },
        )
        .unwrap();
        let right = apply_u_k(
            &input,
            &SpectralOperatorKey {
                mix: IndexMix {
                    xor_mask: 7,
                    rotate: 2,
                },
                phase: PhaseKey {
                    seed: 42,
                    ..phase_key()
                },
            },
        )
        .unwrap();
        assert_ne!(left, right);
    }

    #[test]
    fn binary_u_k_is_an_exact_involution_for_word_codec() {
        let key = BinaryOperatorKey {
            xor_mask: 0x0123_4567_89AB_CDEF,
            rotate: 17,
            phase_mask: 0xF0F0_0F0F_AAAA_5555,
        };
        for value in [0, 1, u64::MAX, 0xDEAD_BEEF_CAFE_BABE] {
            assert_eq!(apply_binary_u_k(apply_binary_u_k(value, key), key), value);
        }
    }
}
