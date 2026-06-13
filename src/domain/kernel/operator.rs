use crate::domain::kernel::key::{ProgramCode, ProgramIR, ProgramStage, StageOpcode};
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePipeline {
    pub program: ProgramIR,
    pub window_bits: usize,
}

pub fn materialize_runtime(
    code: &ProgramCode,
    window_bits: usize,
) -> Result<RuntimePipeline, String> {
    if window_bits == 0 || !window_bits.is_power_of_two() {
        return Err("runtime window must be a non-zero power of two".to_string());
    }
    if window_bits % 64 != 0 {
        return Err("runtime window must be divisible by 64 bits".to_string());
    }
    Ok(RuntimePipeline {
        program: code.program()?,
        window_bits,
    })
}

pub fn contract_block(
    runtime: &RuntimePipeline,
    block: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), String> {
    if block.len() * 8 != runtime.window_bits {
        return Err("runtime window does not match block length".to_string());
    }
    if block.len() % 8 != 0 {
        return Err("runtime block must be divisible by 8 bytes".to_string());
    }
    let mut words = block
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let mut live_bits = 64_usize;
    let mut branches = Vec::new();
    for stage in &runtime.program.stages {
        match stage.opcode {
            StageOpcode::HadamardAxis => apply_stage_hadamard_axis(&mut words, live_bits, *stage),
            StageOpcode::PhaseProject => apply_stage_phase_project(&mut words, live_bits, *stage),
            StageOpcode::GF2Recurrence => {
                apply_stage_gf2_recurrence(&mut words, live_bits, *stage)?
            }
            StageOpcode::OrbitFold => apply_stage_orbit_fold(&mut words, live_bits, *stage),
            StageOpcode::LanePermute => apply_stage_lane_permute(&mut words, *stage),
            StageOpcode::BranchGate => {
                if live_bits <= 1 {
                    return Err("branch gate exhausted the word bit budget".to_string());
                }
                for (index, word) in words.iter_mut().enumerate() {
                    let position = branch_position(*stage, index, live_bits);
                    branches.push(((word.to_owned() >> position) & 1) == 1);
                    *word = remove_bit_at(*word, position);
                    *word &= word_mask(live_bits - 1);
                }
                live_bits -= 1;
            }
            StageOpcode::ParityProject => apply_stage_parity_project(&mut words, live_bits, *stage),
            StageOpcode::Halt => break,
        }
    }
    let seed = pack_words_with_width(&words, live_bits);
    Ok((seed, pack_bools_local(&branches)))
}

pub fn expand_block(
    runtime: &RuntimePipeline,
    seed: &[u8],
    branches: &[u8],
) -> Result<Vec<u8>, String> {
    let total_branch_gates = runtime
        .program
        .stages
        .iter()
        .filter(|stage| stage.opcode == StageOpcode::BranchGate)
        .count();
    if total_branch_gates >= 64 {
        return Err("program contains too many branch gates".to_string());
    }
    let final_live_bits = 64 - total_branch_gates;
    let word_count = runtime.window_bits / 64;
    let mut words = unpack_words_with_width(seed, word_count, final_live_bits)?;
    let branch_count = total_branch_gates
        .checked_mul(word_count)
        .ok_or_else(|| "branch count overflow".to_string())?;
    let crumbs = unpack_bools_local(branches, branch_count)?;
    let mut branch_cursor = crumbs.len();
    let mut live_bits = final_live_bits;
    for stage in runtime.program.stages.iter().rev() {
        match stage.opcode {
            StageOpcode::Halt => {}
            StageOpcode::ParityProject => apply_stage_parity_project(&mut words, live_bits, *stage),
            StageOpcode::BranchGate => {
                live_bits += 1;
                for index in (0..words.len()).rev() {
                    branch_cursor = branch_cursor
                        .checked_sub(1)
                        .ok_or_else(|| "branch log underflow".to_string())?;
                    let position = branch_position(*stage, index, live_bits);
                    words[index] = insert_bit_at(words[index], position, crumbs[branch_cursor]);
                    words[index] &= word_mask(live_bits);
                }
            }
            StageOpcode::LanePermute => apply_stage_lane_permute_inverse(&mut words, *stage),
            StageOpcode::OrbitFold => apply_stage_orbit_fold(&mut words, live_bits, *stage),
            StageOpcode::GF2Recurrence => {
                apply_stage_gf2_recurrence_inverse(&mut words, live_bits, *stage)?
            }
            StageOpcode::PhaseProject => apply_stage_phase_project(&mut words, live_bits, *stage),
            StageOpcode::HadamardAxis => apply_stage_hadamard_axis(&mut words, live_bits, *stage),
        }
    }
    if branch_cursor != 0 {
        return Err("branch log contains trailing bits".to_string());
    }
    Ok(words
        .into_iter()
        .flat_map(u64::to_le_bytes)
        .collect::<Vec<_>>())
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

fn apply_stage_hadamard_axis(words: &mut [u64], live_bits: usize, stage: ProgramStage) {
    let live_mask = word_mask(live_bits);
    for (index, word) in words.iter_mut().enumerate() {
        let bit = ((usize::from(stage.arg0) + index * (usize::from(stage.arg1) + 1))
            % live_bits.max(1)) as u32;
        *word ^= 1_u64 << bit;
        *word &= live_mask;
    }
}

fn apply_stage_phase_project(words: &mut [u64], live_bits: usize, stage: ProgramStage) {
    let live_mask = word_mask(live_bits);
    let mask = repeat_byte(stage.arg0).rotate_left(u32::from(stage.arg1));
    for word in words.iter_mut() {
        *word ^= mask & live_mask;
    }
}

fn apply_stage_gf2_recurrence(
    words: &mut [u64],
    live_bits: usize,
    stage: ProgramStage,
) -> Result<(), String> {
    let shift = shift_from_stage(stage, live_bits)?;
    let live_mask = word_mask(live_bits);
    for word in words.iter_mut() {
        *word ^= (*word >> shift) & live_mask;
        *word &= live_mask;
    }
    Ok(())
}

fn apply_stage_gf2_recurrence_inverse(
    words: &mut [u64],
    live_bits: usize,
    stage: ProgramStage,
) -> Result<(), String> {
    let shift = shift_from_stage(stage, live_bits)?;
    let live_mask = word_mask(live_bits);
    for word in words.iter_mut() {
        let mut restored = *word & live_mask;
        let mut delta = shift;
        while delta < live_bits {
            restored ^= restored >> delta;
            restored &= live_mask;
            delta *= 2;
        }
        *word = restored;
    }
    Ok(())
}

fn apply_stage_orbit_fold(words: &mut [u64], live_bits: usize, stage: ProgramStage) {
    let live_mask = word_mask(live_bits);
    let mask = repeat_byte(stage.arg1).rotate_left(u32::from(stage.arg0));
    for (index, word) in words.iter_mut().enumerate() {
        *word ^= mask.rotate_left((index as u32 + u32::from(stage.arg0)) & 63) & live_mask;
        *word &= live_mask;
    }
}

fn apply_stage_lane_permute(words: &mut [u64], stage: ProgramStage) {
    if words.is_empty() {
        return;
    }
    let rotate = usize::from(stage.arg0) % words.len();
    words.rotate_left(rotate);
}

fn apply_stage_lane_permute_inverse(words: &mut [u64], stage: ProgramStage) {
    if words.is_empty() {
        return;
    }
    let rotate = usize::from(stage.arg0) % words.len();
    words.rotate_right(rotate);
}

fn apply_stage_parity_project(words: &mut [u64], live_bits: usize, stage: ProgramStage) {
    let live_mask = word_mask(live_bits);
    let mask = repeat_byte(stage.arg0 ^ stage.arg1).rotate_left(7);
    for (index, word) in words.iter_mut().enumerate() {
        let parity = ((word.count_ones() + index as u32) & 1) as u64;
        *word ^= (mask & live_mask) * parity;
        *word &= live_mask;
    }
}

fn shift_from_stage(stage: ProgramStage, live_bits: usize) -> Result<usize, String> {
    let shift = usize::from(stage.arg0 % (live_bits as u8).max(2)).max(1);
    if shift >= live_bits {
        return Err("gf2 recurrence shift exceeds the live word width".to_string());
    }
    Ok(shift)
}

fn branch_position(stage: ProgramStage, word_index: usize, live_bits: usize) -> usize {
    (usize::from(stage.arg0) + word_index * (usize::from(stage.arg1) + 1)) % live_bits.max(1)
}

fn word_mask(live_bits: usize) -> u64 {
    if live_bits >= 64 {
        u64::MAX
    } else if live_bits == 0 {
        0
    } else {
        (1_u64 << live_bits) - 1
    }
}

fn remove_bit_at(value: u64, position: usize) -> u64 {
    let low_mask = if position == 0 {
        0
    } else {
        (1_u64 << position) - 1
    };
    let low = value & low_mask;
    let high = if position >= 63 {
        0
    } else {
        value >> (position + 1)
    };
    low | (high << position)
}

fn insert_bit_at(value: u64, position: usize, bit: bool) -> u64 {
    let low_mask = if position == 0 {
        0
    } else {
        (1_u64 << position) - 1
    };
    let low = value & low_mask;
    let high = if position >= 63 { 0 } else { value >> position };
    low | (u64::from(bit) << position)
        | if position >= 63 {
            0
        } else {
            high << (position + 1)
        }
}

fn pack_words_with_width(words: &[u64], live_bits: usize) -> Vec<u8> {
    let bytes_per_word = live_bits.div_ceil(8);
    let mut out = Vec::with_capacity(words.len() * bytes_per_word);
    for word in words {
        out.extend_from_slice(&word.to_le_bytes()[..bytes_per_word]);
    }
    out
}

fn unpack_words_with_width(
    bytes: &[u8],
    word_count: usize,
    live_bits: usize,
) -> Result<Vec<u64>, String> {
    let bytes_per_word = live_bits.div_ceil(8);
    let expected = word_count
        .checked_mul(bytes_per_word)
        .ok_or_else(|| "seed length overflow".to_string())?;
    if bytes.len() != expected {
        return Err("seed length does not match the live word width".to_string());
    }
    let mut words = Vec::with_capacity(word_count);
    for chunk in bytes.chunks_exact(bytes_per_word) {
        let mut encoded = [0_u8; 8];
        encoded[..bytes_per_word].copy_from_slice(chunk);
        words.push(u64::from_le_bytes(encoded) & word_mask(live_bits));
    }
    Ok(words)
}

fn pack_bools_local(bits: &[bool]) -> Vec<u8> {
    let mut out = vec![0_u8; bits.len().div_ceil(8)];
    for (index, bit) in bits.iter().enumerate() {
        if *bit {
            out[index / 8] |= 1 << (index % 8);
        }
    }
    out
}

fn unpack_bools_local(bytes: &[u8], count: usize) -> Result<Vec<bool>, String> {
    let expected = count.div_ceil(8);
    if bytes.len() != expected {
        return Err("branch-log length does not match the expected bit count".to_string());
    }
    Ok((0..count)
        .map(|index| ((bytes[index / 8] >> (index % 8)) & 1) == 1)
        .collect())
}

fn repeat_byte(byte: u8) -> u64 {
    u64::from_le_bytes([byte; 8])
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
