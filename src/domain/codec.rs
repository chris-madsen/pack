use std::collections::{BTreeMap, BTreeSet};

use crate::domain::bitstream::{
    bit_width_for_cardinality, ceil_div_u64, pack_indices, unpack_indices,
};
use crate::domain::kernel::base::{
    generate_base_from_root, standard_base, GeneratedBase, RootSeed,
};
use crate::domain::kernel::budget::{BitBudget, CompressionPolicy};
use crate::domain::kernel::key::MagicKey;
use crate::domain::kernel::operator::{
    apply_binary_u_k, BinaryOperatorKey, IndexMix, PhaseKey, SpectralOperatorKey,
};
use crate::domain::kernel::reversible::{
    feistel_forward, feistel_inverse, FeistelKey, FeistelRoundKey,
};
use crate::domain::kernel::spectral::synthesise_spectral_bits;
use crate::domain::kernel::topology::{
    analyze_topology, block_fingerprint, compile_spectral_key, compile_topology_to_key,
};
use crate::domain::kernel::trajectory::{
    decode_parity_trajectory, encode_parity_trajectory, ParityTrajectory,
};
use crate::domain::model::{
    AlphabetBlock, Archive, ArchiveHeader, BlockAnalysis, BlockEncoding, LayerSummary,
    OperatorBlock, RawBlock, SpectralBlock, TrajectoryBlock,
};

const MAGIC_SINGLE: &[u8; 8] = b"PACKMVP1";
const MAGIC_MULTI: &[u8; 8] = b"PACKREC1";
const BASE_VERSION: u8 = 1;
const MAX_LAYERS: usize = 16;
const MAX_DECODER_STEPS: usize = 1_000_000;
const RECURSIVE_HEADER_BYTES: usize = 29;
const LAYER_SUMMARY_BYTES: usize = 20;
pub const DEFAULT_BLOCK_SIZE_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompressionOutcome {
    pub archive: Vec<u8>,
    pub layer_summaries: Vec<LayerSummary>,
}

pub fn compress_bytes(input: &[u8], block_size_hint: Option<usize>) -> Result<Vec<u8>, String> {
    compress_bytes_with_outcome(input, block_size_hint).map(|outcome| outcome.archive)
}

pub fn compress_bytes_with_outcome(
    input: &[u8],
    block_size_hint: Option<usize>,
) -> Result<CompressionOutcome, String> {
    let mut current = input.to_vec();
    let mut layers = Vec::new();
    let mut best_archive = serialize_recursive_archive(input.len() as u64, &layers, &current);

    for _ in 0..MAX_LAYERS {
        let candidates = candidate_block_sizes(&current, block_size_hint);
        let best = choose_best_single_layer(&current, &candidates)?;
        if best.output.len() >= current.len() {
            break;
        }

        let next_summary = LayerSummary {
            block_size_bytes: best.block_size as u32,
            input_size: current.len() as u64,
            output_size: best.output.len() as u64,
        };
        let mut tentative_layers = layers.clone();
        tentative_layers.push(next_summary);
        let tentative_archive =
            serialize_recursive_archive(input.len() as u64, &tentative_layers, &best.output);

        if tentative_archive.len() >= best_archive.len() {
            break;
        }

        layers = tentative_layers;
        current = best.output;
        best_archive = tentative_archive;
    }

    Ok(CompressionOutcome {
        archive: best_archive,
        layer_summaries: layers,
    })
}

pub fn decompress_bytes(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.starts_with(MAGIC_SINGLE) {
        return decompress_single_layer_bytes(input);
    }

    let recursive = parse_recursive_archive(input)?;
    let mut current = recursive.final_payload;
    for layer in recursive.layer_summaries.iter().rev() {
        if current.len() as u64 != layer.output_size {
            return Err("layer output size does not match recursive metadata".to_string());
        }
        let decoded = decompress_single_layer_bytes(&current)?;
        if decoded.len() as u64 != layer.input_size {
            return Err("layer input size does not match recursive metadata".to_string());
        }
        current = decoded;
    }

    if current.len() as u64 != recursive.original_size {
        return Err("decoded size does not match recursive archive header".to_string());
    }
    Ok(current)
}

pub fn inspect_archive(input: &[u8]) -> Result<Vec<LayerSummary>, String> {
    if input.starts_with(MAGIC_SINGLE) {
        return Ok(vec![LayerSummary {
            block_size_bytes: parse_archive(input)?.header.block_size_bytes,
            input_size: 0,
            output_size: input.len() as u64,
        }]);
    }
    parse_recursive_archive(input).map(|archive| archive.layer_summaries)
}

pub fn analyze_block(block: &[u8]) -> BlockAnalysis {
    let unique_sorted_bytes = BTreeSet::from_iter(block.iter().copied())
        .into_iter()
        .collect::<Vec<_>>();
    BlockAnalysis {
        unique_sorted_bytes,
    }
}

fn encode_block(block: &[u8]) -> Result<BlockEncoding, String> {
    let analysis = analyze_block(block);
    let raw = BlockEncoding::Raw(RawBlock {
        original_len: block.len() as u32,
        payload: block.to_vec(),
    });

    let maybe_operator = try_encode_operator_block(block)?;
    let maybe_spectral = try_encode_spectral_block(block)?;
    let maybe_trajectory = try_encode_trajectory_block(block)?;
    let maybe_alphabet = try_encode_alphabet_block(block, &analysis)?;
    let mut best = raw;

    if let Some(candidate) = maybe_operator {
        if encoded_size_of(&candidate) < encoded_size_of(&best) {
            best = candidate;
        }
    }
    if let Some(candidate) = maybe_spectral {
        if encoded_size_of(&candidate) < encoded_size_of(&best) {
            best = candidate;
        }
    }
    if let Some(candidate) = maybe_trajectory {
        if encoded_size_of(&candidate) < encoded_size_of(&best) {
            best = candidate;
        }
    }
    if let Some(candidate) = maybe_alphabet {
        if encoded_size_of(&candidate) < encoded_size_of(&best) {
            best = candidate;
        }
    }
    Ok(best)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SingleLayerCandidate {
    block_size: usize,
    output: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecursiveArchive {
    original_size: u64,
    layer_summaries: Vec<LayerSummary>,
    final_payload: Vec<u8>,
}

fn choose_best_single_layer(
    input: &[u8],
    candidates: &[usize],
) -> Result<SingleLayerCandidate, String> {
    let mut best: Option<SingleLayerCandidate> = None;

    for &candidate in candidates {
        let output = compress_single_layer_bytes(input, candidate)?;
        let item = SingleLayerCandidate {
            block_size: candidate,
            output,
        };

        let should_replace = best
            .as_ref()
            .map(|current_best| item.output.len() < current_best.output.len())
            .unwrap_or(true);

        if should_replace {
            best = Some(item);
        }
    }

    best.ok_or_else(|| "no candidate block sizes were produced".to_string())
}

fn candidate_block_sizes(input: &[u8], block_size_hint: Option<usize>) -> Vec<usize> {
    let input_len = input.len();
    let baseline = match block_size_hint {
        Some(value) if value > 0 => vec![
            value / 4,
            value / 2,
            value,
            value.saturating_mul(2),
            value.saturating_mul(4),
        ],
        _ => {
            let (window_min, window_max) = adaptive_window_bounds(input);
            let mut values = powers_of_two_between(window_min, window_max);
            values.extend([64, 128, 256, 512, 1024, 2048, 4096, 8192]);
            values
        }
    };

    let upper = input_len.max(64);
    let mut unique = BTreeSet::new();
    for candidate in baseline {
        if candidate > 0 {
            unique.insert(candidate.min(upper));
        }
    }
    unique.insert(upper);
    unique.into_iter().collect()
}

fn adaptive_window_bounds(input: &[u8]) -> (usize, usize) {
    let upper = input.len().next_power_of_two().min(8192).max(64);
    let score = spectral_prominence_score(input).unwrap_or(1.0);
    if score >= 12.0 {
        (512.min(upper), upper.max(512))
    } else if score >= 6.0 {
        (256.min(upper), upper.min(4096).max(256))
    } else {
        (64.min(upper), upper.min(1024).max(64))
    }
}

fn powers_of_two_between(min_value: usize, max_value: usize) -> Vec<usize> {
    let mut values = Vec::new();
    let mut current = min_value.max(1).next_power_of_two();
    let ceiling = max_value.max(current);
    while current <= ceiling {
        values.push(current);
        match current.checked_mul(2) {
            Some(next) => current = next,
            None => break,
        }
    }
    values
}

fn spectral_prominence_score(input: &[u8]) -> Option<f64> {
    let probe_len = largest_power_of_two_probe(input.len())?;
    let probe = input.get(..probe_len)?;
    let signature = analyze_topology(probe).ok()?;
    let strongest = signature.walsh_peaks.first()?.coefficient.abs();
    let mean = signature
        .walsh_peaks
        .iter()
        .map(|peak| peak.coefficient.abs())
        .sum::<f64>()
        / signature.walsh_peaks.len() as f64;
    if mean == 0.0 {
        return Some(f64::INFINITY);
    }
    Some(strongest / mean)
}

fn largest_power_of_two_probe(input_len: usize) -> Option<usize> {
    let capped = input_len.min(8192);
    let bytes = capped.checked_next_power_of_two().unwrap_or(capped);
    let probe = if bytes > capped { bytes / 2 } else { bytes };
    if probe < 8 {
        None
    } else {
        Some(probe)
    }
}

fn compress_single_layer_bytes(input: &[u8], block_size_bytes: usize) -> Result<Vec<u8>, String> {
    let safe_block_size = if block_size_bytes == 0 {
        DEFAULT_BLOCK_SIZE_BYTES
    } else {
        block_size_bytes
    };

    let blocks = input
        .chunks(safe_block_size)
        .map(encode_block)
        .collect::<Result<Vec<_>, _>>()?;

    let archive = Archive {
        header: ArchiveHeader {
            base_version: BASE_VERSION,
            block_size_bytes: safe_block_size as u32,
            original_size: input.len() as u64,
            block_count: blocks.len() as u32,
        },
        blocks,
    };

    serialize_single_layer_archive(&archive)
}

fn decompress_single_layer_bytes(input: &[u8]) -> Result<Vec<u8>, String> {
    let archive = parse_archive(input)?;
    let mut out = Vec::with_capacity(archive.header.original_size as usize);

    for block in archive.blocks {
        match block {
            BlockEncoding::Raw(raw) => out.extend_from_slice(&raw.payload),
            BlockEncoding::Alphabet(alpha) => {
                let restored = decode_alphabet_block(&alpha)?;
                out.extend_from_slice(&restored);
            }
            BlockEncoding::Spectral(spectral) => {
                let restored = decode_spectral_block(&spectral)?;
                out.extend_from_slice(&restored);
            }
            BlockEncoding::Trajectory(trajectory) => {
                let restored = decode_trajectory_block(&trajectory)?;
                out.extend_from_slice(&restored);
            }
            BlockEncoding::Operator(operator) => {
                let restored = decode_operator_block(&operator)?;
                out.extend_from_slice(&restored);
            }
        }
    }

    if out.len() as u64 != archive.header.original_size {
        return Err("decoded size does not match archive header".to_string());
    }

    Ok(out)
}

fn try_encode_alphabet_block(
    block: &[u8],
    analysis: &BlockAnalysis,
) -> Result<Option<BlockEncoding>, String> {
    let alphabet = &analysis.unique_sorted_bytes;
    if alphabet.is_empty() {
        return Ok(None);
    }

    let bit_width = bit_width_for_cardinality(alphabet.len());
    if bit_width >= 8 {
        return Ok(None);
    }

    let mut index_by_byte = BTreeMap::new();
    for (idx, byte) in alphabet.iter().enumerate() {
        index_by_byte.insert(*byte, idx as u8);
    }

    let indices = block
        .iter()
        .map(|byte| {
            index_by_byte
                .get(byte)
                .copied()
                .ok_or_else(|| "alphabet index lookup failed".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;

    let breadcrumbs = pack_indices(&indices, bit_width);
    Ok(Some(BlockEncoding::Alphabet(AlphabetBlock {
        original_len: block.len() as u32,
        alphabet: alphabet.clone(),
        bit_width,
        breadcrumbs,
    })))
}

fn try_encode_trajectory_block(block: &[u8]) -> Result<Option<BlockEncoding>, String> {
    if block.len() < 16 || block.len() % 8 != 0 {
        return Ok(None);
    }
    let words = block
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();

    let mut best: Option<BlockEncoding> = None;
    for steps in [2_u8, 4, 8, 12, 16, 24, 32, 40, 48, 56] {
        let Some(first_terminal) = words.first().map(|value| value >> steps) else {
            continue;
        };
        if !words
            .iter()
            .all(|value| (*value >> steps) == first_terminal)
        {
            continue;
        }

        let mut crumbs = Vec::with_capacity(words.len() * steps as usize);
        for value in &words {
            let trajectory = encode_parity_trajectory(*value, steps as usize)?;
            if trajectory.terminal != first_terminal {
                crumbs.clear();
                break;
            }
            crumbs.extend(trajectory.crumbs);
        }
        if crumbs.is_empty() && !words.is_empty() {
            continue;
        }

        let key = build_trajectory_key(block, steps)?.serialize().to_vec();
        let packed_crumbs = pack_bools(&crumbs);
        let candidate = BlockEncoding::Trajectory(TrajectoryBlock {
            original_len: block.len() as u32,
            key: key.clone(),
            terminal: first_terminal,
            steps,
            breadcrumbs: packed_crumbs,
        });
        let overhead_bytes = encoded_size_of(&candidate)
            .checked_sub(key.len())
            .and_then(|value| {
                value.checked_sub(
                    extract_trajectory_block(&candidate)
                        .unwrap()
                        .breadcrumbs
                        .len(),
                )
            })
            .ok_or_else(|| "trajectory block accounting underflow".to_string())?;
        let budget = BitBudget {
            source_bits: (block.len() * 8) as u64,
            key_bits: (key.len() * 8) as u64,
            crumb_bits: (extract_trajectory_block(&candidate)
                .unwrap()
                .breadcrumbs
                .len()
                * 8) as u64,
            overhead_bits: (overhead_bytes * 8) as u64,
        };
        if CompressionPolicy::MVP.accepts(budget)?
            && best
                .as_ref()
                .map(|current| encoded_size_of(&candidate) < encoded_size_of(current))
                .unwrap_or(true)
        {
            best = Some(candidate);
        }
    }

    Ok(best)
}

fn try_encode_operator_block(block: &[u8]) -> Result<Option<BlockEncoding>, String> {
    if block.len() < 64 || block.len() % 8 != 0 || !block.len().is_power_of_two() {
        return Ok(None);
    }
    let base_key = match build_operator_codec_key(block) {
        Ok(key) => key,
        Err(_) => return Ok(None),
    };
    let words = block
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let base_key_bytes = base_key.serialize();
    let (operator, generated, feistel, _) = parse_operator_runtime_key(&base_key_bytes)?;
    let transformed = words
        .iter()
        .map(|word| apply_operator_word_u_k_with_runtime(*word, &operator, &generated, &feistel))
        .collect::<Result<Vec<_>, _>>()?;
    let original_steps = minimal_common_terminal_steps(&words);
    let transformed_steps = minimal_common_terminal_steps(&transformed);
    if transformed_steps.is_none()
        || original_steps.is_some_and(|steps| transformed_steps.unwrap() >= steps)
    {
        return Ok(None);
    }

    let mut best: Option<BlockEncoding> = None;
    for steps in [2_u8, 4, 8, 12, 16, 24, 32, 40, 48, 56] {
        let terminal = transformed[0] >> steps;
        if !transformed
            .iter()
            .all(|value| (*value >> steps) == terminal)
        {
            continue;
        }
        let mut crumbs = Vec::with_capacity(transformed.len() * steps as usize);
        for value in &transformed {
            let trajectory = encode_parity_trajectory(*value, steps as usize)?;
            if trajectory.terminal != terminal {
                return Err("operator trajectory terminal changed unexpectedly".to_string());
            }
            crumbs.extend(trajectory.crumbs);
        }

        let key_bytes = base_key.serialize().to_vec();
        let candidate = BlockEncoding::Operator(OperatorBlock {
            original_len: block.len() as u32,
            key: key_bytes.clone(),
            terminal,
            steps,
            breadcrumbs: pack_bools(&crumbs),
        });
        let overhead_bytes = encoded_size_of(&candidate)
            .checked_sub(key_bytes.len())
            .and_then(|value| {
                value.checked_sub(
                    extract_operator_block(&candidate)
                        .unwrap()
                        .breadcrumbs
                        .len(),
                )
            })
            .ok_or_else(|| "operator block accounting underflow".to_string())?;
        let budget = BitBudget {
            source_bits: (block.len() * 8) as u64,
            key_bits: (key_bytes.len() * 8) as u64,
            crumb_bits: (extract_operator_block(&candidate)
                .unwrap()
                .breadcrumbs
                .len()
                * 8) as u64,
            overhead_bits: (overhead_bytes * 8) as u64,
        };
        if CompressionPolicy::MVP.accepts(budget)?
            && best
                .as_ref()
                .map(|current| encoded_size_of(&candidate) < encoded_size_of(current))
                .unwrap_or(true)
        {
            best = Some(candidate);
        }
    }
    Ok(best)
}

fn minimal_common_terminal_steps(words: &[u64]) -> Option<u8> {
    [2_u8, 4, 8, 12, 16, 24, 32, 40, 48, 56]
        .into_iter()
        .find(|steps| {
            words.first().is_some_and(|first| {
                let terminal = first >> steps;
                words.iter().all(|word| (*word >> steps) == terminal)
            })
        })
}

fn decode_alphabet_block(block: &AlphabetBlock) -> Result<Vec<u8>, String> {
    let count = block.original_len as usize;
    let indices = unpack_indices(&block.breadcrumbs, block.bit_width, count)?;
    let mut out = Vec::with_capacity(count);
    for index in indices {
        let byte = block
            .alphabet
            .get(index as usize)
            .copied()
            .ok_or_else(|| "decoded alphabet index is out of range".to_string())?;
        out.push(byte);
    }
    Ok(out)
}

fn decode_spectral_block(block: &SpectralBlock) -> Result<Vec<u8>, String> {
    let key = MagicKey::parse(&block.key)?;
    let program = key.spectral_program()?;
    if program.bit_len != block.original_len as usize * 8 {
        return Err("spectral K block size does not match the record".to_string());
    }
    let predictor = synthesise_spectral_bytes(key)?;
    apply_spectral_exceptions(&predictor, &block.residual, program.bit_len)
}

fn decode_trajectory_block(block: &TrajectoryBlock) -> Result<Vec<u8>, String> {
    let key = MagicKey::parse(&block.key)?;
    if key.raw() & 0x3F != block.steps as u64 {
        return Err("trajectory K does not match the encoded step count".to_string());
    }
    let steps = block.steps;
    let word_size = 8;
    let original_len = block.original_len as usize;
    if original_len % word_size != 0 {
        return Err("trajectory block length must be divisible by word size".to_string());
    }
    let word_count = original_len / word_size;
    let total_steps = word_count
        .checked_mul(steps as usize)
        .ok_or_else(|| "trajectory decoder step overflow".to_string())?;
    if total_steps > MAX_DECODER_STEPS {
        return Err("trajectory decoder gas limit exceeded".to_string());
    }
    let crumbs = unpack_bools(&block.breadcrumbs, total_steps)?;
    let mut out = Vec::with_capacity(original_len);
    for chunk in crumbs.chunks_exact(steps as usize) {
        let trajectory = ParityTrajectory {
            terminal: block.terminal,
            crumbs: chunk.to_vec(),
        };
        let value = decode_parity_trajectory(&trajectory)?;
        out.extend_from_slice(&value.to_le_bytes()[..word_size]);
    }
    Ok(out)
}

fn decode_operator_block(block: &OperatorBlock) -> Result<Vec<u8>, String> {
    if block.original_len as usize % 8 != 0 {
        return Err("operator block length must be divisible by 8".to_string());
    }
    MagicKey::parse(&block.key)?;
    let steps = block.steps;
    if !(1..64).contains(&steps) {
        return Err("operator parity step count must be in 1..64".to_string());
    }
    let word_count = block.original_len as usize / 8;
    let total_steps = word_count
        .checked_mul(steps as usize)
        .ok_or_else(|| "operator decoder step overflow".to_string())?;
    if total_steps > MAX_DECODER_STEPS {
        return Err("operator decoder gas limit exceeded".to_string());
    }
    let crumbs = unpack_bools(&block.breadcrumbs, total_steps)?;
    let (operator, generated, feistel, _) = parse_operator_runtime_key(&block.key)?;
    let mut out = Vec::with_capacity(block.original_len as usize);
    for word_crumbs in crumbs.chunks_exact(steps as usize) {
        let transformed = decode_parity_trajectory(&ParityTrajectory {
            terminal: block.terminal,
            crumbs: word_crumbs.to_vec(),
        })?;
        let word =
            apply_operator_word_u_k_with_runtime(transformed, &operator, &generated, &feistel)?;
        out.extend_from_slice(&word.to_le_bytes());
    }
    Ok(out)
}

fn build_trajectory_key(block: &[u8], steps: u8) -> Result<MagicKey, String> {
    let fingerprint = block_fingerprint(block)?;
    Ok(MagicKey::from_raw((fingerprint & !0x3F) | steps as u64))
}

fn build_operator_codec_key(block: &[u8]) -> Result<MagicKey, String> {
    compile_topology_to_key(&analyze_topology(block)?)
}

fn parse_operator_runtime_key(
    key_bytes: &[u8],
) -> Result<(SpectralOperatorKey, GeneratedBase, FeistelKey, u64), String> {
    let key = MagicKey::parse(key_bytes)?;
    let raw = key.raw();
    let seed = raw.rotate_left(17) ^ 0xA076_1D64_78BD_642F;
    let mask = raw.rotate_right(9) ^ 0xE703_7ED1_A0B4_28DB;
    let shift_left = ((raw >> 7) % 31 + 1) as u8;
    let shift_right = ((raw >> 23) % 31 + 1) as u8;
    let odd_multiplier = raw.rotate_left(29) | 1;
    let generated = generate_base_from_root(&standard_base(1)?, RootSeed { value: raw ^ mask })?;
    let operator = SpectralOperatorKey {
        mix: IndexMix {
            xor_mask: raw as usize,
            rotate: ((raw >> 3) % 31 + 1) as u8,
        },
        phase: PhaseKey {
            seed,
            mask,
            shift_left,
            shift_right,
            odd_multiplier,
        },
    };
    let feistel = FeistelKey {
        rounds: vec![
            FeistelRoundKey {
                shift_left,
                shift_right,
                rotate: ((raw >> 41) % 32) as u8,
                odd_multiplier: odd_multiplier as u32 | 1,
                add: seed as u32,
                mask: mask as u32,
            },
            FeistelRoundKey {
                shift_left: shift_right,
                shift_right: shift_left,
                rotate: ((raw >> 53) % 31 + 1) as u8,
                odd_multiplier: (odd_multiplier >> 32) as u32 | 1,
                add: (seed >> 32) as u32,
                mask: (mask >> 32) as u32,
            },
        ],
    };
    feistel.validate()?;
    Ok((operator, generated, feistel, seed))
}

#[cfg(test)]
fn apply_operator_word_u_k(value: u64, key_bytes: &[u8]) -> Result<u64, String> {
    let (operator, generated, feistel, _) = parse_operator_runtime_key(key_bytes)?;
    apply_operator_word_u_k_with_runtime(value, &operator, &generated, &feistel)
}

fn apply_operator_word_u_k_with_runtime(
    value: u64,
    operator: &SpectralOperatorKey,
    generated: &GeneratedBase,
    feistel: &FeistelKey,
) -> Result<u64, String> {
    let forward = feistel_forward(generated.apply_forward(value)?, feistel)?;
    let phase = operator.phase.seed
        ^ operator.phase.mask.rotate_left(17)
        ^ operator.phase.odd_multiplier.rotate_right(11)
        ^ (operator.phase.shift_left as u64)
        ^ ((operator.phase.shift_right as u64) << 8);
    let reflected = apply_binary_u_k(
        forward,
        BinaryOperatorKey {
            xor_mask: operator.mix.xor_mask as u64,
            rotate: operator.mix.rotate,
            phase_mask: phase,
        },
    );
    generated.apply_inverse(feistel_inverse(reflected, feistel)?)
}

fn try_encode_spectral_block(block: &[u8]) -> Result<Option<BlockEncoding>, String> {
    let bit_len = block.len() * 8;
    if bit_len < 64 || !bit_len.is_power_of_two() || bit_len > 8192 {
        return Ok(None);
    }
    let signature = analyze_topology(block)?;
    let key = compile_spectral_key(&signature)?;
    let predictor = synthesise_spectral_bytes(key)?;
    let residual = encode_spectral_exceptions(block, &predictor)?;
    let key_bytes = key.serialize().to_vec();
    let candidate = BlockEncoding::Spectral(SpectralBlock {
        original_len: block.len() as u32,
        key: key_bytes.clone(),
        residual,
    });
    let spectral = extract_spectral_block(&candidate).unwrap();
    let overhead_bytes = encoded_size_of(&candidate)
        .checked_sub(key_bytes.len())
        .and_then(|value| value.checked_sub(spectral.residual.len()))
        .ok_or_else(|| "spectral block accounting underflow".to_string())?;
    let budget = BitBudget {
        source_bits: bit_len as u64,
        key_bits: 64,
        crumb_bits: (spectral.residual.len() * 8) as u64,
        overhead_bits: (overhead_bytes * 8) as u64,
    };
    Ok(CompressionPolicy::MVP.accepts(budget)?.then_some(candidate))
}

fn synthesise_spectral_bytes(key: MagicKey) -> Result<Vec<u8>, String> {
    let bits = synthesise_spectral_bits(&key.spectral_program()?)?;
    let mut bytes = vec![0_u8; bits.len().div_ceil(8)];
    for (index, bit) in bits.into_iter().enumerate() {
        if bit == 1 {
            bytes[index / 8] |= 1 << (index % 8);
        }
    }
    Ok(bytes)
}

fn encode_spectral_exceptions(actual: &[u8], predictor: &[u8]) -> Result<Vec<u8>, String> {
    if actual.len() != predictor.len() {
        return Err("spectral predictor length does not match the block".to_string());
    }
    let mut out = Vec::new();
    let mut previous = None;
    for bit_index in 0..actual.len() * 8 {
        let actual_bit = (actual[bit_index / 8] >> (bit_index % 8)) & 1;
        let predicted_bit = (predictor[bit_index / 8] >> (bit_index % 8)) & 1;
        if actual_bit != predicted_bit {
            let delta = match previous {
                None => bit_index + 1,
                Some(last) => bit_index - last,
            };
            encode_uleb128(delta as u64, &mut out);
            previous = Some(bit_index);
        }
    }
    Ok(out)
}

fn apply_spectral_exceptions(
    predictor: &[u8],
    residual: &[u8],
    bit_len: usize,
) -> Result<Vec<u8>, String> {
    let mut out = predictor.to_vec();
    let mut cursor = 0;
    let mut previous = None;
    while cursor < residual.len() {
        let delta = decode_uleb128(residual, &mut cursor)?;
        if delta == 0 {
            return Err("spectral exception delta must be positive".to_string());
        }
        let index = match previous {
            None => delta as usize - 1,
            Some(last) => last + delta as usize,
        };
        if index >= bit_len {
            return Err("spectral exception index is outside the block".to_string());
        }
        out[index / 8] ^= 1 << (index % 8);
        previous = Some(index);
    }
    Ok(out)
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

fn decode_uleb128(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let start = *cursor;
    let mut value = 0_u64;
    let mut shift = 0_u32;
    loop {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| "truncated spectral exception varint".to_string())?;
        *cursor += 1;
        if shift >= 64 {
            return Err("spectral exception varint overflow".to_string());
        }
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            let mut canonical = Vec::new();
            encode_uleb128(value, &mut canonical);
            if canonical.as_slice() != &bytes[start..*cursor] {
                return Err("spectral exception varint is not canonical".to_string());
            }
            return Ok(value);
        }
        shift += 7;
    }
}

fn extract_trajectory_block(block: &BlockEncoding) -> Option<&TrajectoryBlock> {
    match block {
        BlockEncoding::Trajectory(trajectory) => Some(trajectory),
        _ => None,
    }
}

fn extract_spectral_block(block: &BlockEncoding) -> Option<&SpectralBlock> {
    match block {
        BlockEncoding::Spectral(spectral) => Some(spectral),
        _ => None,
    }
}

fn extract_operator_block(block: &BlockEncoding) -> Option<&OperatorBlock> {
    match block {
        BlockEncoding::Operator(operator) => Some(operator),
        _ => None,
    }
}

fn pack_bools(bits: &[bool]) -> Vec<u8> {
    let values = bits.iter().map(|bit| u8::from(*bit)).collect::<Vec<_>>();
    pack_indices(&values, 1)
}

fn unpack_bools(bytes: &[u8], count: usize) -> Result<Vec<bool>, String> {
    Ok(unpack_indices(bytes, 1, count)?
        .into_iter()
        .map(|value| value != 0)
        .collect())
}

fn encoded_size_of(block: &BlockEncoding) -> usize {
    match block {
        BlockEncoding::Raw(raw) => 1 + 4 + 4 + raw.payload.len(),
        BlockEncoding::Alphabet(alpha) => {
            1 + 4 + 1 + 1 + alpha.alphabet.len() + 4 + alpha.breadcrumbs.len()
        }
        BlockEncoding::Spectral(spectral) => {
            1 + 4 + 2 + spectral.key.len() + 4 + spectral.residual.len()
        }
        BlockEncoding::Trajectory(trajectory) => {
            1 + 4 + 2 + trajectory.key.len() + 8 + 1 + 4 + trajectory.breadcrumbs.len()
        }
        BlockEncoding::Operator(operator) => {
            1 + 4 + 2 + operator.key.len() + 8 + 1 + 4 + operator.breadcrumbs.len()
        }
    }
}

fn serialize_single_layer_archive(archive: &Archive) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC_SINGLE);
    out.push(archive.header.base_version);
    push_u32(&mut out, archive.header.block_size_bytes);
    push_u64(&mut out, archive.header.original_size);
    push_u32(&mut out, archive.header.block_count);

    for block in &archive.blocks {
        match block {
            BlockEncoding::Raw(raw) => {
                out.push(block.mode() as u8);
                push_u32(&mut out, raw.original_len);
                push_u32(&mut out, raw.payload.len() as u32);
                out.extend_from_slice(&raw.payload);
            }
            BlockEncoding::Alphabet(alpha) => {
                out.push(block.mode() as u8);
                push_u32(&mut out, alpha.original_len);
                out.push(alpha.alphabet.len() as u8);
                out.push(alpha.bit_width);
                out.extend_from_slice(&alpha.alphabet);
                push_u32(&mut out, alpha.breadcrumbs.len() as u32);
                out.extend_from_slice(&alpha.breadcrumbs);
            }
            BlockEncoding::Spectral(spectral) => {
                out.push(block.mode() as u8);
                push_u32(&mut out, spectral.original_len);
                push_u16(&mut out, spectral.key.len() as u16);
                out.extend_from_slice(&spectral.key);
                push_u32(&mut out, spectral.residual.len() as u32);
                out.extend_from_slice(&spectral.residual);
            }
            BlockEncoding::Trajectory(trajectory) => {
                out.push(block.mode() as u8);
                push_u32(&mut out, trajectory.original_len);
                push_u16(&mut out, trajectory.key.len() as u16);
                out.extend_from_slice(&trajectory.key);
                push_u64(&mut out, trajectory.terminal);
                out.push(trajectory.steps);
                push_u32(&mut out, trajectory.breadcrumbs.len() as u32);
                out.extend_from_slice(&trajectory.breadcrumbs);
            }
            BlockEncoding::Operator(operator) => {
                out.push(block.mode() as u8);
                push_u32(&mut out, operator.original_len);
                push_u16(&mut out, operator.key.len() as u16);
                out.extend_from_slice(&operator.key);
                push_u64(&mut out, operator.terminal);
                out.push(operator.steps);
                push_u32(&mut out, operator.breadcrumbs.len() as u32);
                out.extend_from_slice(&operator.breadcrumbs);
            }
        }
    }

    Ok(out)
}

fn serialize_recursive_archive(
    original_size: u64,
    layer_summaries: &[LayerSummary],
    final_payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        RECURSIVE_HEADER_BYTES + layer_summaries.len() * LAYER_SUMMARY_BYTES + final_payload.len(),
    );
    out.extend_from_slice(MAGIC_MULTI);
    out.push(BASE_VERSION);
    push_u64(&mut out, original_size);
    push_u32(&mut out, layer_summaries.len() as u32);
    for layer in layer_summaries {
        push_u32(&mut out, layer.block_size_bytes);
        push_u64(&mut out, layer.input_size);
        push_u64(&mut out, layer.output_size);
    }
    push_u64(&mut out, final_payload.len() as u64);
    out.extend_from_slice(final_payload);
    out
}

fn parse_archive(input: &[u8]) -> Result<Archive, String> {
    let mut cursor = 0_usize;
    expect_exact_magic(input, &mut cursor, MAGIC_SINGLE)?;
    let base_version = read_u8(input, &mut cursor)?;
    if base_version != BASE_VERSION {
        return Err(format!("unsupported base version: {base_version}"));
    }
    let block_size_bytes = read_u32(input, &mut cursor)?;
    if block_size_bytes == 0 {
        return Err("block size must be greater than zero".to_string());
    }
    let original_size = read_u64(input, &mut cursor)?;
    let block_count = read_u32(input, &mut cursor)?;
    if block_count as usize > input.len() {
        return Err("block count exceeds archive size".to_string());
    }

    let mut blocks = Vec::with_capacity(block_count as usize);
    for _ in 0..block_count {
        let mode = read_u8(input, &mut cursor)?;
        let original_len = read_u32(input, &mut cursor)?;
        match mode {
            0 => {
                let payload_len = read_u32(input, &mut cursor)? as usize;
                if payload_len != original_len as usize {
                    return Err("raw payload length does not match original length".to_string());
                }
                let payload = read_vec(input, &mut cursor, payload_len)?;
                blocks.push(BlockEncoding::Raw(RawBlock {
                    original_len,
                    payload,
                }));
            }
            2 => {
                let alphabet_len = read_u8(input, &mut cursor)? as usize;
                let bit_width = read_u8(input, &mut cursor)?;
                let alphabet = read_vec(input, &mut cursor, alphabet_len)?;
                validate_alphabet(&alphabet, bit_width)?;
                let breadcrumbs_len = read_u32(input, &mut cursor)? as usize;
                let breadcrumbs = read_vec(input, &mut cursor, breadcrumbs_len)?;
                let expected_bits = original_len as u64 * bit_width as u64;
                let expected_bytes = ceil_div_u64(expected_bits, 8) as usize;
                if breadcrumbs.len() != expected_bytes {
                    return Err("alphabet breadcrumb payload length is not canonical".to_string());
                }
                blocks.push(BlockEncoding::Alphabet(AlphabetBlock {
                    original_len,
                    alphabet,
                    bit_width,
                    breadcrumbs,
                }));
            }
            1 => {
                let key_len = read_u16(input, &mut cursor)? as usize;
                let key = read_vec(input, &mut cursor, key_len)?;
                let parsed = MagicKey::parse(&key)?;
                let program = parsed.spectral_program()?;
                if program.bit_len != original_len as usize * 8 {
                    return Err("spectral K block size does not match the record".to_string());
                }
                let residual_len = read_u32(input, &mut cursor)? as usize;
                let residual = read_vec(input, &mut cursor, residual_len)?;
                let predictor = synthesise_spectral_bytes(parsed)?;
                apply_spectral_exceptions(&predictor, &residual, program.bit_len)?;
                blocks.push(BlockEncoding::Spectral(SpectralBlock {
                    original_len,
                    key,
                    residual,
                }));
            }
            3 => {
                let key_len = read_u16(input, &mut cursor)? as usize;
                let key = read_vec(input, &mut cursor, key_len)?;
                let parsed = MagicKey::parse(&key)?;
                let terminal = read_u64(input, &mut cursor)?;
                let steps = read_u8(input, &mut cursor)?;
                if !(1..64).contains(&steps) || parsed.raw() & 0x3F != steps as u64 {
                    return Err("trajectory K does not match a valid step count".to_string());
                }
                let breadcrumbs_len = read_u32(input, &mut cursor)? as usize;
                let breadcrumbs = read_vec(input, &mut cursor, breadcrumbs_len)?;
                let expected_bits = (original_len as usize / 8)
                    .checked_mul(steps as usize)
                    .ok_or_else(|| "trajectory breadcrumb length overflow".to_string())?;
                if original_len as usize % 8 != 0
                    || breadcrumbs.len() != ceil_div_u64(expected_bits as u64, 8) as usize
                {
                    return Err(
                        "trajectory parity breadcrumb payload length is not canonical".to_string(),
                    );
                }
                blocks.push(BlockEncoding::Trajectory(TrajectoryBlock {
                    original_len,
                    key,
                    terminal,
                    steps,
                    breadcrumbs,
                }));
            }
            4 => {
                let key_len = read_u16(input, &mut cursor)? as usize;
                let key = read_vec(input, &mut cursor, key_len)?;
                MagicKey::parse(&key)?;
                let terminal = read_u64(input, &mut cursor)?;
                let steps = read_u8(input, &mut cursor)?;
                if !(1..64).contains(&steps) {
                    return Err("operator parity step count must be in 1..64".to_string());
                }
                if original_len as usize % 8 != 0 {
                    return Err("operator block length must be divisible by 8".to_string());
                }
                let breadcrumbs_len = read_u32(input, &mut cursor)? as usize;
                let breadcrumbs = read_vec(input, &mut cursor, breadcrumbs_len)?;
                let expected_bits = (original_len as usize / 8)
                    .checked_mul(steps as usize)
                    .ok_or_else(|| "operator breadcrumb length overflow".to_string())?;
                let expected_bytes = ceil_div_u64(expected_bits as u64, 8) as usize;
                if breadcrumbs.len() != expected_bytes {
                    return Err(
                        "operator parity breadcrumb payload length is not canonical".to_string()
                    );
                }
                blocks.push(BlockEncoding::Operator(OperatorBlock {
                    original_len,
                    key,
                    terminal,
                    steps,
                    breadcrumbs,
                }));
            }
            other => return Err(format!("unknown block mode: {other}")),
        }
    }
    ensure_fully_consumed(input, cursor)?;

    Ok(Archive {
        header: ArchiveHeader {
            base_version,
            block_size_bytes,
            original_size,
            block_count,
        },
        blocks,
    })
}

fn parse_recursive_archive(input: &[u8]) -> Result<RecursiveArchive, String> {
    let mut cursor = 0_usize;
    expect_exact_magic(input, &mut cursor, MAGIC_MULTI)?;
    let base_version = read_u8(input, &mut cursor)?;
    if base_version != BASE_VERSION {
        return Err(format!("unsupported base version: {base_version}"));
    }
    let original_size = read_u64(input, &mut cursor)?;
    let layer_count = read_u32(input, &mut cursor)? as usize;
    if layer_count > MAX_LAYERS {
        return Err("layer count exceeds configured maximum".to_string());
    }
    let mut layer_summaries = Vec::with_capacity(layer_count);
    for _ in 0..layer_count {
        layer_summaries.push(LayerSummary {
            block_size_bytes: read_u32(input, &mut cursor)?,
            input_size: read_u64(input, &mut cursor)?,
            output_size: read_u64(input, &mut cursor)?,
        });
    }
    let final_payload_len = read_u64(input, &mut cursor)? as usize;
    let final_payload = read_vec(input, &mut cursor, final_payload_len)?;
    ensure_fully_consumed(input, cursor)?;
    validate_layer_chain(original_size, &layer_summaries, final_payload.len() as u64)?;
    Ok(RecursiveArchive {
        original_size,
        layer_summaries,
        final_payload,
    })
}

fn expect_exact_magic(input: &[u8], cursor: &mut usize, magic: &[u8; 8]) -> Result<(), String> {
    let end = cursor
        .checked_add(magic.len())
        .ok_or_else(|| "archive cursor overflow".to_string())?;
    let actual = input
        .get(*cursor..end)
        .ok_or_else(|| "archive is too short for magic header".to_string())?;
    if actual != magic {
        return Err("invalid archive magic".to_string());
    }
    *cursor = end;
    Ok(())
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u8(input: &[u8], cursor: &mut usize) -> Result<u8, String> {
    let value = *input
        .get(*cursor)
        .ok_or_else(|| "unexpected end of archive".to_string())?;
    *cursor += 1;
    Ok(value)
}

fn read_u32(input: &[u8], cursor: &mut usize) -> Result<u32, String> {
    let bytes = read_array::<4>(input, cursor)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u16(input: &[u8], cursor: &mut usize) -> Result<u16, String> {
    let bytes = read_array::<2>(input, cursor)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u64(input: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let bytes = read_array::<8>(input, cursor)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_array<const N: usize>(input: &[u8], cursor: &mut usize) -> Result<[u8; N], String> {
    let end = cursor
        .checked_add(N)
        .ok_or_else(|| "archive cursor overflow".to_string())?;
    let slice = input
        .get(*cursor..end)
        .ok_or_else(|| "unexpected end of archive".to_string())?;
    let mut array = [0_u8; N];
    array.copy_from_slice(slice);
    *cursor = end;
    Ok(array)
}

fn read_vec(input: &[u8], cursor: &mut usize, len: usize) -> Result<Vec<u8>, String> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| "archive cursor overflow".to_string())?;
    let slice = input
        .get(*cursor..end)
        .ok_or_else(|| "unexpected end of archive".to_string())?;
    *cursor = end;
    Ok(slice.to_vec())
}

fn validate_alphabet(alphabet: &[u8], bit_width: u8) -> Result<(), String> {
    if alphabet.is_empty() {
        return Err("alphabet must not be empty".to_string());
    }
    if !alphabet.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err("alphabet must be sorted and contain unique bytes".to_string());
    }
    let expected_width = bit_width_for_cardinality(alphabet.len());
    if bit_width != expected_width {
        return Err("alphabet bit width is not canonical".to_string());
    }
    Ok(())
}

fn validate_layer_chain(
    original_size: u64,
    layers: &[LayerSummary],
    final_payload_size: u64,
) -> Result<(), String> {
    if layers.is_empty() {
        if final_payload_size != original_size {
            return Err("raw recursive payload size does not match original size".to_string());
        }
        return Ok(());
    }

    if layers[0].input_size != original_size {
        return Err("first layer input size does not match original size".to_string());
    }
    if layers
        .windows(2)
        .any(|pair| pair[0].output_size != pair[1].input_size)
    {
        return Err("recursive layer size chain is inconsistent".to_string());
    }
    if layers
        .last()
        .is_some_and(|layer| layer.output_size != final_payload_size)
    {
        return Err("final payload size does not match last layer".to_string());
    }
    if layers
        .iter()
        .any(|layer| layer.block_size_bytes == 0 || layer.output_size >= layer.input_size)
    {
        return Err("recursive layer metadata violates compression invariants".to_string());
    }
    Ok(())
}

fn ensure_fully_consumed(input: &[u8], cursor: usize) -> Result<(), String> {
    if cursor != input.len() {
        return Err("archive contains trailing bytes".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod compact_codec_tests {
    use super::{
        apply_operator_word_u_k, build_operator_codec_key, compress_bytes,
        compress_bytes_with_outcome, compress_single_layer_bytes, decompress_bytes, encode_block,
        encoded_size_of, minimal_common_terminal_steps, parse_archive,
        serialize_single_layer_archive, synthesise_spectral_bytes, try_encode_operator_block,
        Archive, ArchiveHeader, BlockEncoding, MagicKey, OperatorBlock, RawBlock, SpectralBlock,
        DEFAULT_BLOCK_SIZE_BYTES, MAGIC_MULTI,
    };
    use crate::domain::kernel::topology::{analyze_topology, compile_spectral_key};

    fn walsh_basis_bytes(bit_len: usize, basis_index: usize) -> Vec<u8> {
        let mut out = vec![0_u8; bit_len / 8];
        for position in 0..bit_len {
            if (position & basis_index).count_ones() & 1 == 0 {
                out[position / 8] |= 1 << (position % 8);
            }
        }
        out
    }

    #[test]
    fn roundtrip_ascii_and_binary_data() {
        for input in [
            b"01234567890123456789".repeat(128),
            (0_u32..2048)
                .map(|value| ((value * 37 + 11) % 256) as u8)
                .collect::<Vec<_>>(),
        ] {
            let packed = compress_bytes(&input, Some(512)).unwrap();
            assert_eq!(decompress_bytes(&packed).unwrap(), input);
        }
    }

    #[test]
    fn recursive_layers_are_used_only_when_the_complete_archive_shrinks() {
        let input = b"0123456789".repeat(5000);
        let outcome = compress_bytes_with_outcome(&input, None).unwrap();
        assert!(!outcome.layer_summaries.is_empty());
        assert_eq!(decompress_bytes(&outcome.archive).unwrap(), input);
        assert!(outcome.archive.len() < input.len() + 29);
    }

    #[test]
    fn mvp_block_size_is_4096_bits() {
        assert_eq!(DEFAULT_BLOCK_SIZE_BYTES, 512);
    }

    #[test]
    fn real_encode_path_emits_spectral_for_a_known_walsh_signal() {
        let input = walsh_basis_bytes(4096, 0b1010_0110_1101);
        let encoded = encode_block(&input).unwrap();
        let spectral = match encoded {
            BlockEncoding::Spectral(block) => block,
            other => panic!("expected spectral block, got {other:?}"),
        };
        assert_eq!(spectral.key.len(), 8);
        assert!(spectral.residual.is_empty());
        assert!(encoded_size_of(&BlockEncoding::Spectral(spectral.clone())) < input.len());
        assert_eq!(super::decode_spectral_block(&spectral).unwrap(), input);
    }

    #[test]
    fn public_archive_path_serializes_and_decodes_a_real_spectral_block() {
        let input = walsh_basis_bytes(4096, 0b1010_0110_1101);
        let packed = compress_single_layer_bytes(&input, input.len()).unwrap();
        let archive = parse_archive(&packed).unwrap();
        assert!(matches!(
            archive.blocks.as_slice(),
            [BlockEncoding::Spectral(SpectralBlock {
                key,
                residual,
                ..
            })] if key.len() == 8 && residual.is_empty()
        ));
        assert_eq!(
            super::decompress_single_layer_bytes(&packed).unwrap(),
            input
        );
    }

    #[test]
    fn spectral_magic_constant_expands_the_recorded_walsh_coordinates() {
        let left = walsh_basis_bytes(512, 17);
        let right = walsh_basis_bytes(512, 19);
        let left_key = compile_spectral_key(&analyze_topology(&left).unwrap()).unwrap();
        let right_key = compile_spectral_key(&analyze_topology(&right).unwrap()).unwrap();
        assert_eq!(left_key.serialize().len(), 8);
        assert_ne!(left_key, right_key);
        assert_ne!(
            synthesise_spectral_bytes(left_key).unwrap(),
            synthesise_spectral_bytes(right_key).unwrap()
        );
    }

    #[test]
    fn spectral_exception_stream_is_canonical_and_lossless() {
        let input = walsh_basis_bytes(512, 7);
        let key = compile_spectral_key(&analyze_topology(&input).unwrap()).unwrap();
        let mut changed = input.clone();
        changed[0] ^= 0b0000_0011;
        changed[63] ^= 0b1000_0000;
        let predictor = synthesise_spectral_bytes(key).unwrap();
        let residual = super::encode_spectral_exceptions(&changed, &predictor).unwrap();
        let block = SpectralBlock {
            original_len: changed.len() as u32,
            key: key.serialize().to_vec(),
            residual,
        };
        assert_eq!(super::decode_spectral_block(&block).unwrap(), changed);

        let malformed = SpectralBlock {
            residual: vec![0x80, 0x00],
            ..block
        };
        assert!(super::decode_spectral_block(&malformed).is_err());
    }

    #[test]
    fn operator_k_is_one_constant_and_u_k_is_an_involution() {
        let source = [0x3C_u8; 64];
        let key = build_operator_codec_key(&source).unwrap();
        assert_eq!(key.serialize().len(), 8);
        for value in [0, 1, u64::MAX, 0x0123_4567_89AB_CDEF] {
            let transformed = apply_operator_word_u_k(value, &key.serialize()).unwrap();
            assert_eq!(
                apply_operator_word_u_k(transformed, &key.serialize()).unwrap(),
                value
            );
        }
    }

    #[test]
    fn operator_is_never_accepted_without_strictly_shorter_parity_trajectory() {
        for seed in 0_u64..64 {
            let input = (0..64)
                .flat_map(|index| {
                    seed.wrapping_add(index)
                        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                        .to_le_bytes()
                })
                .collect::<Vec<_>>();
            let original_words = input
                .chunks_exact(8)
                .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
                .collect::<Vec<_>>();
            if let Some(BlockEncoding::Operator(block)) = try_encode_operator_block(&input).unwrap()
            {
                let key = MagicKey::parse(&block.key).unwrap();
                let transformed = original_words
                    .iter()
                    .map(|word| apply_operator_word_u_k(*word, &key.serialize()).unwrap())
                    .collect::<Vec<_>>();
                assert!(
                    minimal_common_terminal_steps(&transformed)
                        < minimal_common_terminal_steps(&original_words)
                );
            }
        }
    }

    #[test]
    fn malformed_fixed_width_k_is_rejected_by_archive_parser() {
        let archive = Archive {
            header: ArchiveHeader {
                base_version: 1,
                block_size_bytes: 64,
                original_size: 64,
                block_count: 1,
            },
            blocks: vec![BlockEncoding::Spectral(SpectralBlock {
                original_len: 64,
                key: vec![0; 7],
                residual: Vec::new(),
            })],
        };
        assert!(parse_archive(&serialize_single_layer_archive(&archive).unwrap()).is_err());
    }

    #[test]
    fn unsupported_version_trailing_bytes_and_invalid_raw_lengths_are_rejected() {
        let input = b"0123456789".repeat(500);
        let mut archive = compress_bytes(&input, None).unwrap();
        assert!(archive.starts_with(MAGIC_MULTI));
        archive[8] = 2;
        assert!(decompress_bytes(&archive).is_err());

        let mut archive = compress_bytes(&input, None).unwrap();
        archive.push(0xAA);
        assert!(decompress_bytes(&archive).is_err());

        let malformed = Archive {
            header: ArchiveHeader {
                base_version: 1,
                block_size_bytes: 8,
                original_size: 8,
                block_count: 1,
            },
            blocks: vec![BlockEncoding::Raw(RawBlock {
                original_len: 8,
                payload: vec![0; 7],
            })],
        };
        assert!(super::decompress_single_layer_bytes(
            &serialize_single_layer_archive(&malformed).unwrap()
        )
        .is_err());
    }

    #[test]
    fn operator_wire_format_roundtrips_terminal_steps_and_parity() {
        let source = [0x3C_u8; 64];
        let key = build_operator_codec_key(&source).unwrap();
        let value = 0x0123_4567_89AB_CDEF_u64;
        let transformed = apply_operator_word_u_k(value, &key.serialize()).unwrap();
        let trajectory =
            crate::domain::kernel::trajectory::encode_parity_trajectory(transformed, 63).unwrap();
        let block = OperatorBlock {
            original_len: 8,
            key: key.serialize().to_vec(),
            terminal: trajectory.terminal,
            steps: 63,
            breadcrumbs: super::pack_bools(&trajectory.crumbs),
        };
        assert_eq!(
            super::decode_operator_block(&block).unwrap(),
            value.to_le_bytes()
        );
    }
}
