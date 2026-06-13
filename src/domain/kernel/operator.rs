use std::collections::{BTreeMap, BTreeSet};

use crate::domain::bitstream::{bit_width_for_cardinality, pack_indices, unpack_indices};
use crate::domain::kernel::key::{ConstantFamily, ConstantLayout, PackedConstantK, RoutingKind};
use crate::domain::kernel::reversible::{
    feistel_forward, feistel_inverse, FeistelKey, FeistelRoundKey,
};
use crate::domain::kernel::spectral::normalized_fwht;

pub const OPERATOR_BLOCK_WORDS: usize = 64;
const SEED_MODE_RAW: u8 = 0;
const SEED_MODE_PALETTE: u8 = 1;

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
    pub layout: ConstantLayout,
    pub window_bits: usize,
    pub word_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BranchGeometry {
    pivot: usize,
    full_mask: u64,
}

pub fn materialize_runtime(
    code: &PackedConstantK,
    window_bits: usize,
) -> Result<RuntimePipeline, String> {
    if window_bits == 0 || !window_bits.is_power_of_two() {
        return Err("runtime window must be a non-zero power of two".to_string());
    }
    if window_bits % 64 != 0 {
        return Err("runtime window must be divisible by 64 bits".to_string());
    }
    Ok(RuntimePipeline {
        layout: code.layout()?,
        window_bits,
        word_count: window_bits / 64,
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
    apply_fixed_pipeline_forward(&mut words, &runtime.layout);
    let mut live_bits = 64_usize;
    let mut branches =
        Vec::with_capacity(runtime.word_count * runtime.layout.branch_rounds as usize);
    for round in 0..runtime.layout.branch_rounds as usize {
        if live_bits <= 1 {
            return Err("branch contraction exhausted the word width".to_string());
        }
        for (index, word) in words.iter_mut().enumerate() {
            let geometry = branch_geometry(&runtime.layout, index, round, live_bits);
            let branch = parity(*word & geometry.full_mask) == 1;
            branches.push(branch);
            *word = remove_bit_at(*word, geometry.pivot) & word_mask(live_bits - 1);
        }
        live_bits -= 1;
    }
    let seed = encode_terminal_seed(&words, live_bits)?;
    Ok((seed, pack_bools_local(&branches)))
}

pub fn expand_block(
    runtime: &RuntimePipeline,
    seed: &[u8],
    branches: &[u8],
) -> Result<Vec<u8>, String> {
    let total_branch_bits = runtime.word_count * runtime.layout.branch_rounds as usize;
    let crumbs = unpack_bools_local(branches, total_branch_bits)?;
    let final_live_bits = 64 - runtime.layout.branch_rounds as usize;
    let mut words = decode_terminal_seed(seed, runtime.word_count, final_live_bits)?;
    let mut cursor = crumbs.len();
    let mut live_bits = final_live_bits;
    for round in (0..runtime.layout.branch_rounds as usize).rev() {
        live_bits += 1;
        for index in (0..words.len()).rev() {
            cursor = cursor
                .checked_sub(1)
                .ok_or_else(|| "branch log underflow".to_string())?;
            let geometry = branch_geometry(&runtime.layout, index, round, live_bits);
            let reduced_mask = remove_bit_at(geometry.full_mask, geometry.pivot);
            let branch = u8::from(crumbs[cursor]);
            let pivot = (branch ^ parity(words[index] & reduced_mask)) == 1;
            words[index] =
                insert_bit_at(words[index], geometry.pivot, pivot) & word_mask(live_bits);
        }
    }
    if cursor != 0 {
        return Err("branch log contains trailing bits".to_string());
    }
    apply_fixed_pipeline_inverse(&mut words, &runtime.layout)?;
    Ok(words.into_iter().flat_map(u64::to_le_bytes).collect())
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
    apply_slice_butterfly_forward(words);
}

pub fn apply_block_butterfly_inverse(words: &mut [u64; OPERATOR_BLOCK_WORDS]) {
    apply_slice_butterfly_inverse(words);
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

fn apply_fixed_pipeline_forward(words: &mut [u64], layout: &ConstantLayout) {
    apply_routing_forward(words, layout);
    if layout.routing == RoutingKind::Butterfly {
        apply_slice_butterfly_forward(words);
    }
    for (index, word) in words.iter_mut().enumerate() {
        *word = word_forward_mix(*word, layout, index);
    }
}

fn apply_fixed_pipeline_inverse(words: &mut [u64], layout: &ConstantLayout) -> Result<(), String> {
    for (index, word) in words.iter_mut().enumerate() {
        *word = word_inverse_mix(*word, layout, index)?;
    }
    if layout.routing == RoutingKind::Butterfly {
        apply_slice_butterfly_inverse(words);
    }
    apply_routing_inverse(words, layout);
    Ok(())
}

fn word_forward_mix(mut value: u64, layout: &ConstantLayout, word_index: usize) -> u64 {
    let phase_mask = per_word_mask(layout.phase_mask, word_index, layout.rotate_right);
    value ^= phase_mask;
    value = value.rotate_left(layout.rotate_left as u32);
    value = value.wrapping_mul(layout.odd_multiplier);
    value = value.rotate_right(layout.rotate_right as u32);
    value = gf2_right_mix_forward(value, layout.gf2_shift);
    if let Some(mask) = layout.affine_mask {
        value ^= per_word_mask(mask, word_index, layout.rotate_left);
    }
    match layout.family {
        ConstantFamily::PhaseXor => value,
        ConstantFamily::OddAffine => {
            value.rotate_left(((word_index + usize::from(layout.lane_rotate)) % 63 + 1) as u32)
        }
        ConstantFamily::Hybrid => value ^ phase_mask.rotate_right(layout.rotate_right as u32),
    }
}

fn word_inverse_mix(
    mut value: u64,
    layout: &ConstantLayout,
    word_index: usize,
) -> Result<u64, String> {
    let phase_mask = per_word_mask(layout.phase_mask, word_index, layout.rotate_right);
    value = match layout.family {
        ConstantFamily::PhaseXor => value,
        ConstantFamily::OddAffine => {
            value.rotate_right(((word_index + usize::from(layout.lane_rotate)) % 63 + 1) as u32)
        }
        ConstantFamily::Hybrid => value ^ phase_mask.rotate_right(layout.rotate_right as u32),
    };
    if let Some(mask) = layout.affine_mask {
        value ^= per_word_mask(mask, word_index, layout.rotate_left);
    }
    value = gf2_right_mix_inverse(value, layout.gf2_shift);
    value = value.rotate_left(layout.rotate_right as u32);
    value = value.wrapping_mul(mod_inverse_odd_u64(layout.odd_multiplier));
    value = value.rotate_right(layout.rotate_left as u32);
    Ok(value ^ phase_mask)
}

fn apply_routing_forward(words: &mut [u64], layout: &ConstantLayout) {
    if words.is_empty() {
        return;
    }
    match layout.routing {
        RoutingKind::Identity | RoutingKind::Butterfly => {}
        RoutingKind::RotateWords => {
            words.rotate_left(usize::from(layout.lane_rotate) % words.len())
        }
        RoutingKind::ReverseWords => words.reverse(),
    }
}

fn apply_routing_inverse(words: &mut [u64], layout: &ConstantLayout) {
    if words.is_empty() {
        return;
    }
    match layout.routing {
        RoutingKind::Identity | RoutingKind::Butterfly => {}
        RoutingKind::RotateWords => {
            words.rotate_right(usize::from(layout.lane_rotate) % words.len())
        }
        RoutingKind::ReverseWords => words.reverse(),
    }
}

fn apply_slice_butterfly_forward(words: &mut [u64]) {
    if words.len() < 2 || !words.len().is_power_of_two() {
        return;
    }
    let mut half_width = 1;
    while half_width < words.len() {
        slice_cnot_stage(words, half_width);
        half_width *= 2;
    }
}

fn apply_slice_butterfly_inverse(words: &mut [u64]) {
    if words.len() < 2 || !words.len().is_power_of_two() {
        return;
    }
    let mut half_width = words.len() / 2;
    loop {
        slice_cnot_stage(words, half_width);
        if half_width == 1 {
            break;
        }
        half_width /= 2;
    }
}

fn slice_cnot_stage(words: &mut [u64], half_width: usize) {
    let block_width = half_width * 2;
    for block_start in (0..words.len()).step_by(block_width) {
        for offset in 0..half_width {
            let control = block_start + offset;
            let target = control + half_width;
            words[target] ^= words[control];
        }
    }
}

fn branch_geometry(
    layout: &ConstantLayout,
    word_index: usize,
    round: usize,
    live_bits: usize,
) -> BranchGeometry {
    let pivot = (usize::from(layout.pivot_seed)
        + word_index * usize::from(layout.branch_stride)
        + round * usize::from(layout.branch_span))
        % live_bits.max(1);
    let mut mask = layout
        .phase_mask
        .rotate_left(((word_index + round) % 64) as u32)
        ^ repeat_byte(layout.branch_stride ^ layout.branch_span).rotate_right(
            ((word_index * (round + 1) + usize::from(layout.rotate_right)) % 64) as u32,
        );
    if let Some(affine_mask) = layout.affine_mask {
        mask ^= affine_mask.rotate_left(((round + usize::from(layout.lane_rotate)) % 64) as u32);
    }
    mask &= word_mask(live_bits);
    mask |= 1_u64 << pivot;
    BranchGeometry {
        pivot,
        full_mask: mask,
    }
}

fn gf2_right_mix_forward(value: u64, shift: u8) -> u64 {
    value ^ (value >> shift)
}

fn gf2_right_mix_inverse(mut value: u64, shift: u8) -> u64 {
    let mut delta = usize::from(shift);
    while delta < 64 {
        value ^= value >> delta;
        delta *= 2;
    }
    value
}

fn mod_inverse_odd_u64(value: u64) -> u64 {
    let mut inverse = 1_u64;
    for _ in 0..6 {
        inverse = inverse.wrapping_mul(2_u64.wrapping_sub(value.wrapping_mul(inverse)));
    }
    inverse
}

fn per_word_mask(mask: u64, word_index: usize, rotate_hint: u8) -> u64 {
    mask.rotate_left(((word_index * (usize::from(rotate_hint) + 1)) % 64) as u32)
}

fn encode_terminal_seed(words: &[u64], live_bits: usize) -> Result<Vec<u8>, String> {
    let raw_payload = pack_words_with_width(words, live_bits);
    let raw = std::iter::once(SEED_MODE_RAW)
        .chain(raw_payload.iter().copied())
        .collect::<Vec<_>>();

    let bytes_per_word = live_bits.div_ceil(8);
    let terminals = words
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if terminals.is_empty() || terminals.len() > usize::from(u8::MAX) + 1 {
        return Ok(raw);
    }
    let index_by_terminal = terminals
        .iter()
        .copied()
        .enumerate()
        .map(|(index, terminal)| (terminal, index as u8))
        .collect::<BTreeMap<_, _>>();
    let indices = words
        .iter()
        .map(|word| {
            index_by_terminal
                .get(word)
                .copied()
                .ok_or_else(|| "terminal palette lookup failed".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let bit_width = bit_width_for_cardinality(terminals.len());
    let packed_indices = pack_indices(&indices, bit_width);
    let mut palette =
        Vec::with_capacity(3 + terminals.len() * bytes_per_word + packed_indices.len());
    palette.push(SEED_MODE_PALETTE);
    palette.push(terminals.len() as u8);
    palette.push(bit_width);
    for terminal in &terminals {
        palette.extend_from_slice(&terminal.to_le_bytes()[..bytes_per_word]);
    }
    palette.extend_from_slice(&packed_indices);
    Ok(if palette.len() < raw.len() {
        palette
    } else {
        raw
    })
}

fn decode_terminal_seed(
    seed: &[u8],
    word_count: usize,
    live_bits: usize,
) -> Result<Vec<u64>, String> {
    let Some((&mode, payload)) = seed.split_first() else {
        return Err("terminal seed stream is empty".to_string());
    };
    match mode {
        SEED_MODE_RAW => unpack_words_with_width(payload, word_count, live_bits),
        SEED_MODE_PALETTE => decode_palette_seed(payload, word_count, live_bits),
        _ => Err("unsupported terminal seed encoding mode".to_string()),
    }
}

fn decode_palette_seed(
    seed: &[u8],
    word_count: usize,
    live_bits: usize,
) -> Result<Vec<u64>, String> {
    if seed.len() < 2 {
        return Err("palette seed is missing its header".to_string());
    }
    let terminal_count = usize::from(seed[0]);
    let bit_width = seed[1];
    if terminal_count == 0 {
        return Err("palette seed must contain at least one terminal".to_string());
    }
    let bytes_per_word = live_bits.div_ceil(8);
    let table_bytes = terminal_count
        .checked_mul(bytes_per_word)
        .ok_or_else(|| "palette seed table length overflow".to_string())?;
    if seed.len() < 2 + table_bytes {
        return Err("palette seed terminal table is truncated".to_string());
    }
    let table = &seed[2..2 + table_bytes];
    let terminals = table
        .chunks_exact(bytes_per_word)
        .map(|chunk| {
            let mut encoded = [0_u8; 8];
            encoded[..bytes_per_word].copy_from_slice(chunk);
            Ok(u64::from_le_bytes(encoded) & word_mask(live_bits))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let indices = unpack_indices(&seed[2 + table_bytes..], bit_width, word_count)?;
    indices
        .into_iter()
        .map(|index| {
            terminals
                .get(index as usize)
                .copied()
                .ok_or_else(|| "palette seed index is out of range".to_string())
        })
        .collect()
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
        contract_block, default_feistel_from_seed, expand_block, materialize_runtime,
        strongest_binary_word_peaks, BinaryOperatorKey, IndexMix, PhaseKey, SpectralOperatorKey,
        OPERATOR_BLOCK_WORDS,
    };
    use crate::domain::kernel::key::{
        ConstantFamily, ConstantLayout, PackedConstantK, RoutingKind,
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

    fn sample_layout() -> ConstantLayout {
        ConstantLayout {
            family: ConstantFamily::Hybrid,
            routing: RoutingKind::Butterfly,
            branch_rounds: 6,
            rotate_left: 13,
            rotate_right: 7,
            gf2_shift: 9,
            lane_rotate: 5,
            pivot_seed: 11,
            branch_stride: 3,
            branch_span: 17,
            phase_mask: 0x6996_0579_31EE_C0DE,
            odd_multiplier: 0x9E37_79B9_7F4A_7C15,
            affine_mask: Some(0xA5A5_5A5A_C3C3_3C3C),
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

    #[test]
    fn packed_constant_runtime_roundtrips_seed_and_branch_log() {
        let input = [0x3C_u8; 512];
        let packed = PackedConstantK::from_layout(&sample_layout()).unwrap();
        let runtime = materialize_runtime(&packed, input.len() * 8).unwrap();
        let (seed, branches) = contract_block(&runtime, &input).unwrap();
        let restored = expand_block(&runtime, &seed, &branches).unwrap();
        assert_eq!(restored, input);
        assert!(!seed.is_empty());
    }

    #[test]
    fn branch_log_is_parity_driven_not_removed_bit_echo() {
        let input = [0xA5_u8; 512];
        let packed = PackedConstantK::from_layout(&sample_layout()).unwrap();
        let runtime = materialize_runtime(&packed, input.len() * 8).unwrap();
        let (seed, branches) = contract_block(&runtime, &input).unwrap();
        let mut changed = branches.clone();
        changed[0] ^= 1;
        assert_ne!(expand_block(&runtime, &seed, &changed).unwrap(), input);
    }
}
