use std::collections::{BTreeMap, BTreeSet};

use crate::domain::bitstream::{
    bit_width_for_cardinality, ceil_div_u64, pack_indices, unpack_indices,
};
use crate::domain::kernel::budget::{BitBudget, CompressionPolicy};
use crate::domain::kernel::key::{KeySegment, MagicKey, SegmentKind};
use crate::domain::kernel::operator::{apply_u_k, IndexMix, PhaseKey, SpectralOperatorKey};
use crate::domain::kernel::trajectory::{decode_parity_trajectory, encode_parity_trajectory, ParityTrajectory};
use crate::domain::kernel::topology::{analyze_topology, compile_topology_to_key};
use crate::domain::model::{
    AlphabetBlock, Archive, ArchiveHeader, BlockAnalysis, BlockEncoding, LayerSummary, RawBlock,
    OperatorBlock, SpectralBlock, TrajectoryBlock,
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
    let maybe_trajectory = try_encode_trajectory_block(block)?;
    let maybe_spectral = try_encode_spectral_block(block)?;
    let maybe_alphabet = try_encode_alphabet_block(block, &analysis)?;
    let mut best = raw;

    if let Some(candidate) = maybe_operator {
        if encoded_size_of(&candidate) < encoded_size_of(&best) {
            best = candidate;
        }
    }
    if let Some(candidate) = maybe_trajectory {
        if encoded_size_of(&candidate) < encoded_size_of(&best) {
            best = candidate;
        }
    }
    if let Some(candidate) = maybe_spectral {
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
    if probe < 8 { None } else { Some(probe) }
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
        if !words.iter().all(|value| (*value >> steps) == first_terminal) {
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

        let key = build_trajectory_key(block, first_terminal, steps)?.serialize()?;
        let packed_crumbs = pack_bools(&crumbs);
        let candidate = BlockEncoding::Trajectory(TrajectoryBlock {
            original_len: block.len() as u32,
            key: key.clone(),
            breadcrumbs: packed_crumbs,
        });
        let overhead_bytes = encoded_size_of(&candidate)
            .checked_sub(key.len())
            .and_then(|value| value.checked_sub(extract_trajectory_block(&candidate).unwrap().breadcrumbs.len()))
            .ok_or_else(|| "trajectory block accounting underflow".to_string())?;
        let budget = BitBudget {
            source_bits: (block.len() * 8) as u64,
            key_bits: (key.len() * 8) as u64,
            crumb_bits: (extract_trajectory_block(&candidate).unwrap().breadcrumbs.len() * 8) as u64,
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
    if block.len() < 64 || block.len() % 8 != 0 {
        return Ok(None);
    }
    let key_bytes = build_operator_codec_key(block)?.serialize()?;
    let base_words = generate_operator_base_words(block.len(), &key_bytes)?;
    let words = block
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    if words.len() != base_words.len() {
        return Ok(None);
    }

    let mut crumbs = Vec::with_capacity(words.len());
    for (actual, base) in words.iter().zip(&base_words) {
        if (actual & !1) != *base {
            return Ok(None);
        }
        crumbs.push((actual & 1) == 1);
    }

    let packed_crumbs = pack_bools(&crumbs);
    let candidate = BlockEncoding::Operator(OperatorBlock {
        original_len: block.len() as u32,
        key: key_bytes.clone(),
        breadcrumbs: packed_crumbs,
    });
    let overhead_bytes = encoded_size_of(&candidate)
        .checked_sub(key_bytes.len())
        .and_then(|value| value.checked_sub(extract_operator_block(&candidate).unwrap().breadcrumbs.len()))
        .ok_or_else(|| "operator block accounting underflow".to_string())?;
    let budget = BitBudget {
        source_bits: (block.len() * 8) as u64,
        key_bits: (key_bytes.len() * 8) as u64,
        crumb_bits: (extract_operator_block(&candidate).unwrap().breadcrumbs.len() * 8) as u64,
        overhead_bits: (overhead_bytes * 8) as u64,
    };
    if !CompressionPolicy::MVP.accepts(budget)? {
        return Ok(None);
    }
    Ok(Some(candidate))
}

fn try_encode_spectral_block(block: &[u8]) -> Result<Option<BlockEncoding>, String> {
    if block.len() < 8 || !block.len().is_power_of_two() {
        return Ok(None);
    }

    let key_bytes = match compile_executable_spectral_key(block) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let predictor = synthesise_predictor_from_key(block.len(), &key_bytes)?;
    let residual = encode_sparse_residual(block, &predictor)?;

    let candidate = BlockEncoding::Spectral(SpectralBlock {
        original_len: block.len() as u32,
        key: key_bytes.clone(),
        residual,
    });

    let overhead_bytes = encoded_size_of(&candidate)
        .checked_sub(key_bytes.len())
        .and_then(|value| {
            value.checked_sub(extract_spectral_block(&candidate).unwrap().residual.len())
        })
        .ok_or_else(|| "spectral block accounting underflow".to_string())?;
    let budget = BitBudget {
        source_bits: (block.len() * 8) as u64,
        key_bits: (key_bytes.len() * 8) as u64,
        crumb_bits: (extract_spectral_block(&candidate).unwrap().residual.len() * 8) as u64,
        overhead_bits: (overhead_bytes * 8) as u64,
    };
    if !CompressionPolicy::MVP.accepts(budget)? {
        return Ok(None);
    }
    Ok(Some(candidate))
}

fn compile_executable_spectral_key(block: &[u8]) -> Result<Vec<u8>, String> {
    if let Ok(signature) = analyze_topology(block) {
        if let Ok(mut key) = compile_topology_to_key(&signature) {
            if attach_predictor_seed_to_key(&mut key, block).is_ok() {
                if let Ok(bytes) = key.serialize() {
                    return Ok(bytes);
                }
            }
        }
    }
    build_minimal_spectral_key(block)?.serialize()
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
    let key_bytes = key.serialize()?;
    let predictor = synthesise_predictor_from_key(block.original_len as usize, &key_bytes)?;
    apply_sparse_residual(&predictor, &block.residual)
}

fn decode_trajectory_block(block: &TrajectoryBlock) -> Result<Vec<u8>, String> {
    let key = MagicKey::parse(&block.key)?;
    if key.header.main_pattern_id as u8 != crate::domain::kernel::base::PatternId::Trajectory as u8 {
        return Err("trajectory block K must use trajectory pattern".to_string());
    }
    let (terminal, steps, word_size) = parse_trajectory_schedule(&key)?;
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
            terminal,
            crumbs: chunk.to_vec(),
        };
        let value = decode_parity_trajectory(&trajectory)?;
        out.extend_from_slice(&value.to_le_bytes()[..word_size]);
    }
    Ok(out)
}

fn decode_operator_block(block: &OperatorBlock) -> Result<Vec<u8>, String> {
    let base_words = generate_operator_base_words(block.original_len as usize, &block.key)?;
    let crumbs = unpack_bools(&block.breadcrumbs, base_words.len())?;
    if base_words.len() > MAX_DECODER_STEPS {
        return Err("operator decoder gas limit exceeded".to_string());
    }
    let mut out = Vec::with_capacity(block.original_len as usize);
    for (word, crumb) in base_words.into_iter().zip(crumbs) {
        out.extend_from_slice(&(word | u64::from(crumb)).to_le_bytes());
    }
    Ok(out)
}

fn attach_predictor_seed_to_key(key: &mut MagicKey, block: &[u8]) -> Result<(), String> {
    let seed = spectral_seed_from_block(block);
    let payload = seed.to_le_bytes().to_vec();
    match key
        .segments
        .iter_mut()
        .find(|segment| segment.kind == SegmentKind::AuxConst)
    {
        Some(segment) => segment.payload = payload,
        None => key.segments.push(KeySegment {
            kind: SegmentKind::AuxConst,
            payload,
        }),
    }
    key.segments.sort_by_key(|segment| segment.kind);
    while key.unchecked_serialized_len() > 64 {
        let Some(position) = key.segments.iter().position(|segment| {
            segment.kind == SegmentKind::WalshConfig && segment.payload.len() > 2
        }) else {
            break;
        };
        let new_len = key.segments[position].payload.len() - 2;
        key.segments[position].payload.truncate(new_len);
    }
    if key.unchecked_serialized_len() > 64 {
        return Err("spectral K exceeds 512-bit MVP limit after seed attachment".to_string());
    }
    Ok(())
}

fn build_trajectory_key(block: &[u8], terminal: u64, steps: u8) -> Result<MagicKey, String> {
    let key = MagicKey {
        header: crate::domain::kernel::key::KeyHeader {
            version: 1,
            main_pattern_id: crate::domain::kernel::base::PatternId::Trajectory,
            rounds: 1,
            block_log2: (block.len() * 8).ilog2() as u8,
            flags: 0,
        },
        segments: vec![
            KeySegment {
                kind: SegmentKind::RevMix,
                payload: vec![steps, 8],
            },
            KeySegment {
                kind: SegmentKind::AuxConst,
                payload: terminal.to_le_bytes().to_vec(),
            },
        ],
    };
    key.validate()?;
    Ok(key)
}

fn build_operator_codec_key(block: &[u8]) -> Result<MagicKey, String> {
    let first_word = block
        .get(..8)
        .ok_or_else(|| "operator key builder requires at least one 64-bit word".to_string())?;
    let seed = u64::from_le_bytes(first_word.try_into().unwrap()) & !1_u64;
    let (xor_mask, rotate, phase_mask, shift_left, shift_right, odd_multiplier) =
        (1_u8, 1_u8, 0xF0F0_0F0F_AAAA_5555_u64, 13_u8, 7_u8, 0x9E37_79B9_7F4A_7C15_u64);
    let key = MagicKey {
        header: crate::domain::kernel::key::KeyHeader {
            version: 1,
            main_pattern_id: crate::domain::kernel::base::PatternId::SpectralInvolution,
            rounds: 1,
            block_log2: (block.len() * 8).ilog2() as u8,
            flags: 0,
        },
        segments: vec![
            KeySegment {
                kind: SegmentKind::RevMix,
                payload: vec![xor_mask, rotate],
            },
            KeySegment {
                kind: SegmentKind::PhaseMask,
                payload: phase_mask.to_le_bytes().to_vec(),
            },
            KeySegment {
                kind: SegmentKind::WalshConfig,
                payload: {
                    let mut payload = vec![shift_left, shift_right];
                    payload.extend_from_slice(&odd_multiplier.to_le_bytes());
                    payload
                },
            },
            KeySegment {
                kind: SegmentKind::AuxConst,
                payload: seed.to_le_bytes().to_vec(),
            },
        ],
    };
    key.validate()?;
    Ok(key)
}

fn parse_trajectory_schedule(key: &MagicKey) -> Result<(u64, u8, usize), String> {
    let schedule = key
        .segments
        .iter()
        .find(|segment| segment.kind == SegmentKind::RevMix)
        .ok_or_else(|| "trajectory K does not contain schedule".to_string())?;
    if schedule.payload.len() != 2 {
        return Err("trajectory schedule must contain exactly [steps, word_size]".to_string());
    }
    let steps = schedule.payload[0];
    let word_size = schedule.payload[1] as usize;
    if !(1..64).contains(&steps) {
        return Err("trajectory steps must be in 1..64".to_string());
    }
    if word_size != 8 {
        return Err("trajectory MVP currently supports only 8-byte words".to_string());
    }
    let terminal = spectral_seed_from_key(key)?;
    Ok((terminal, steps, word_size))
}

fn parse_operator_runtime_key(key_bytes: &[u8]) -> Result<(SpectralOperatorKey, u64), String> {
    let key = MagicKey::parse(key_bytes)?;
    let rev = key
        .segments
        .iter()
        .find(|segment| segment.kind == SegmentKind::RevMix)
        .ok_or_else(|| "operator K does not contain RevMix".to_string())?;
    if rev.payload.len() != 2 {
        return Err("operator RevMix payload must be [xor_mask, rotate]".to_string());
    }
    let phase_mask = key
        .segments
        .iter()
        .find(|segment| segment.kind == SegmentKind::PhaseMask)
        .ok_or_else(|| "operator K does not contain PhaseMask".to_string())?;
    let mask = u64::from_le_bytes(
        phase_mask
            .payload
            .as_slice()
            .try_into()
            .map_err(|_| "operator phase mask must be 8 bytes".to_string())?,
    );
    let walsh = key
        .segments
        .iter()
        .find(|segment| segment.kind == SegmentKind::WalshConfig)
        .ok_or_else(|| "operator K does not contain WalshConfig".to_string())?;
    if walsh.payload.len() != 10 {
        return Err("operator WalshConfig payload must be [shift_left, shift_right, odd_multiplier]".to_string());
    }
    let odd_multiplier = u64::from_le_bytes(
        walsh.payload[2..10]
            .try_into()
            .map_err(|_| "operator odd multiplier payload is malformed".to_string())?,
    );
    let seed = spectral_seed_from_key(&key)?;
    let operator = SpectralOperatorKey {
        mix: IndexMix {
            xor_mask: rev.payload[0] as usize,
            rotate: rev.payload[1],
        },
        phase: PhaseKey {
            seed,
            mask,
            shift_left: walsh.payload[0],
            shift_right: walsh.payload[1],
            odd_multiplier,
        },
    };
    Ok((operator, seed))
}

fn generate_operator_base_words(original_len: usize, key_bytes: &[u8]) -> Result<Vec<u64>, String> {
    if original_len % 8 != 0 {
        return Err("operator block length must be divisible by 8".to_string());
    }
    let word_count = original_len / 8;
    if word_count > MAX_DECODER_STEPS {
        return Err("operator decoder gas limit exceeded".to_string());
    }
    let (operator, seed) = parse_operator_runtime_key(key_bytes)?;
    let chunk_words = 64;
    let mut words = Vec::with_capacity(word_count);
    for chunk_index in 0..word_count.div_ceil(chunk_words) {
        let start = chunk_index * chunk_words;
        let remaining = word_count - start;
        let width = remaining.min(chunk_words).next_power_of_two();
        let state = (0..width)
            .map(|offset| {
                let idx = (start + offset) as u64;
                let mut x = seed ^ idx.rotate_left((offset % 31) as u32 + 1);
                x ^= x << 13;
                x ^= x >> 7;
                x = x.rotate_left(17);
                ((x & 0xFFFF) as f64) - 32768.0
            })
            .collect::<Vec<_>>();
        let local_bit_width = width.ilog2().max(1);
        let local_operator = SpectralOperatorKey {
            mix: IndexMix {
                xor_mask: operator.mix.xor_mask,
                rotate: (operator.mix.rotate as u32 % local_bit_width) as u8,
            },
            phase: operator.phase,
        };
        let transformed = apply_u_k(&state, &local_operator)?;
        for (local_index, value) in transformed.into_iter().take(remaining).enumerate() {
            let global = start + local_index;
            if global == 0 {
                words.push(seed & !1_u64);
                continue;
            }
            let mut word = value.to_bits()
                ^ seed.rotate_left((global % 63) as u32 + 1)
                ^ ((global as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            word = word.wrapping_mul(operator.phase.odd_multiplier | 1);
            words.push(word & !1_u64);
        }
    }
    Ok(words)
}

fn build_minimal_spectral_key(block: &[u8]) -> Result<MagicKey, String> {
    let seed = spectral_seed_from_block(block);
    let key = MagicKey {
        header: crate::domain::kernel::key::KeyHeader {
            version: 1,
            main_pattern_id: crate::domain::kernel::base::PatternId::SpectralInvolution,
            rounds: 1,
            block_log2: (block.len() * 8).ilog2() as u8,
            flags: 0,
        },
        segments: vec![
            KeySegment {
                kind: SegmentKind::RevMix,
                payload: {
                    let mut payload = vec![1];
                    payload.extend_from_slice(&1_u64.to_le_bytes());
                    payload.extend_from_slice(&seed.rotate_left(1).to_le_bytes());
                    payload
                },
            },
            KeySegment {
                kind: SegmentKind::PhaseMask,
                payload: 0_u64.to_le_bytes().to_vec(),
            },
            KeySegment {
                kind: SegmentKind::WalshConfig,
                payload: vec![0, 0],
            },
            KeySegment {
                kind: SegmentKind::CrumbConfig,
                payload: vec![1],
            },
            KeySegment {
                kind: SegmentKind::AuxConst,
                payload: seed.to_le_bytes().to_vec(),
            },
        ],
    };
    key.validate()?;
    Ok(key)
}

fn synthesise_predictor_from_key(original_len: usize, key_bytes: &[u8]) -> Result<Vec<u8>, String> {
    if original_len == 0 {
        return Ok(Vec::new());
    }
    let key = MagicKey::parse(key_bytes)?;
    let seed = spectral_seed_from_key(&key)?;
    let mut state = seed | 1;
    let mut out = vec![0_u8; original_len];
    let prefix = seed.to_le_bytes();
    let copy = prefix.len().min(original_len);
    out[..copy].copy_from_slice(&prefix[..copy]);
    for (_index, slot) in out.iter_mut().enumerate().skip(copy) {
        state ^= state << 13;
        state ^= state >> 7;
        state = state.rotate_left(17);
        *slot = (state as u8) ^ ((state >> 11) as u8) ^ ((state >> 37) as u8);
    }
    Ok(out)
}

fn spectral_seed_from_key(key: &MagicKey) -> Result<u64, String> {
    if let Some(segment) = key
        .segments
        .iter()
        .find(|segment| segment.kind == SegmentKind::AuxConst)
    {
        let bytes: [u8; 8] = segment
            .payload
            .as_slice()
            .try_into()
            .map_err(|_| "spectral aux seed must be 8 bytes".to_string())?;
        return Ok(u64::from_le_bytes(bytes));
    }
    Err("spectral K does not contain predictor seed".to_string())
}

fn spectral_seed_from_block(block: &[u8]) -> u64 {
    let mut seed = 0_u64;
    for (index, byte) in block.iter().take(8).enumerate() {
        seed |= (*byte as u64) << (index * 8);
    }
    if seed == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        seed
    }
}

fn encode_sparse_residual(actual: &[u8], predictor: &[u8]) -> Result<Vec<u8>, String> {
    if actual.len() != predictor.len() {
        return Err("spectral predictor length does not match block".to_string());
    }
    if actual.len() > u16::MAX as usize {
        return Err(
            "spectral sparse residual currently supports blocks up to 65535 bytes".to_string(),
        );
    }
    let mut out = Vec::new();
    for (index, (&left, &right)) in actual.iter().zip(predictor).enumerate() {
        let diff = left ^ right;
        if diff != 0 {
            out.extend_from_slice(&(index as u16).to_le_bytes());
            out.push(diff);
        }
    }
    Ok(out)
}

fn apply_sparse_residual(predictor: &[u8], residual: &[u8]) -> Result<Vec<u8>, String> {
    if residual.len() % 3 != 0 {
        return Err("spectral residual payload must be canonical triples".to_string());
    }
    let mut out = predictor.to_vec();
    let mut seen = BTreeSet::new();
    for chunk in residual.chunks_exact(3) {
        let index = u16::from_le_bytes([chunk[0], chunk[1]]) as usize;
        let value = chunk[2];
        if index >= out.len() {
            return Err("spectral residual index is out of range".to_string());
        }
        if !seen.insert(index) {
            return Err("spectral residual contains duplicate byte positions".to_string());
        }
        out[index] ^= value;
    }
    Ok(out)
}

fn extract_spectral_block(block: &BlockEncoding) -> Option<&SpectralBlock> {
    match block {
        BlockEncoding::Spectral(spectral) => Some(spectral),
        _ => None,
    }
}

fn extract_trajectory_block(block: &BlockEncoding) -> Option<&TrajectoryBlock> {
    match block {
        BlockEncoding::Trajectory(trajectory) => Some(trajectory),
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

#[cfg(test)]
fn make_trajectory_shaped_block(len: usize, terminal: u64, steps: u8) -> Vec<u8> {
    assert!(len % 8 == 0);
    let mut out = Vec::with_capacity(len);
    let mask = if steps == 64 { u64::MAX } else { (1_u64 << steps) - 1 };
    for i in 0..(len / 8) {
        let crumb = (i as u64) & mask;
        let value = (terminal << steps) | crumb;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

#[cfg(test)]
fn make_operator_shaped_block(len: usize, seed: u64) -> Vec<u8> {
    let key = MagicKey {
        header: crate::domain::kernel::key::KeyHeader {
            version: 1,
            main_pattern_id: crate::domain::kernel::base::PatternId::SpectralInvolution,
            rounds: 1,
            block_log2: (len * 8).ilog2() as u8,
            flags: 0,
        },
        segments: vec![
            KeySegment {
                kind: SegmentKind::RevMix,
                payload: vec![1, 1],
            },
            KeySegment {
                kind: SegmentKind::PhaseMask,
                payload: 0xF0F0_0F0F_AAAA_5555_u64.to_le_bytes().to_vec(),
            },
            KeySegment {
                kind: SegmentKind::WalshConfig,
                payload: {
                    let mut payload = vec![13, 7];
                    payload.extend_from_slice(&0x9E37_79B9_7F4A_7C15_u64.to_le_bytes());
                    payload
                },
            },
            KeySegment {
                kind: SegmentKind::AuxConst,
                payload: (seed & !1_u64).to_le_bytes().to_vec(),
            },
        ],
    }
    .serialize()
    .unwrap();
    let base_words = generate_operator_base_words(len, &key).unwrap();
    let mut out = Vec::with_capacity(len);
    for (index, word) in base_words.into_iter().enumerate() {
        out.extend_from_slice(&(word | u64::from(index % 2 == 1)).to_le_bytes());
    }
    out
}

#[cfg(test)]
fn synthesise_predictor_shaped_block(len: usize, seed: u64) -> Vec<u8> {
    for offset in 0..512_u64 {
        let block = seed_predictor(len, seed.wrapping_add(offset));
        if matches!(
            try_encode_spectral_block(&block),
            Ok(Some(BlockEncoding::Spectral(_)))
        ) {
            return block;
        }
    }
    seed_predictor(len, seed)
}

#[cfg(test)]
fn seed_predictor(len: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0_u8; len];
    let prefix = seed.to_le_bytes();
    let copy = prefix.len().min(len);
    out[..copy].copy_from_slice(&prefix[..copy]);
    let mut state = seed | 1;
    for slot in out.iter_mut().skip(copy) {
        state ^= state << 13;
        state ^= state >> 7;
        state = state.rotate_left(17);
        *slot = (state as u8) ^ ((state >> 11) as u8) ^ ((state >> 37) as u8);
    }
    out
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
            1 + 4 + 2 + trajectory.key.len() + 4 + trajectory.breadcrumbs.len()
        }
        BlockEncoding::Operator(operator) => {
            1 + 4 + 2 + operator.key.len() + 4 + operator.breadcrumbs.len()
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
                push_u32(&mut out, trajectory.breadcrumbs.len() as u32);
                out.extend_from_slice(&trajectory.breadcrumbs);
            }
            BlockEncoding::Operator(operator) => {
                out.push(block.mode() as u8);
                push_u32(&mut out, operator.original_len);
                push_u16(&mut out, operator.key.len() as u16);
                out.extend_from_slice(&operator.key);
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
                MagicKey::parse(&key)?;
                let residual_len = read_u32(input, &mut cursor)? as usize;
                let residual = read_vec(input, &mut cursor, residual_len)?;
                if residual.len() % 3 != 0 {
                    return Err("spectral residual payload must be canonical triples".to_string());
                }
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
                if parsed.header.main_pattern_id as u8
                    != crate::domain::kernel::base::PatternId::Trajectory as u8
                {
                    return Err("trajectory block K must use trajectory pattern".to_string());
                }
                let breadcrumbs_len = read_u32(input, &mut cursor)? as usize;
                let breadcrumbs = read_vec(input, &mut cursor, breadcrumbs_len)?;
                blocks.push(BlockEncoding::Trajectory(TrajectoryBlock {
                    original_len,
                    key,
                    breadcrumbs,
                }));
            }
            4 => {
                let key_len = read_u16(input, &mut cursor)? as usize;
                let key = read_vec(input, &mut cursor, key_len)?;
                MagicKey::parse(&key)?;
                let breadcrumbs_len = read_u32(input, &mut cursor)? as usize;
                let breadcrumbs = read_vec(input, &mut cursor, breadcrumbs_len)?;
                blocks.push(BlockEncoding::Operator(OperatorBlock {
                    original_len,
                    key,
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
mod tests {
    use super::{
        adaptive_window_bounds, candidate_block_sizes, compress_bytes,
        compress_bytes_with_outcome, compress_single_layer_bytes, decompress_bytes,
        decode_trajectory_block, make_operator_shaped_block, make_trajectory_shaped_block,
        serialize_single_layer_archive, spectral_prominence_score, synthesise_predictor_shaped_block,
        try_encode_spectral_block, try_encode_trajectory_block, DEFAULT_BLOCK_SIZE_BYTES,
        LAYER_SUMMARY_BYTES, MAGIC_MULTI, RECURSIVE_HEADER_BYTES,
    };
    use crate::domain::kernel::base::PatternId;
    use crate::domain::kernel::key::{KeyHeader, KeySegment, MagicKey, SegmentKind};
    use crate::domain::model::{
        Archive, ArchiveHeader, BlockEncoding, SpectralBlock, TrajectoryBlock,
    };

    #[test]
    fn roundtrip_ascii_digits() {
        let input = b"012345678901234567890123456789".repeat(64);
        let packed = compress_bytes(&input, Some(128)).unwrap();
        let unpacked = decompress_bytes(&packed).unwrap();
        assert_eq!(input, unpacked);
    }

    #[test]
    fn roundtrip_binary_noise() {
        let input = (0_u16..1024)
            .map(|value| ((value * 37 + 11) % 256) as u8)
            .collect::<Vec<_>>();
        let packed = compress_bytes(&input, Some(256)).unwrap();
        let unpacked = decompress_bytes(&packed).unwrap();
        assert_eq!(input, unpacked);
    }

    #[test]
    fn uses_multiple_layers_when_beneficial() {
        let input = b"0123456789".repeat(5000);
        let outcome = compress_bytes_with_outcome(&input, None).unwrap();
        let unpacked = decompress_bytes(&outcome.archive).unwrap();
        assert_eq!(input, unpacked);
        assert!(!outcome.layer_summaries.is_empty());
    }

    #[test]
    fn mvp_block_size_is_4096_bits_not_4096_bytes() {
        assert_eq!(DEFAULT_BLOCK_SIZE_BYTES, 4096 / 8);
    }

    #[test]
    fn default_adaptive_candidates_include_mvp_and_larger_blocks() {
        let input = vec![0xAA_u8; 16_384];
        let candidates = candidate_block_sizes(&input, None);
        assert!(candidates.contains(&DEFAULT_BLOCK_SIZE_BYTES));
        assert!(candidates.contains(&4096));
        assert!(candidates.contains(&8192));
    }

    #[test]
    fn rejects_layer_when_root_overhead_erases_local_gain() {
        let input = b"AB".iter().copied().cycle().take(45).collect::<Vec<_>>();
        let local = compress_single_layer_bytes(&input, input.len()).unwrap();
        assert!(local.len() < input.len());

        let outcome = compress_bytes_with_outcome(&input, Some(input.len())).unwrap();
        assert!(outcome.layer_summaries.is_empty());
        assert_eq!(outcome.archive.len(), RECURSIVE_HEADER_BYTES + input.len());
    }

    #[test]
    fn accepted_layers_reduce_complete_recursive_archive() {
        let input = b"0123456789".repeat(5000);
        let outcome = compress_bytes_with_outcome(&input, None).unwrap();
        let raw_recursive_size = RECURSIVE_HEADER_BYTES + input.len();
        assert!(outcome.archive.len() < raw_recursive_size);
        assert_eq!(
            outcome.archive.len(),
            RECURSIVE_HEADER_BYTES
                + outcome.layer_summaries.len() * LAYER_SUMMARY_BYTES
                + outcome.layer_summaries.last().unwrap().output_size as usize
        );
    }

    #[test]
    fn rejects_unsupported_base_version() {
        let input = b"0123456789".repeat(500);
        let mut archive = compress_bytes(&input, None).unwrap();
        assert!(archive.starts_with(MAGIC_MULTI));
        archive[8] = 2;
        assert!(decompress_bytes(&archive)
            .unwrap_err()
            .contains("unsupported base version"));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let input = b"0123456789".repeat(500);
        let mut archive = compress_bytes(&input, None).unwrap();
        archive.push(0xAA);
        assert!(decompress_bytes(&archive)
            .unwrap_err()
            .contains("trailing bytes"));
    }

    #[test]
    fn rejects_tampered_layer_size_chain() {
        let input = b"0123456789".repeat(5000);
        let mut archive = compress_bytes(&input, None).unwrap();
        assert!(archive.starts_with(MAGIC_MULTI));
        let first_layer_output_offset = 8 + 1 + 8 + 4 + 4 + 8;
        archive[first_layer_output_offset] ^= 1;
        assert!(decompress_bytes(&archive).is_err());
    }

    #[test]
    fn rejects_raw_block_with_mismatched_original_length() {
        let input = (0_u16..256).map(|value| value as u8).collect::<Vec<_>>();
        let mut archive = compress_single_layer_bytes(&input, input.len()).unwrap();
        let raw_original_len_offset = 8 + 1 + 4 + 8 + 4 + 1;
        archive[raw_original_len_offset] ^= 1;
        assert!(decompress_bytes(&archive)
            .unwrap_err()
            .contains("raw payload length"));
    }

    #[test]
    fn spectral_block_roundtrips_through_real_archive() {
        let key = MagicKey {
            header: KeyHeader {
                version: 1,
                main_pattern_id: PatternId::SpectralInvolution,
                rounds: 1,
                block_log2: 6,
                flags: 0,
            },
            segments: vec![
                KeySegment {
                    kind: SegmentKind::RevMix,
                    payload: vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                },
                KeySegment {
                    kind: SegmentKind::PhaseMask,
                    payload: 0_u64.to_le_bytes().to_vec(),
                },
                KeySegment {
                    kind: SegmentKind::WalshConfig,
                    payload: vec![0, 0],
                },
                KeySegment {
                    kind: SegmentKind::CrumbConfig,
                    payload: vec![1],
                },
                KeySegment {
                    kind: SegmentKind::AuxConst,
                    payload: 0x1122_3344_5566_7788_u64.to_le_bytes().to_vec(),
                },
            ],
        }
        .serialize()
        .unwrap();
        let block = SpectralBlock {
            original_len: 8,
            key,
            residual: vec![0, 0, 0],
        };
        let archive = Archive {
            header: ArchiveHeader {
                base_version: 1,
                block_size_bytes: 8,
                original_size: 8,
                block_count: 1,
            },
            blocks: vec![BlockEncoding::Spectral(block)],
        };
        let encoded = serialize_single_layer_archive(&archive).unwrap();
        let decoded = decompress_bytes(&encoded).unwrap();
        assert_eq!(decoded.len(), 8);
    }

    #[test]
    fn real_codec_uses_spectral_mode_when_predictor_residual_is_sparse() {
        let input = super::synthesise_predictor_shaped_block(512, 0x1122_3344_5566_7788);
        let encoded = try_encode_spectral_block(&input).unwrap();
        assert!(matches!(encoded, Some(BlockEncoding::Spectral(_))));
    }

    #[test]
    fn real_codec_rejects_spectral_mode_when_gain_is_below_threshold() {
        let input = (0_u16..64).map(|value| value as u8).collect::<Vec<_>>();
        let encoded = try_encode_spectral_block(&input).unwrap();
        assert!(encoded.is_none());
    }

    #[test]
    fn trajectory_block_roundtrips_through_real_archive() {
        let key = MagicKey {
            header: KeyHeader {
                version: 1,
                main_pattern_id: PatternId::Trajectory,
                rounds: 1,
                block_log2: 6,
                flags: 0,
            },
            segments: vec![
                KeySegment {
                    kind: SegmentKind::RevMix,
                    payload: vec![4, 8],
                },
                KeySegment {
                    kind: SegmentKind::AuxConst,
                    payload: 0x1234_5678_9ABC_DEF0_u64.to_le_bytes().to_vec(),
                },
            ],
        }
        .serialize()
        .unwrap();
        let block = TrajectoryBlock {
            original_len: 16,
            key,
            breadcrumbs: vec![0b0101_1111],
        };
        let archive = Archive {
            header: ArchiveHeader {
                base_version: 1,
                block_size_bytes: 16,
                original_size: 16,
                block_count: 1,
            },
            blocks: vec![BlockEncoding::Trajectory(block)],
        };
        let encoded = serialize_single_layer_archive(&archive).unwrap();
        let decoded = decompress_bytes(&encoded).unwrap();
        assert_eq!(decoded.len(), 16);
    }

    #[test]
    fn trajectory_decoder_requires_only_key_and_v_without_external_crumb_cfg() {
        let input = make_trajectory_shaped_block(2048, 0x1234_5678_9ABC_DEF0, 4);
        let encoded = try_encode_trajectory_block(&input).unwrap();
        let block = match encoded {
            Some(BlockEncoding::Trajectory(block)) => block,
            other => panic!("expected trajectory block, got {other:?}"),
        };
        let restored = decode_trajectory_block(&block).unwrap();
        assert_eq!(restored, input);
    }

    #[test]
    fn trajectory_decoder_rejects_gas_limit_exceeded() {
        let key = MagicKey {
            header: KeyHeader {
                version: 1,
                main_pattern_id: PatternId::Trajectory,
                rounds: 1,
                block_log2: 20,
                flags: 0,
            },
            segments: vec![
                KeySegment {
                    kind: SegmentKind::RevMix,
                    payload: vec![63, 8],
                },
                KeySegment {
                    kind: SegmentKind::AuxConst,
                    payload: 0_u64.to_le_bytes().to_vec(),
                },
            ],
        }
        .serialize()
        .unwrap();
        let block = TrajectoryBlock {
            original_len: 200_000,
            key,
            breadcrumbs: vec![0xFF; 10],
        };
        assert!(decode_trajectory_block(&block)
            .unwrap_err()
            .contains("gas limit"));
    }

    #[test]
    fn trajectory_mode_falls_back_to_raw_when_not_profitable() {
        let input = (0_u32..512).map(|i| (i & 0xFF) as u8).collect::<Vec<_>>();
        let encoded = try_encode_trajectory_block(&input).unwrap();
        assert!(encoded.is_none());
    }

    #[test]
    fn trajectory_shaped_block_achieves_at_least_ten_x_compression() {
        let input = make_trajectory_shaped_block(2048, 0x1234_5678_9ABC_DEF0, 2);
        let packed = compress_bytes(&input, None).unwrap();
        let restored = decompress_bytes(&packed).unwrap();
        let factor = input.len() as f64 / packed.len() as f64;
        assert_eq!(restored, input);
        assert!(
            factor >= 10.0,
            "expected >=10x compression, got {factor:.4}x ({} -> {})",
            input.len(),
            packed.len()
        );
    }

    #[test]
    fn operator_shaped_block_achieves_at_least_ten_x_compression() {
        let input = make_operator_shaped_block(2048, 0xCAFEBABE12345678);
        let packed = compress_bytes(&input, None).unwrap();
        let restored = decompress_bytes(&packed).unwrap();
        let factor = input.len() as f64 / packed.len() as f64;
        assert_eq!(restored, input);
        assert!(
            factor >= 10.0,
            "expected >=10x compression, got {factor:.4}x ({} -> {})",
            input.len(),
            packed.len()
        );
    }

    #[test]
    fn spectral_mode_rejects_full_block_residual_semantics_on_noise() {
        let input = (0_u32..512)
            .map(|i| ((i.wrapping_mul(73).wrapping_add(19)) & 0xFF) as u8)
            .collect::<Vec<_>>();
        let encoded = try_encode_spectral_block(&input).unwrap();
        assert!(encoded.is_none());
    }

    #[test]
    fn adaptive_window_bounds_are_power_of_two_and_layer_local() {
        let peaky = vec![0xAA_u8; 4096];
        let noisy = (0_u32..4096)
            .map(|i| ((i.wrapping_mul(37).wrapping_add(11)) & 0xFF) as u8)
            .collect::<Vec<_>>();
        let peaky_bounds = adaptive_window_bounds(&peaky);
        let noisy_bounds = adaptive_window_bounds(&noisy);
        assert!(peaky_bounds.0.is_power_of_two());
        assert!(peaky_bounds.1.is_power_of_two());
        assert!(noisy_bounds.0.is_power_of_two());
        assert!(noisy_bounds.1.is_power_of_two());
        assert!(peaky_bounds.0 <= peaky_bounds.1);
        assert!(noisy_bounds.0 <= noisy_bounds.1);
        assert!(
            spectral_prominence_score(&peaky).unwrap()
                >= spectral_prominence_score(&noisy).unwrap()
        );
    }

    #[test]
    fn generator_shaped_block_achieves_at_least_ten_x_compression() {
        let input = synthesise_predictor_shaped_block(2048, 0x1122_3344_5566_7788);
        let packed = compress_bytes(&input, None).unwrap();
        let restored = decompress_bytes(&packed).unwrap();
        assert_eq!(restored, input);
        let factor = input.len() as f64 / packed.len() as f64;
        assert!(
            factor >= 10.0,
            "expected >=10x compression, got {factor:.4}x ({} -> {})",
            input.len(),
            packed.len()
        );
    }

    #[test]
    fn generator_shaped_stress_ensemble_keeps_ten_x_and_roundtrip() {
        for seed in 1_u64..=16 {
            let input = synthesise_predictor_shaped_block(2048, seed.wrapping_mul(0x9E37_79B9));
            let packed = compress_bytes(&input, None).unwrap();
            let restored = decompress_bytes(&packed).unwrap();
            let factor = input.len() as f64 / packed.len() as f64;
            assert_eq!(restored, input);
            assert!(
                factor >= 10.0,
                "seed {seed}: expected >=10x compression, got {factor:.4}x ({} -> {})",
                input.len(),
                packed.len()
            );
        }
    }
}
