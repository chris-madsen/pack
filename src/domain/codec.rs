use std::collections::{BTreeMap, BTreeSet};

use crate::domain::bitstream::{
    bit_width_for_cardinality, ceil_div_u64, pack_indices, unpack_indices,
};
use crate::domain::kernel::budget::{BitBudget, CompressionPolicy};
use crate::domain::kernel::key::{MagicKey, MagicKeyKind, OperatorBlueprint};
use crate::domain::kernel::operator::{
    apply_binary_u_k, strongest_binary_word_peaks, BinaryOperatorKey, BinaryWordPeak, IndexMix,
    PhaseKey, SpectralOperatorKey,
};
use crate::domain::kernel::spectral::synthesise_spectral_bits;
use crate::domain::kernel::topology::{
    analyze_topology, block_fingerprint, compile_spectral_key, compile_topology_to_key,
    TopologySignature,
};
use crate::domain::kernel::trajectory::{
    decode_parity_trajectory, encode_parity_trajectory, ParityTrajectory,
};
use crate::domain::model::{
    AlphabetBlock, Archive, ArchiveHeader, BlockAnalysis, BlockEncoding, LayerSummary,
    OperatorBlock, RawBlock, SparseAlphabetBlock, SpectralBlock, TrajectoryBlock,
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
        let archive = parse_archive(input)?;
        return Ok(vec![LayerSummary {
            block_size_bytes: archive.header.block_size_bytes,
            input_size: archive.header.original_size,
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

    // Cache topology: the Walsh transform is O(n log n); no need to run it twice
    // (once in try_encode_operator_block and once in try_encode_spectral_block).
    let cached_topology: Option<TopologySignature> =
        if block.len() >= 8 && (block.len() * 8).is_power_of_two() {
            analyze_topology(block).ok()
        } else {
            None
        };

    let maybe_operator = try_encode_operator_block(block, cached_topology.as_ref())?;
    let maybe_spectral = try_encode_spectral_block(block, cached_topology.as_ref())?;
    let maybe_trajectory = try_encode_trajectory_block(block)?;
    let maybe_sparse_alphabet = try_encode_sparse_alphabet_block(block, &analysis)?;
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
    if let Some(candidate) = maybe_sparse_alphabet {
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct TerminalPalettePlan {
    terminals: Vec<u64>,
    terminal_indices: Vec<u8>,
    terminal_index_bits: u8,
    breadcrumbs: Vec<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TrajectoryScore {
    encoded_bits: u64,
    steps: u8,
    terminal_count: usize,
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
            BlockEncoding::SparseAlphabet(alpha) => {
                let restored = decode_sparse_alphabet_block(&alpha)?;
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

fn try_encode_sparse_alphabet_block(
    block: &[u8],
    analysis: &BlockAnalysis,
) -> Result<Option<BlockEncoding>, String> {
    if analysis.unique_sorted_bytes.len() <= 1 {
        return Ok(None);
    }

    let counts = block
        .iter()
        .copied()
        .fold(BTreeMap::new(), |mut map, byte| {
            *map.entry(byte).or_insert(0_usize) += 1;
            map
        });
    let mut rarest = counts
        .into_iter()
        .map(|(byte, count)| (count, byte))
        .collect::<Vec<_>>();
    rarest.sort();

    let mut best: Option<BlockEncoding> = None;
    for exception_count in 1..=3 {
        let exception_alphabet = rarest
            .iter()
            .take(exception_count)
            .map(|(_, byte)| *byte)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if exception_alphabet.is_empty()
            || exception_alphabet.len() >= analysis.unique_sorted_bytes.len()
        {
            continue;
        }
        let dense_alphabet = analysis
            .unique_sorted_bytes
            .iter()
            .copied()
            .filter(|byte| !exception_alphabet.contains(byte))
            .collect::<Vec<_>>();
        let dense_bit_width = bit_width_for_cardinality(dense_alphabet.len());
        if dense_bit_width >= 8 {
            continue;
        }

        let dense_index_by_byte = dense_alphabet
            .iter()
            .copied()
            .enumerate()
            .map(|(index, byte)| (byte, index as u8))
            .collect::<BTreeMap<_, _>>();
        let exception_index_by_byte = exception_alphabet
            .iter()
            .copied()
            .enumerate()
            .map(|(index, byte)| (byte, index as u8))
            .collect::<BTreeMap<_, _>>();

        let mut dense_indices = Vec::with_capacity(block.len());
        let mut exception_indices = Vec::new();
        let mut exception_positions = Vec::new();
        for (position, byte) in block.iter().copied().enumerate() {
            if let Some(index) = exception_index_by_byte.get(&byte).copied() {
                exception_indices.push(index);
                exception_positions.push(position);
            } else {
                dense_indices.push(
                    dense_index_by_byte
                        .get(&byte)
                        .copied()
                        .ok_or_else(|| "dense alphabet lookup failed".to_string())?,
                );
            }
        }
        if exception_indices.is_empty() {
            continue;
        }

        let candidate = BlockEncoding::SparseAlphabet(SparseAlphabetBlock {
            original_len: block.len() as u32,
            dense_alphabet: dense_alphabet.clone(),
            dense_bit_width,
            dense_breadcrumbs: pack_indices(&dense_indices, dense_bit_width),
            exception_alphabet: exception_alphabet.clone(),
            exception_indices: pack_indices(
                &exception_indices,
                bit_width_for_cardinality(exception_alphabet.len()),
            ),
            exception_positions: encode_position_deltas(&exception_positions),
        });
        if best
            .as_ref()
            .map(|current| encoded_size_of(&candidate) < encoded_size_of(current))
            .unwrap_or(true)
        {
            best = Some(candidate);
        }
    }

    Ok(best)
}

fn trajectory_block_crumb_bytes(candidate: &BlockEncoding) -> usize {
    match candidate {
        BlockEncoding::Trajectory(t) => t.breadcrumbs.len() + t.terminal_indices.len(),
        BlockEncoding::Operator(o) => o.breadcrumbs.len() + o.terminal_indices.len(),
        _ => 0,
    }
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
    for steps in candidate_steps() {
        let Some(plan) = build_terminal_palette_plan(&words, steps, 256)? else {
            continue;
        };
        let key = build_trajectory_key(block, steps)?.serialize().to_vec();
        let packed_crumbs = pack_bools(&plan.breadcrumbs);
        let packed_terminal_indices =
            pack_indices(&plan.terminal_indices, plan.terminal_index_bits);
        let candidate = BlockEncoding::Trajectory(TrajectoryBlock {
            original_len: block.len() as u32,
            key: key.clone(),
            terminals: plan.terminals.clone(),
            terminal_indices: packed_terminal_indices,
            steps,
            breadcrumbs: packed_crumbs,
        });
        let crumb_bytes = trajectory_block_crumb_bytes(&candidate);
        let overhead_bytes = encoded_size_of(&candidate)
            .checked_sub(key.len())
            .and_then(|v| v.checked_sub(crumb_bytes))
            .ok_or_else(|| "trajectory block accounting underflow".to_string())?;
        let budget = BitBudget {
            source_bits: (block.len() * 8) as u64,
            key_bits: (key.len() * 8) as u64,
            crumb_bits: (crumb_bytes * 8) as u64,
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

fn try_encode_operator_block(
    block: &[u8],
    cached_topology: Option<&TopologySignature>,
) -> Result<Option<BlockEncoding>, String> {
    if block.len() < 64 || block.len() % 8 != 0 || !block.len().is_power_of_two() {
        return Ok(None);
    }
    let words = block
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let base_key = match select_operator_codec_key_from_family(block, &words, cached_topology) {
        Ok(Some(key)) => key,
        Ok(None) | Err(_) => return Ok(None),
    };
    let base_key_bytes = base_key.serialize();
    let operator = parse_operator_runtime_key(&base_key_bytes)?;
    let transformed = words
        .iter()
        .map(|word| apply_operator_word_u_k_with_runtime(*word, &operator))
        .collect::<Result<Vec<_>, _>>()?;
    let direct_score = grouped_trajectory_score(&words)?;
    let transformed_score = grouped_trajectory_score(&transformed)?;
    if !is_better_trajectory_score(transformed_score, direct_score) {
        return Ok(None);
    }

    let mut best: Option<BlockEncoding> = None;
    for steps in candidate_steps() {
        let Some(plan) = build_terminal_palette_plan(&transformed, steps, 256)? else {
            continue;
        };
        let key_bytes = base_key.serialize().to_vec();
        let candidate = BlockEncoding::Operator(OperatorBlock {
            original_len: block.len() as u32,
            key: key_bytes.clone(),
            terminals: plan.terminals.clone(),
            terminal_indices: pack_indices(&plan.terminal_indices, plan.terminal_index_bits),
            steps,
            breadcrumbs: pack_bools(&plan.breadcrumbs),
        });
        let crumb_bytes = trajectory_block_crumb_bytes(&candidate);
        let overhead_bytes = encoded_size_of(&candidate)
            .checked_sub(key_bytes.len())
            .and_then(|v| v.checked_sub(crumb_bytes))
            .ok_or_else(|| "operator block accounting underflow".to_string())?;
        let budget = BitBudget {
            source_bits: (block.len() * 8) as u64,
            key_bits: (key_bytes.len() * 8) as u64,
            crumb_bits: (crumb_bytes * 8) as u64,
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

fn candidate_steps() -> std::ops::Range<u8> {
    1_u8..64
}

fn build_terminal_palette_plan(
    words: &[u64],
    steps: u8,
    max_terminals: usize,
) -> Result<Option<TerminalPalettePlan>, String> {
    if words.is_empty() || !(1..64).contains(&steps) || max_terminals == 0 {
        return Ok(None);
    }

    let trajectories = words
        .iter()
        .map(|value| encode_parity_trajectory(*value, steps as usize))
        .collect::<Result<Vec<_>, _>>()?;
    let terminals = trajectories
        .iter()
        .map(|trajectory| trajectory.terminal)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if terminals.len() > max_terminals || terminals.len() > usize::from(u8::MAX) + 1 {
        return Ok(None);
    }

    let index_by_terminal = terminals
        .iter()
        .copied()
        .enumerate()
        .map(|(index, terminal)| (terminal, index as u8))
        .collect::<BTreeMap<_, _>>();
    let terminal_indices = trajectories
        .iter()
        .map(|trajectory| {
            index_by_terminal
                .get(&trajectory.terminal)
                .copied()
                .ok_or_else(|| "terminal palette lookup failed".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let breadcrumbs = trajectories
        .into_iter()
        .flat_map(|trajectory| trajectory.crumbs)
        .collect::<Vec<_>>();
    Ok(Some(TerminalPalettePlan {
        terminal_index_bits: bit_width_for_cardinality(terminals.len()),
        terminals,
        terminal_indices,
        breadcrumbs,
    }))
}

fn estimate_plan_bits(plan: &TerminalPalettePlan) -> u64 {
    plan.terminals.len() as u64 * 64
        + plan.terminal_indices.len() as u64 * plan.terminal_index_bits as u64
        + plan.breadcrumbs.len() as u64
}

fn best_grouped_trajectory_plan(
    words: &[u64],
) -> Result<Option<(u8, TerminalPalettePlan)>, String> {
    let mut best: Option<(u8, TerminalPalettePlan)> = None;
    for steps in candidate_steps() {
        let Some(plan) = build_terminal_palette_plan(words, steps, 256)? else {
            continue;
        };
        let better = best
            .as_ref()
            .map(|(_, current)| estimate_plan_bits(&plan) < estimate_plan_bits(current))
            .unwrap_or(true);
        if better {
            best = Some((steps, plan));
        }
    }
    Ok(best)
}

fn grouped_trajectory_score(words: &[u64]) -> Result<Option<TrajectoryScore>, String> {
    Ok(
        best_grouped_trajectory_plan(words)?.map(|(steps, plan)| TrajectoryScore {
            encoded_bits: estimate_plan_bits(&plan),
            steps,
            terminal_count: plan.terminals.len(),
        }),
    )
}

fn is_better_trajectory_score(
    candidate: Option<TrajectoryScore>,
    incumbent: Option<TrajectoryScore>,
) -> bool {
    match (candidate, incumbent) {
        (Some(left), Some(right)) => {
            left.encoded_bits < right.encoded_bits
                || (left.encoded_bits == right.encoded_bits
                    && (left.terminal_count < right.terminal_count
                        || (left.terminal_count == right.terminal_count
                            && left.steps < right.steps)))
        }
        (Some(_), None) => true,
        _ => false,
    }
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

fn decode_sparse_alphabet_block(block: &SparseAlphabetBlock) -> Result<Vec<u8>, String> {
    let original_len = block.original_len as usize;
    let exception_positions = decode_position_deltas(&block.exception_positions)?;
    let exception_count = exception_positions.len();
    if exception_count > original_len {
        return Err("sparse alphabet exception count exceeds block length".to_string());
    }
    let exception_indices = unpack_indices(
        &block.exception_indices,
        bit_width_for_cardinality(block.exception_alphabet.len()),
        exception_count,
    )?;
    let dense_count = original_len
        .checked_sub(exception_count)
        .ok_or_else(|| "sparse alphabet dense count underflow".to_string())?;
    let dense_indices =
        unpack_indices(&block.dense_breadcrumbs, block.dense_bit_width, dense_count)?;

    let exception_map = exception_positions
        .into_iter()
        .zip(exception_indices)
        .map(|(position, index)| {
            let byte = *block
                .exception_alphabet
                .get(index as usize)
                .ok_or_else(|| "sparse alphabet exception index is out of range".to_string())?;
            Ok((position, byte))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;

    let mut dense_cursor = 0_usize;
    let mut out = Vec::with_capacity(original_len);
    for position in 0..original_len {
        if let Some(byte) = exception_map.get(&position).copied() {
            out.push(byte);
        } else {
            let index = *dense_indices
                .get(dense_cursor)
                .ok_or_else(|| "dense sparse-alphabet stream is truncated".to_string())?;
            dense_cursor += 1;
            out.push(
                *block
                    .dense_alphabet
                    .get(index as usize)
                    .ok_or_else(|| "dense sparse-alphabet index is out of range".to_string())?,
            );
        }
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
    let restored = apply_spectral_exceptions(&predictor, &block.residual, program.bit_len)?;
    if encode_spectral_exceptions(&restored, &predictor)? != block.residual {
        return Err("spectral residual encoding is not canonical".to_string());
    }
    Ok(restored)
}

fn decode_trajectory_block(block: &TrajectoryBlock) -> Result<Vec<u8>, String> {
    let key = MagicKey::parse(&block.key)?;
    if key.trajectory_steps()? != block.steps {
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
    let terminal_indices = unpack_indices(
        &block.terminal_indices,
        bit_width_for_cardinality(block.terminals.len()),
        word_count,
    )?;
    let crumbs = unpack_bools(&block.breadcrumbs, total_steps)?;
    let mut out = Vec::with_capacity(original_len);
    for (terminal_index, chunk) in terminal_indices
        .into_iter()
        .zip(crumbs.chunks_exact(steps as usize))
    {
        let terminal = *block
            .terminals
            .get(terminal_index as usize)
            .ok_or_else(|| "trajectory terminal index is out of range".to_string())?;
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
    if block.original_len as usize % 8 != 0 {
        return Err("operator block length must be divisible by 8".to_string());
    }
    MagicKey::parse(&block.key)?.require_kind(MagicKeyKind::Operator)?;
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
    let terminal_indices = unpack_indices(
        &block.terminal_indices,
        bit_width_for_cardinality(block.terminals.len()),
        word_count,
    )?;
    let crumbs = unpack_bools(&block.breadcrumbs, total_steps)?;
    let operator = parse_operator_runtime_key(&block.key)?;
    let mut out = Vec::with_capacity(block.original_len as usize);
    for (terminal_index, word_crumbs) in terminal_indices
        .into_iter()
        .zip(crumbs.chunks_exact(steps as usize))
    {
        let terminal = *block
            .terminals
            .get(terminal_index as usize)
            .ok_or_else(|| "operator terminal index is out of range".to_string())?;
        let transformed = decode_parity_trajectory(&ParityTrajectory {
            terminal,
            crumbs: word_crumbs.to_vec(),
        })?;
        let word = invert_operator_word_u_k_with_runtime(transformed, &operator)?;
        out.extend_from_slice(&word.to_le_bytes());
    }
    Ok(out)
}

fn build_trajectory_key(block: &[u8], steps: u8) -> Result<MagicKey, String> {
    let fingerprint = block_fingerprint(block)?;
    MagicKey::from_trajectory_payload(fingerprint, steps)
}

#[cfg(test)]
fn build_operator_codec_key(block: &[u8]) -> Result<MagicKey, String> {
    compile_topology_to_key(&analyze_topology(block)?)
}

fn select_operator_codec_key_from_family(
    block: &[u8],
    words: &[u64],
    cached_topology: Option<&TopologySignature>,
) -> Result<Option<MagicKey>, String> {
    let signature = match cached_topology {
        Some(sig) => sig.clone(),
        None => analyze_topology(block)?,
    };
    let original_score = grouped_trajectory_score(words)?;
    let candidates = operator_candidate_family(&signature, words)?;
    let mut best = None;
    let mut best_key = None;
    for candidate in candidates {
        let score = score_operator_key(words, candidate)?;
        if is_better_operator_score(score, best) {
            best = score;
            best_key = Some(candidate);
        }
    }

    if is_better_trajectory_score(best, original_score) {
        Ok(best_key)
    } else {
        Ok(None)
    }
}

fn score_operator_key(words: &[u64], key: MagicKey) -> Result<Option<TrajectoryScore>, String> {
    let operator = parse_operator_runtime_key(&key.serialize())?;
    let transformed = words
        .iter()
        .map(|word| apply_operator_word_u_k_with_runtime(*word, &operator))
        .collect::<Result<Vec<_>, _>>()?;
    grouped_trajectory_score(&transformed)
}

fn is_better_operator_score(
    candidate: Option<TrajectoryScore>,
    incumbent: Option<TrajectoryScore>,
) -> bool {
    is_better_trajectory_score(candidate, incumbent)
}

fn operator_candidate_family(
    signature: &TopologySignature,
    words: &[u64],
) -> Result<Vec<MagicKey>, String> {
    let base = compile_topology_to_key(signature)?;
    let base_blueprint = base.operator_blueprint()?;
    let fallback_shift = signature.shift_scores.first();
    let binary_peaks = strongest_binary_word_peaks(words, 4)?;
    let mut variants = Vec::new();
    for (dominant_slot, dominant) in binary_peaks.iter().enumerate() {
        for (secondary_slot, secondary) in binary_peaks.iter().enumerate() {
            if secondary_slot == dominant_slot {
                continue;
            }
            let Some(shift) = signature
                .shift_scores
                .get((dominant_slot + secondary_slot) % signature.shift_scores.len())
                .or(fallback_shift)
            else {
                continue;
            };
            let tertiary = binary_peaks
                .get((secondary_slot + 1) % binary_peaks.len())
                .unwrap_or(secondary);
            let primary_shift = encoded_primary_shift(dominant, secondary, shift.shift as u8);
            variants.push(OperatorBlueprint {
                dominant_index: dominant.bit as u16,
                dominant_positive: dominant.positive,
                primary_shift,
                shift_match: quantize_ratio(dominant.bias as usize, words.len(), 9) as u16,
                derivative_density: base_blueprint.derivative_density,
                popcnt_density: base_blueprint.popcnt_density,
                secondary_delta: encode_operator_delta(dominant.bit, secondary.bit),
                tertiary_delta: encode_operator_delta(dominant.bit, tertiary.bit),
                fingerprint_bias: ((signature.fingerprint
                    >> (((dominant_slot + secondary_slot) % 8) * 5))
                    & 0x1F) as u8,
            });
        }
    }
    Ok(variants
        .into_iter()
        .chain(std::iter::once(base_blueprint))
        .filter_map(|variant| MagicKey::from_operator_blueprint(&variant).ok())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn encoded_primary_shift(
    dominant: &BinaryWordPeak,
    secondary: &BinaryWordPeak,
    fallback_shift: u8,
) -> u8 {
    let delta = ((secondary.bit as i16 - dominant.bit as i16).rem_euclid(64) as u8) % 31;
    delta.max(1).max(fallback_shift.min(31))
}

fn encode_operator_delta(from: u8, to: u8) -> u8 {
    ((to as i16 - from as i16 - 1).rem_euclid(32)) as u8
}

fn quantize_ratio(numerator: usize, denominator: usize, bits: u8) -> u64 {
    if denominator == 0 {
        return 0;
    }
    let max = (1_u64 << bits) - 1;
    ((numerator as u128 * max as u128 + (denominator as u128 / 2)) / denominator as u128) as u64
}

fn parse_operator_runtime_key(key_bytes: &[u8]) -> Result<SpectralOperatorKey, String> {
    let key = MagicKey::parse(key_bytes)?;
    let blueprint = key
        .require_kind(MagicKeyKind::Operator)?
        .operator_blueprint()?;
    let dominant_index = u64::from(blueprint.dominant_index);
    let secondary_index = (dominant_index + u64::from(blueprint.secondary_delta) + 1) & 63;
    let tertiary_index = (dominant_index
        + u64::from(blueprint.tertiary_delta)
        + u64::from(blueprint.primary_shift)
        + 1)
        & 63;
    let seed = dominant_index
        | (u64::from(blueprint.shift_match) << 13)
        | (u64::from(blueprint.derivative_density) << 22)
        | (u64::from(blueprint.popcnt_density) << 30)
        | (u64::from(blueprint.secondary_delta) << 38)
        | (u64::from(blueprint.tertiary_delta) << 43)
        | (u64::from(blueprint.fingerprint_bias) << 48)
        | (u64::from(blueprint.dominant_positive) << 53);
    let mask = repeat_byte(blueprint.popcnt_density)
        ^ repeat_byte(blueprint.derivative_density)
            .rotate_left((blueprint.shift_match as u32) & 63)
        ^ (1_u64 << secondary_index)
        ^ (1_u64 << tertiary_index);
    let shift_left = ((u16::from(blueprint.primary_shift) + u16::from(blueprint.secondary_delta))
        % 31
        + 1) as u8;
    let shift_right = ((u16::from(blueprint.primary_shift)
        + u16::from(blueprint.tertiary_delta)
        + u16::from(blueprint.fingerprint_bias))
        % 31
        + 1) as u8;
    let odd_multiplier = (repeat_byte(blueprint.derivative_density)
        ^ repeat_byte(blueprint.popcnt_density).rotate_left(u32::from(blueprint.primary_shift))
        ^ (u64::from(blueprint.fingerprint_bias) << 56)
        ^ (u64::from(blueprint.shift_match) << 17))
        | 1;
    Ok(SpectralOperatorKey {
        mix: IndexMix {
            xor_mask: (mask
                ^ repeat_byte(blueprint.fingerprint_bias)
                    .rotate_right(u32::from(blueprint.primary_shift)))
                as usize,
            rotate: ((u16::from(blueprint.primary_shift)
                + u16::from(blueprint.secondary_delta)
                + u16::from(blueprint.fingerprint_bias))
                % 63
                + 1) as u8,
        },
        phase: PhaseKey {
            seed,
            mask,
            shift_left,
            shift_right,
            odd_multiplier,
        },
    })
}

#[cfg(test)]
fn apply_operator_word_u_k(value: u64, key_bytes: &[u8]) -> Result<u64, String> {
    let operator = parse_operator_runtime_key(key_bytes)?;
    apply_operator_word_u_k_with_runtime(value, &operator)
}

fn apply_operator_word_u_k_with_runtime(
    value: u64,
    operator: &SpectralOperatorKey,
) -> Result<u64, String> {
    let phase = operator.phase.seed
        ^ operator.phase.mask.rotate_left(17)
        ^ operator.phase.odd_multiplier.rotate_right(11)
        ^ (operator.phase.shift_left as u64)
        ^ ((operator.phase.shift_right as u64) << 8);
    Ok(apply_binary_u_k(
        value,
        BinaryOperatorKey {
            xor_mask: operator.mix.xor_mask as u64,
            rotate: operator.mix.rotate,
            phase_mask: phase,
        },
    ))
}

fn invert_operator_word_u_k_with_runtime(
    value: u64,
    operator: &SpectralOperatorKey,
) -> Result<u64, String> {
    apply_operator_word_u_k_with_runtime(value, operator)
}

fn repeat_byte(byte: u8) -> u64 {
    u64::from_le_bytes([byte; 8])
}

fn try_encode_spectral_block(
    block: &[u8],
    cached_topology: Option<&TopologySignature>,
) -> Result<Option<BlockEncoding>, String> {
    let bit_len = block.len() * 8;
    if bit_len < 64 || !bit_len.is_power_of_two() || bit_len > 8192 {
        return Ok(None);
    }
    let signature = match cached_topology {
        Some(sig) => sig.clone(),
        None => analyze_topology(block)?,
    };
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
    let mut sparse = Vec::new();
    let mut dense = vec![0_u8; actual.len()];
    let mut previous = None;
    for bit_index in 0..actual.len() * 8 {
        let actual_bit = (actual[bit_index / 8] >> (bit_index % 8)) & 1;
        let predicted_bit = (predictor[bit_index / 8] >> (bit_index % 8)) & 1;
        if actual_bit != predicted_bit {
            dense[bit_index / 8] ^= 1 << (bit_index % 8);
            let delta = match previous {
                None => bit_index + 1,
                Some(last) => bit_index - last,
            };
            encode_uleb128(delta as u64, &mut sparse);
            previous = Some(bit_index);
        }
    }
    let dense_encoded_len = if dense.iter().any(|byte| *byte != 0) {
        1 + dense.len()
    } else {
        usize::MAX
    };
    if dense_encoded_len < sparse.len() {
        let mut out = Vec::with_capacity(dense_encoded_len);
        out.push(0);
        out.extend_from_slice(&dense);
        Ok(out)
    } else {
        Ok(sparse)
    }
}

fn apply_spectral_exceptions(
    predictor: &[u8],
    residual: &[u8],
    bit_len: usize,
) -> Result<Vec<u8>, String> {
    if residual.first() == Some(&0) {
        let dense = &residual[1..];
        if dense.len() != predictor.len() {
            return Err("dense spectral exception bitmap has non-canonical length".to_string());
        }
        let mut out = predictor.to_vec();
        for (byte, mask) in out.iter_mut().zip(dense) {
            *byte ^= *mask;
        }
        return Ok(out);
    }

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

fn encode_position_deltas(positions: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous = None;
    for position in positions {
        let delta = match previous {
            None => position + 1,
            Some(last) => position - last,
        };
        encode_uleb128(delta as u64, &mut out);
        previous = Some(*position);
    }
    out
}

fn decode_position_deltas(bytes: &[u8]) -> Result<Vec<usize>, String> {
    let mut cursor = 0_usize;
    let mut previous = None;
    let mut positions = Vec::new();
    while cursor < bytes.len() {
        let delta = decode_uleb128(bytes, &mut cursor)?;
        if delta == 0 {
            return Err("position delta must be positive".to_string());
        }
        let position = match previous {
            None => delta as usize - 1,
            Some(last) => last + delta as usize,
        };
        positions.push(position);
        previous = Some(position);
    }
    Ok(positions)
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
    let mut out = Vec::new();
    append_serialized_block(&mut out, block);
    out.len()
}

fn serialize_single_layer_archive(archive: &Archive) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC_SINGLE);
    out.push(archive.header.base_version);
    push_u32(&mut out, archive.header.block_size_bytes);
    push_u64(&mut out, archive.header.original_size);
    push_u32(&mut out, archive.header.block_count);

    for block in &archive.blocks {
        append_serialized_block(&mut out, block);
    }

    Ok(out)
}

fn append_serialized_block(out: &mut Vec<u8>, block: &BlockEncoding) {
    match block {
        BlockEncoding::Raw(raw) => {
            out.push(block.mode() as u8);
            push_u32(out, raw.original_len);
            push_u32(out, raw.payload.len() as u32);
            out.extend_from_slice(&raw.payload);
        }
        BlockEncoding::Alphabet(alpha) => {
            out.push(block.mode() as u8);
            push_u32(out, alpha.original_len);
            out.push(alpha.alphabet.len() as u8);
            out.push(alpha.bit_width);
            out.extend_from_slice(&alpha.alphabet);
            push_u32(out, alpha.breadcrumbs.len() as u32);
            out.extend_from_slice(&alpha.breadcrumbs);
        }
        BlockEncoding::SparseAlphabet(alpha) => {
            out.push(block.mode() as u8);
            push_u32(out, alpha.original_len);
            out.push(alpha.dense_alphabet.len() as u8);
            out.push(alpha.dense_bit_width);
            out.extend_from_slice(&alpha.dense_alphabet);
            push_u32(out, alpha.dense_breadcrumbs.len() as u32);
            out.extend_from_slice(&alpha.dense_breadcrumbs);
            out.push(alpha.exception_alphabet.len() as u8);
            out.extend_from_slice(&alpha.exception_alphabet);
            push_u32(out, alpha.exception_indices.len() as u32);
            out.extend_from_slice(&alpha.exception_indices);
            push_u32(out, alpha.exception_positions.len() as u32);
            out.extend_from_slice(&alpha.exception_positions);
        }
        BlockEncoding::Spectral(spectral) => {
            out.push(block.mode() as u8);
            push_u32(out, spectral.original_len);
            push_u16(out, spectral.key.len() as u16);
            out.extend_from_slice(&spectral.key);
            push_u32(out, spectral.residual.len() as u32);
            out.extend_from_slice(&spectral.residual);
        }
        BlockEncoding::Trajectory(trajectory) => {
            out.push(block.mode() as u8);
            push_u32(out, trajectory.original_len);
            push_u16(out, trajectory.key.len() as u16);
            out.extend_from_slice(&trajectory.key);
            push_u16(out, trajectory.terminals.len() as u16);
            for terminal in &trajectory.terminals {
                push_u64(out, *terminal);
            }
            push_u32(out, trajectory.terminal_indices.len() as u32);
            out.extend_from_slice(&trajectory.terminal_indices);
            out.push(trajectory.steps);
            push_u32(out, trajectory.breadcrumbs.len() as u32);
            out.extend_from_slice(&trajectory.breadcrumbs);
        }
        BlockEncoding::Operator(operator) => {
            out.push(block.mode() as u8);
            push_u32(out, operator.original_len);
            push_u16(out, operator.key.len() as u16);
            out.extend_from_slice(&operator.key);
            push_u16(out, operator.terminals.len() as u16);
            for terminal in &operator.terminals {
                push_u64(out, *terminal);
            }
            push_u32(out, operator.terminal_indices.len() as u32);
            out.extend_from_slice(&operator.terminal_indices);
            out.push(operator.steps);
            push_u32(out, operator.breadcrumbs.len() as u32);
            out.extend_from_slice(&operator.breadcrumbs);
        }
    }
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
            5 => {
                let dense_alphabet_len = read_u8(input, &mut cursor)? as usize;
                let dense_bit_width = read_u8(input, &mut cursor)?;
                let dense_alphabet = read_vec(input, &mut cursor, dense_alphabet_len)?;
                validate_alphabet(&dense_alphabet, dense_bit_width)?;
                let dense_breadcrumbs_len = read_u32(input, &mut cursor)? as usize;
                let dense_breadcrumbs = read_vec(input, &mut cursor, dense_breadcrumbs_len)?;
                let exception_alphabet_len = read_u8(input, &mut cursor)? as usize;
                let exception_alphabet = read_vec(input, &mut cursor, exception_alphabet_len)?;
                validate_alphabet(
                    &exception_alphabet,
                    bit_width_for_cardinality(exception_alphabet.len()),
                )?;
                let exception_indices_len = read_u32(input, &mut cursor)? as usize;
                let exception_indices = read_vec(input, &mut cursor, exception_indices_len)?;
                let exception_positions_len = read_u32(input, &mut cursor)? as usize;
                let exception_positions = read_vec(input, &mut cursor, exception_positions_len)?;
                let positions = decode_position_deltas(&exception_positions)?;
                if positions
                    .iter()
                    .any(|position| *position >= original_len as usize)
                {
                    return Err(
                        "sparse alphabet exception position is outside the block".to_string()
                    );
                }
                let exception_count = positions.len();
                let dense_count = (original_len as usize)
                    .checked_sub(exception_count)
                    .ok_or_else(|| "sparse alphabet dense count underflow".to_string())?;
                let expected_dense_bytes =
                    ceil_div_u64(dense_count as u64 * dense_bit_width as u64, 8) as usize;
                if dense_breadcrumbs.len() != expected_dense_bytes {
                    return Err(
                        "sparse alphabet dense breadcrumb payload length is not canonical"
                            .to_string(),
                    );
                }
                let expected_exception_index_bytes = ceil_div_u64(
                    exception_count as u64
                        * bit_width_for_cardinality(exception_alphabet.len()) as u64,
                    8,
                ) as usize;
                if exception_indices.len() != expected_exception_index_bytes {
                    return Err(
                        "sparse alphabet exception index payload length is not canonical"
                            .to_string(),
                    );
                }
                blocks.push(BlockEncoding::SparseAlphabet(SparseAlphabetBlock {
                    original_len,
                    dense_alphabet,
                    dense_bit_width,
                    dense_breadcrumbs,
                    exception_alphabet,
                    exception_indices,
                    exception_positions,
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
                let terminal_count = read_u16(input, &mut cursor)? as usize;
                if terminal_count == 0 {
                    return Err("trajectory terminal palette must not be empty".to_string());
                }
                let terminals = (0..terminal_count)
                    .map(|_| read_u64(input, &mut cursor))
                    .collect::<Result<Vec<_>, _>>()?;
                let terminal_indices_len = read_u32(input, &mut cursor)? as usize;
                let terminal_indices = read_vec(input, &mut cursor, terminal_indices_len)?;
                let steps = read_u8(input, &mut cursor)?;
                if !(1..64).contains(&steps) || parsed.trajectory_steps()? != steps {
                    return Err("trajectory K does not match a valid step count".to_string());
                }
                let breadcrumbs_len = read_u32(input, &mut cursor)? as usize;
                let breadcrumbs = read_vec(input, &mut cursor, breadcrumbs_len)?;
                let word_count = original_len as usize / 8;
                let expected_terminal_index_bytes = ceil_div_u64(
                    word_count as u64 * bit_width_for_cardinality(terminals.len()) as u64,
                    8,
                ) as usize;
                if original_len as usize % 8 != 0
                    || terminal_indices.len() != expected_terminal_index_bytes
                {
                    return Err(
                        "trajectory terminal index payload length is not canonical".to_string()
                    );
                }
                let expected_bits = word_count
                    .checked_mul(steps as usize)
                    .ok_or_else(|| "trajectory breadcrumb length overflow".to_string())?;
                if breadcrumbs.len() != ceil_div_u64(expected_bits as u64, 8) as usize {
                    return Err(
                        "trajectory parity breadcrumb payload length is not canonical".to_string(),
                    );
                }
                blocks.push(BlockEncoding::Trajectory(TrajectoryBlock {
                    original_len,
                    key,
                    terminals,
                    terminal_indices,
                    steps,
                    breadcrumbs,
                }));
            }
            4 => {
                let key_len = read_u16(input, &mut cursor)? as usize;
                let key = read_vec(input, &mut cursor, key_len)?;
                MagicKey::parse(&key)?.require_kind(MagicKeyKind::Operator)?;
                let terminal_count = read_u16(input, &mut cursor)? as usize;
                if terminal_count == 0 {
                    return Err("operator terminal palette must not be empty".to_string());
                }
                let terminals = (0..terminal_count)
                    .map(|_| read_u64(input, &mut cursor))
                    .collect::<Result<Vec<_>, _>>()?;
                let terminal_indices_len = read_u32(input, &mut cursor)? as usize;
                let terminal_indices = read_vec(input, &mut cursor, terminal_indices_len)?;
                let steps = read_u8(input, &mut cursor)?;
                if !(1..64).contains(&steps) {
                    return Err("operator parity step count must be in 1..64".to_string());
                }
                if original_len as usize % 8 != 0 {
                    return Err("operator block length must be divisible by 8".to_string());
                }
                let breadcrumbs_len = read_u32(input, &mut cursor)? as usize;
                let breadcrumbs = read_vec(input, &mut cursor, breadcrumbs_len)?;
                let word_count = original_len as usize / 8;
                let expected_terminal_index_bytes = ceil_div_u64(
                    word_count as u64 * bit_width_for_cardinality(terminals.len()) as u64,
                    8,
                ) as usize;
                if terminal_indices.len() != expected_terminal_index_bytes {
                    return Err(
                        "operator terminal index payload length is not canonical".to_string()
                    );
                }
                let expected_bits = word_count
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
                    terminals,
                    terminal_indices,
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
        encoded_size_of, grouped_trajectory_score, inspect_archive, parse_archive,
        score_operator_key, select_operator_codec_key_from_family, serialize_single_layer_archive,
        synthesise_spectral_bytes, try_encode_operator_block, Archive, ArchiveHeader,
        BlockEncoding, MagicKey, OperatorBlock, RawBlock, SpectralBlock, DEFAULT_BLOCK_SIZE_BYTES,
        MAGIC_MULTI,
    };
    use crate::domain::kernel::operator::strongest_binary_word_peaks;
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
    fn sparse_alphabet_factors_rare_symbols_out_of_a_dense_core() {
        let core = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".repeat(32);
        let mut input = Vec::new();
        for chunk in core.chunks(64) {
            input.extend_from_slice(chunk);
            input.push(b'\n');
        }
        let analysis = super::analyze_block(&input);
        let sparse = match super::try_encode_sparse_alphabet_block(&input, &analysis).unwrap() {
            Some(BlockEncoding::SparseAlphabet(block)) => block,
            other => panic!("expected sparse alphabet block, got {other:?}"),
        };
        let plain = match super::try_encode_alphabet_block(&input, &analysis).unwrap() {
            Some(block) => block,
            None => panic!("expected plain alphabet candidate"),
        };
        assert!(sparse.exception_alphabet.contains(&b'\n'));
        assert_eq!(super::decode_sparse_alphabet_block(&sparse).unwrap(), input);
        assert!(encoded_size_of(&BlockEncoding::SparseAlphabet(sparse.clone())) < input.len());
        assert!(
            encoded_size_of(&BlockEncoding::SparseAlphabet(sparse.clone()))
                < encoded_size_of(&plain)
        );
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
    fn inspect_archive_reports_real_input_size_for_single_layer_archives() {
        let input = b"0123456789".repeat(200);
        let packed = compress_single_layer_bytes(&input, 256).unwrap();
        let layers = inspect_archive(&packed).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].input_size, input.len() as u64);
        assert_eq!(layers[0].output_size, packed.len() as u64);
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
    fn dense_spectral_residual_is_used_when_sparse_deltas_are_worse() {
        let predictor = vec![0_u8; 64];
        let actual = vec![0xFF_u8; 64];
        let residual = super::encode_spectral_exceptions(&actual, &predictor).unwrap();
        assert_eq!(residual.len(), 1 + actual.len());
        assert_eq!(residual[0], 0);
        assert_eq!(
            super::apply_spectral_exceptions(&predictor, &residual, actual.len() * 8).unwrap(),
            actual
        );
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
    fn operator_codec_runtime_is_direct_binary_u_k() {
        let source = [0x3C_u8; 64];
        let key = build_operator_codec_key(&source).unwrap();
        let runtime = super::parse_operator_runtime_key(&key.serialize()).unwrap();
        let phase = runtime.phase.seed
            ^ runtime.phase.mask.rotate_left(17)
            ^ runtime.phase.odd_multiplier.rotate_right(11)
            ^ (runtime.phase.shift_left as u64)
            ^ ((runtime.phase.shift_right as u64) << 8);
        let expected = apply_binary_u_k(
            0x0123_4567_89AB_CDEF,
            BinaryOperatorKey {
                xor_mask: runtime.mix.xor_mask as u64,
                rotate: runtime.mix.rotate,
                phase_mask: phase,
            },
        );
        assert_eq!(
            super::apply_operator_word_u_k(0x0123_4567_89AB_CDEF, &key.serialize()).unwrap(),
            expected
        );
    }

    #[test]
    fn parity_step_search_covers_the_full_runtime_domain() {
        assert_eq!(
            super::candidate_steps().collect::<Vec<_>>(),
            (1_u8..64).collect::<Vec<_>>()
        );
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
                assert!(super::is_better_trajectory_score(
                    grouped_trajectory_score(&transformed).unwrap(),
                    grouped_trajectory_score(&original_words).unwrap()
                ));
            }
        }
    }

    #[test]
    fn operator_family_selection_never_returns_a_key_worse_than_the_structural_seed() {
        let input = (0..64_u64)
            .flat_map(|value| {
                value
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    .rotate_left((value % 17) as u32)
                    .to_le_bytes()
            })
            .collect::<Vec<_>>();
        let words = input
            .chunks_exact(8)
            .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
            .collect::<Vec<_>>();
        let seed = build_operator_codec_key(&input).unwrap();
        let seed_score = score_operator_key(&words, seed).unwrap();
        if let Some(found) = select_operator_codec_key_from_family(&input, &words).unwrap() {
            let found_score = score_operator_key(&words, found).unwrap();
            assert!(
                super::is_better_operator_score(found_score, seed_score)
                    || found_score == seed_score
            );
        }
    }

    #[test]
    fn operator_family_contains_a_blueprint_built_from_binary_runtime_peaks() {
        let words = [
            0x0000_0000_0000_0000_u64,
            0x0000_0000_0000_0000_u64,
            0xFFFF_FFFF_FFFF_FFFF_u64,
            0xFFFF_FFFF_FFFF_FFFF_u64,
            0x0000_0000_0000_0000_u64,
            0xFFFF_FFFF_FFFF_FFFF_u64,
            0x0000_0000_0000_0000_u64,
            0xFFFF_FFFF_FFFF_FFFF_u64,
        ];
        let input = words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect::<Vec<_>>();
        let signature = analyze_topology(&input).unwrap();
        let binary_peak = strongest_binary_word_peaks(&words, 1).unwrap()[0];
        let family = super::operator_candidate_family(&signature, &words).unwrap();
        assert!(family.iter().any(|key| {
            let blueprint = key.operator_blueprint().unwrap();
            blueprint.dominant_index == binary_peak.bit as u16
                && blueprint.dominant_positive == binary_peak.positive
        }));
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
    fn operator_wire_format_roundtrips_terminal_palette_steps_and_parity() {
        let source = [0x3C_u8; 64];
        let key = build_operator_codec_key(&source).unwrap();
        let values = [0x0123_4567_89AB_CDEF_u64, 0x1123_4567_89AB_CDEE_u64];
        let transformed = values
            .iter()
            .map(|value| apply_operator_word_u_k(*value, &key.serialize()).unwrap())
            .collect::<Vec<_>>();
        let left = crate::domain::kernel::trajectory::encode_parity_trajectory(transformed[0], 63)
            .unwrap();
        let right = crate::domain::kernel::trajectory::encode_parity_trajectory(transformed[1], 63)
            .unwrap();
        let block = OperatorBlock {
            original_len: 16,
            key: key.serialize().to_vec(),
            terminals: vec![left.terminal, right.terminal],
            terminal_indices: crate::domain::bitstream::pack_indices(&[0, 1], 1),
            steps: 63,
            breadcrumbs: super::pack_bools(&[left.crumbs.clone(), right.crumbs.clone()].concat()),
        };
        let expected = values
            .into_iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(super::decode_operator_block(&block).unwrap(), expected);
    }

    #[test]
    fn encoded_size_of_matches_the_single_block_wire_format() {
        let input = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz+/".repeat(8);
        for block in [
            BlockEncoding::Raw(RawBlock {
                original_len: input.len() as u32,
                payload: input.clone(),
            }),
            encode_block(&input).unwrap(),
        ] {
            let archive = Archive {
                header: ArchiveHeader {
                    base_version: 1,
                    block_size_bytes: input.len() as u32,
                    original_size: input.len() as u64,
                    block_count: 1,
                },
                blocks: vec![block.clone()],
            };
            let wire = serialize_single_layer_archive(&archive).unwrap();
            let header_len = 8 + 1 + 4 + 8 + 4;
            assert_eq!(encoded_size_of(&block), wire.len() - header_len);
        }
    }
    #[test]
    fn compress_decompress_roundtrip_sequential() {
        let input: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        let compressed = compress_bytes(&input, None).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn compress_decompress_roundtrip_pseudorandom() {
        // Детерминированный LCG — никаких внешних зависимостей
        let mut state: u64 = 0xDEAD_BEEF_CAFE_1337;
        let input: Vec<u8> = (0..2048)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let compressed = compress_bytes(&input, None).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn uniform_input_compresses_to_smaller_archive() {
        let input = vec![0x42u8; 512];
        let compressed = compress_bytes(&input, None).unwrap();
        assert!(
            compressed.len() < input.len(),
            "uniform input should compress: compressed={} >= original={}",
            compressed.len(),
            input.len()
        );
    }

    #[test]
    fn inspect_archive_single_layer_reports_correct_input_size() {
        let input: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        let compressed = compress_bytes(&input, None).unwrap();
        let layers = inspect_archive(&compressed).unwrap();
        // Both single-layer and multi-layer paths: last layer's output_size = compressed len,
        // and the chain must reconstruct original_size.
        let reconstructed: u64 = layers[0].input_size;
        assert_eq!(reconstructed, input.len() as u64);
    }

    #[test]
    fn operator_transform_is_involution() {
        // apply дважды должен вернуть исходное значение (функция инволютивна)
        let block: Vec<u8> = (0u8..64).map(|i| i.wrapping_mul(7) ^ 0xAB).collect();
        let key = build_operator_codec_key(&block).unwrap();
        let original: u64 = 0x0123_4567_89AB_CDEF;
        let transformed = apply_operator_word_u_k(original, &key.serialize()).unwrap();
        let restored = apply_operator_word_u_k(transformed, &key.serialize()).unwrap();
        assert_eq!(
            restored, original,
            "operator must be an involution: apply(apply(x)) == x"
        );
    }

    #[test]
    fn compress_decompress_roundtrip_all_zeros() {
        let input = vec![0u8; 2048];
        let compressed = compress_bytes(&input, None).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn compress_decompress_roundtrip_all_255() {
        let input = vec![255u8; 2048];
        let compressed = compress_bytes(&input, None).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn compress_decompress_roundtrip_single_byte() {
        let input = vec![0xABu8];
        let compressed = compress_bytes(&input, None).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn compress_decompress_roundtrip_empty() {
        let input: Vec<u8> = vec![];
        let compressed = compress_bytes(&input, None).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }
}
