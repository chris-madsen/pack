use std::collections::{BTreeMap, BTreeSet};

use crate::domain::bitstream::{
    bit_width_for_cardinality, ceil_div_u64, pack_indices, unpack_indices,
};
use crate::domain::kernel::budget::{BitBudget, CompressionPolicy};
use crate::domain::kernel::key::MagicKey;
use crate::domain::kernel::operator::{
    apply_strict_operator_inverse_step, apply_strict_operator_step,
};
use crate::domain::kernel::spectral::synthesise_spectral_bits;
use crate::domain::kernel::topology::{
    analyze_topology, block_fingerprint, compile_spectral_key, compile_strict_operator_key,
    TopologySignature,
};
use crate::domain::kernel::trajectory::{
    decode_parity_trajectory, encode_parity_trajectory, ParityTrajectory,
};
use crate::domain::model::{
    AlphabetBlock, Archive, ArchiveHeader, BlockAnalysis, BlockEncoding, BlockRecord, LayerSummary,
    OperatorBlock, OperatorTerminalMode, PassWindowBand, RawBlock, SparseAlphabetBlock,
    SpectralBlock, TrajectoryBlock,
};

const MAGIC_SINGLE: &[u8; 8] = b"PACKMVP1";
const MAGIC_MULTI: &[u8; 8] = b"PACKREC1";
const BASE_VERSION: u8 = 1;
const MAX_LAYERS: usize = 16;
const MAX_DECODER_STEPS: usize = 1_000_000;
const RECURSIVE_HEADER_BYTES: usize = 27;
const LAYER_SUMMARY_BYTES: usize = 18;
const MIN_WINDOW_BYTES: usize = 16;
const MAX_WINDOW_BYTES: usize = 2048;
const MAX_STRICT_WINDOW_BYTES: usize = 16384;
const MAX_STRICT_OPERATOR_STEPS: u8 = 64;
const STRICT_GENERATOR_GAIN_FRACTION: f64 = 0.9;
/// Byte length of the PACKMVP1 single-layer archive header:
/// magic(8) + version(1) + window_min_exp(1) + window_max_exp(1)
/// + original_size(8) + block_count(4) = 23 bytes.
pub(crate) const SINGLE_LAYER_HEADER_BYTES: usize = 23;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompressionProfile {
    General,
    StrictGeneratorOnly,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ProfilePolicy {
    use_alphabet_fallback: bool,
    first_layer_max_ratio: Option<f64>,
    recursive_min_gain_fraction: f64,
}

impl CompressionProfile {
    fn policy(self) -> ProfilePolicy {
        match self {
            Self::General => ProfilePolicy {
                use_alphabet_fallback: true,
                first_layer_max_ratio: None,
                recursive_min_gain_fraction: 0.0,
            },
            Self::StrictGeneratorOnly => ProfilePolicy {
                use_alphabet_fallback: false,
                first_layer_max_ratio: Some(0.1),
                recursive_min_gain_fraction: 0.1,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompressionOutcome {
    pub archive: Vec<u8>,
    pub layer_summaries: Vec<LayerSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratorDebugBlock {
    pub offset: usize,
    pub window_bytes: usize,
    pub key_hex: String,
    pub steps: Option<u8>,
    pub terminal_mode: Option<OperatorTerminalMode>,
    pub key_bits: u64,
    pub crumb_bits: u64,
    pub overhead_bits: u64,
    pub total_bits: u64,
    pub accepted: bool,
    pub reject_reason: String,
}

pub fn compress_bytes(input: &[u8], block_size_hint: Option<usize>) -> Result<Vec<u8>, String> {
    compress_bytes_with_profile(input, block_size_hint, CompressionProfile::General)
        .map(|outcome| outcome.archive)
}

pub fn compress_bytes_with_outcome(
    input: &[u8],
    block_size_hint: Option<usize>,
) -> Result<CompressionOutcome, String> {
    compress_bytes_with_profile(input, block_size_hint, CompressionProfile::General)
}

pub fn compress_bytes_generator_only_strict(
    input: &[u8],
    block_size_hint: Option<usize>,
) -> Result<CompressionOutcome, String> {
    compress_bytes_with_profile(
        input,
        block_size_hint,
        CompressionProfile::StrictGeneratorOnly,
    )
}

pub fn debug_generator_only_strict(
    input: &[u8],
    block_size_hint: Option<usize>,
) -> Result<Vec<GeneratorDebugBlock>, String> {
    let band = analyze_pass_band(
        input,
        block_size_hint,
        0,
        CompressionProfile::StrictGeneratorOnly,
    )?;
    let sizes = window_sizes_for_band(band)?;
    let mut reports = Vec::new();
    let mut offset = 0_usize;
    while offset < input.len() {
        let remaining = &input[offset..];
        let fitting_sizes = sizes
            .iter()
            .copied()
            .filter(|size| remaining.len() >= *size)
            .collect::<Vec<_>>();
        if fitting_sizes.is_empty() {
            reports.push(GeneratorDebugBlock {
                offset,
                window_bytes: remaining.len(),
                key_hex: String::new(),
                steps: None,
                terminal_mode: None,
                key_bits: 0,
                crumb_bits: (remaining.len() * 8) as u64,
                overhead_bits: 0,
                total_bits: (remaining.len() * 8) as u64,
                accepted: false,
                reject_reason: "tail-smaller-than-min-window".to_string(),
            });
            break;
        }
        for size in &fitting_sizes {
            reports.push(debug_strict_operator_block(&remaining[..*size], offset)?);
        }
        let (_, _, block_size) = select_strict_generator_block(remaining, &sizes)?;
        offset += block_size;
    }
    Ok(reports)
}

fn compress_bytes_with_profile(
    input: &[u8],
    block_size_hint: Option<usize>,
    profile: CompressionProfile,
) -> Result<CompressionOutcome, String> {
    let policy = profile.policy();
    let mut current = input.to_vec();
    let mut layers = Vec::new();
    let mut best_archive = serialize_recursive_archive(input.len() as u64, &layers, &current);

    let mut first_layer_attempted = false;
    for layer_index in 0..MAX_LAYERS {
        let band = analyze_pass_band(&current, block_size_hint, layer_index, profile)?;
        let best = compress_single_layer_bytes_with_profile(&current, band, profile)?;
        if layer_index == 0 {
            first_layer_attempted = true;
        }
        if best.output.len() >= current.len() {
            if layer_index == 0 && policy.first_layer_max_ratio.is_some() {
                return Err(
                    "InsufficientFirstLayerGain: no acceptable first compressed layer was produced"
                        .to_string(),
                );
            }
            break;
        }

        let next_summary = LayerSummary {
            window_min_bits: band.min_window_bits,
            window_max_bits: band.max_window_bits,
            input_size: current.len() as u64,
            output_size: best.output.len() as u64,
        };
        let mut tentative_layers = layers.clone();
        tentative_layers.push(next_summary);
        let tentative_archive =
            serialize_recursive_archive(input.len() as u64, &tentative_layers, &best.output);

        if layer_index == 0 {
            if let Some(max_ratio) = policy.first_layer_max_ratio {
                if input.is_empty()
                    || tentative_archive.len() as f64 / input.len() as f64 > max_ratio
                {
                    return Err(format!(
                        "InsufficientFirstLayerGain: required at least {:.1}x, got {:.4}x",
                        1.0 / max_ratio,
                        if tentative_archive.is_empty() {
                            f64::INFINITY
                        } else {
                            input.len() as f64 / tentative_archive.len() as f64
                        }
                    ));
                }
            }
        }

        let required_next_len = ((best_archive.len() as f64)
            * (1.0 - policy.recursive_min_gain_fraction))
            .floor() as usize;
        if tentative_archive.len() >= best_archive.len()
            || (policy.recursive_min_gain_fraction > 0.0
                && tentative_archive.len() > required_next_len)
        {
            break;
        }

        layers = tentative_layers;
        current = best.output;
        best_archive = tentative_archive;
    }

    if first_layer_attempted && layers.is_empty() && policy.first_layer_max_ratio.is_some() {
        return Err(
            "InsufficientFirstLayerGain: first layer did not satisfy the strict contract"
                .to_string(),
        );
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
        // output_size is the compressed payload size (archive blob minus the
        // fixed 23-byte PACKMVP1 header).  This is consistent with the
        // multi-layer format where output_size tracks the compressed data
        // bytes, not the full framed archive.
        let payload_size = input.len().saturating_sub(SINGLE_LAYER_HEADER_BYTES) as u64;
        return Ok(vec![LayerSummary {
            window_min_bits: 1_u32 << archive.header.window_min_exp,
            window_max_bits: 1_u32 << archive.header.window_max_exp,
            input_size: archive.header.original_size,
            output_size: payload_size,
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

fn analyze_pass_band(
    input: &[u8],
    block_size_hint: Option<usize>,
    layer_index: usize,
    profile: CompressionProfile,
) -> Result<PassWindowBand, String> {
    let available = candidate_window_sizes(input.len(), profile);
    let hint =
        block_size_hint.filter(|value| value.is_power_of_two() && *value >= MIN_WINDOW_BYTES);
    if available.is_empty() {
        let bits = (input.len().max(1) * 8) as u32;
        return Ok(PassWindowBand {
            min_window_bits: bits,
            max_window_bits: bits,
        });
    }

    let scored = available
        .iter()
        .copied()
        .filter_map(|size| {
            let prefix = input.get(..size)?;
            let signature = analyze_topology(prefix).ok()?;
            Some((size, topology_score(&signature)))
        })
        .collect::<Vec<_>>();
    if scored.is_empty() {
        let fallback = hint.unwrap_or(*available.first().unwrap());
        return Ok(PassWindowBand {
            min_window_bits: (fallback * 8) as u32,
            max_window_bits: (fallback * 8) as u32,
        });
    }

    let best_score = scored
        .iter()
        .map(|(_, score)| *score)
        .fold(f64::NEG_INFINITY, f64::max);
    let threshold = best_score * 0.85;
    let mut selected = scored
        .iter()
        .filter(|(_, score)| *score >= threshold)
        .map(|(size, _)| *size)
        .collect::<Vec<_>>();
    selected.sort_unstable();
    selected.dedup();

    let mut min_bytes = *selected.first().unwrap_or(&scored[0].0);
    let mut max_bytes = *selected.last().unwrap_or(&scored[0].0);
    if let Some(anchor) = hint {
        min_bytes = min_bytes.min(anchor);
        max_bytes = max_bytes.max(anchor.min(*available.last().unwrap()));
    }
    if layer_index > 0 {
        max_bytes = max_bytes.min(128);
        min_bytes = min_bytes.min(max_bytes);
    }
    Ok(PassWindowBand {
        min_window_bits: (min_bytes * 8) as u32,
        max_window_bits: (max_bytes * 8) as u32,
    })
}

fn candidate_window_sizes(input_len: usize, profile: CompressionProfile) -> Vec<usize> {
    let cap = match profile {
        CompressionProfile::General => MAX_WINDOW_BYTES,
        CompressionProfile::StrictGeneratorOnly => MAX_STRICT_WINDOW_BYTES,
    };
    let capped = input_len.min(cap);
    let mut size = MIN_WINDOW_BYTES;
    let mut out = Vec::new();
    while size <= capped {
        out.push(size);
        size *= 2;
    }
    out
}

fn window_sizes_for_band(band: PassWindowBand) -> Result<Vec<usize>, String> {
    if band.min_window_bits == 0
        || band.max_window_bits == 0
        || band.min_window_bits > band.max_window_bits
        || !band.min_window_bits.is_power_of_two()
        || !band.max_window_bits.is_power_of_two()
    {
        return Err("pass window band must be a non-empty power-of-two range".to_string());
    }
    let mut out = Vec::new();
    let mut size = (band.min_window_bits / 8) as usize;
    let max = (band.max_window_bits / 8) as usize;
    while size <= max {
        out.push(size);
        size *= 2;
    }
    Ok(out)
}

fn select_block_window(input: &[u8], sizes: &[usize]) -> Result<(u8, usize), String> {
    let mut best: Option<(u8, usize, f64)> = None;
    for (index, &size) in sizes.iter().enumerate() {
        let Some(block) = input.get(..size) else {
            continue;
        };
        let score = match analyze_topology(block) {
            Ok(signature) => topology_score(&signature),
            Err(_) => continue,
        };
        let candidate = (index as u8, size, score);
        let should_replace = best
            .as_ref()
            .map(|(_, best_size, best_score)| {
                candidate.2 > *best_score
                    || (candidate.2 == *best_score && candidate.1 > *best_size)
            })
            .unwrap_or(true);
        if should_replace {
            best = Some(candidate);
        }
    }
    if let Some((index, size, _)) = best {
        return Ok((index, size));
    }
    Ok((
        0,
        input
            .len()
            .min(*sizes.first().unwrap_or(&input.len()))
            .max(1),
    ))
}

/// Score a topology signature for window-size selection.
/// Returns 0.0 for degenerate inputs (empty walsh_peaks) rather than panicking.
fn topology_score(signature: &TopologySignature) -> f64 {
    let Some(peak0) = signature.walsh_peaks.first() else {
        return 0.0;
    };
    let peak_sum = signature
        .walsh_peaks
        .iter()
        .map(|peak| peak.coefficient.unsigned_abs() as f64)
        .sum::<f64>()
        .max(1e-12);
    let prominence = peak0.coefficient.unsigned_abs() as f64 / peak_sum;
    let derivative_density = signature.derivative.iter().filter(|bit| **bit == 1).count() as f64
        / signature.derivative.len().max(1) as f64;
    let shift_coherence = signature
        .shift_scores
        .first()
        .map(|score| score.matching_bits as f64 / signature.bit_len.max(1) as f64)
        .unwrap_or(0.0);
    prominence + shift_coherence - derivative_density * 0.35
}

fn encode_block_with_profile(
    block: &[u8],
    profile: CompressionProfile,
) -> Result<BlockEncoding, String> {
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

    let maybe_operator = try_encode_operator_block(block, cached_topology.as_ref(), profile)?;
    let maybe_spectral = if profile == CompressionProfile::General {
        try_encode_spectral_block(block, cached_topology.as_ref())?
    } else {
        None
    };
    let maybe_trajectory = if profile == CompressionProfile::General {
        try_encode_trajectory_block(block)?
    } else {
        None
    };
    let (maybe_sparse_alphabet, maybe_alphabet) =
        if profile == CompressionProfile::General && profile.policy().use_alphabet_fallback {
            (
                try_encode_sparse_alphabet_block(block, &analysis)?,
                try_encode_alphabet_block(block, &analysis)?,
            )
        } else {
            (None, None)
        };
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
    band: PassWindowBand,
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct OperatorTerminalCandidate {
    mode: OperatorTerminalMode,
    payload: Vec<u8>,
}

#[cfg(test)]
fn compress_single_layer_bytes(input: &[u8], band: PassWindowBand) -> Result<Vec<u8>, String> {
    compress_single_layer_bytes_with_profile(input, band, CompressionProfile::General)
        .map(|candidate| candidate.output)
}

fn compress_single_layer_bytes_with_profile(
    input: &[u8],
    band: PassWindowBand,
    profile: CompressionProfile,
) -> Result<SingleLayerCandidate, String> {
    let block_sizes = window_sizes_for_band(band)?;
    let mut blocks = Vec::new();
    let mut offset = 0_usize;
    while offset < input.len() {
        let (window_index, encoding, block_size) = match profile {
            CompressionProfile::General => {
                let (window_index, block_size) =
                    select_block_window(&input[offset..], &block_sizes)?;
                let block = &input[offset..offset + block_size];
                (
                    window_index,
                    encode_block_with_profile(block, profile)?,
                    block_size,
                )
            }
            CompressionProfile::StrictGeneratorOnly => {
                select_strict_generator_block(&input[offset..], &block_sizes)?
            }
        };
        blocks.push(BlockRecord {
            window_index,
            encoding,
        });
        offset += block_size;
    }

    let archive = Archive {
        header: ArchiveHeader {
            base_version: BASE_VERSION,
            window_min_exp: band.min_window_bits.ilog2() as u8,
            window_max_exp: band.max_window_bits.ilog2() as u8,
            original_size: input.len() as u64,
            block_count: blocks.len() as u32,
        },
        blocks,
    };

    Ok(SingleLayerCandidate {
        band,
        output: serialize_single_layer_archive(&archive)?,
    })
}

fn select_strict_generator_block(
    input: &[u8],
    sizes: &[usize],
) -> Result<(u8, BlockEncoding, usize), String> {
    let mut best: Option<(u8, BlockEncoding, usize)> = None;
    for (index, &size) in sizes.iter().enumerate() {
        let Some(block) = input.get(..size) else {
            continue;
        };
        let encoding = encode_block_with_profile(block, CompressionProfile::StrictGeneratorOnly)?;
        let candidate = (index as u8, encoding, size);
        let should_replace = match best.as_ref() {
            None => true,
            Some((_, best_encoding, best_size)) => {
                let lhs = encoded_size_of(&candidate.1) * *best_size;
                let rhs = encoded_size_of(best_encoding) * candidate.2;
                lhs < rhs
                    || (lhs == rhs
                        && encoded_size_of(&candidate.1) < encoded_size_of(best_encoding))
            }
        };
        if should_replace {
            best = Some(candidate);
        }
    }
    Ok(best.unwrap_or((
        0,
        BlockEncoding::Raw(RawBlock {
            original_len: input.len() as u32,
            payload: input.to_vec(),
        }),
        input.len().max(1),
    )))
}

fn decompress_single_layer_bytes(input: &[u8]) -> Result<Vec<u8>, String> {
    let archive = parse_archive(input)?;
    let mut out = Vec::with_capacity(archive.header.original_size as usize);

    for block in archive.blocks {
        match block.encoding {
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
    // bit_width == 0 means alphabet has exactly one symbol.  pack_indices is
    // safe at width=0 (returns an empty vec) but the encoding carries no
    // information and can never beat a Raw block — skip it explicitly.
    if bit_width == 0 || bit_width >= 8 {
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
        BlockEncoding::Operator(o) => o.terminal_payload.len(),
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
    profile: CompressionProfile,
) -> Result<Option<BlockEncoding>, String> {
    if block.len() < 16 || block.len() % 8 != 0 || !block.len().is_power_of_two() {
        return Ok(None);
    }
    let signature = match cached_topology {
        Some(sig) => sig.clone(),
        None => analyze_topology(block)?,
    };
    let key = compile_strict_operator_key(&signature, block.len() * 8)?
        .to_le_bytes()
        .to_vec();
    let mut words = block
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let policy = match profile {
        CompressionProfile::General => CompressionPolicy::MVP,
        CompressionProfile::StrictGeneratorOnly => CompressionPolicy {
            minimum_gain_fraction: STRICT_GENERATOR_GAIN_FRACTION,
            ..CompressionPolicy::MVP
        },
    };
    let mut best: Option<BlockEncoding> = None;
    for steps in 1..=MAX_STRICT_OPERATOR_STEPS {
        apply_strict_operator_step(
            &mut words,
            u64::from_le_bytes(key.clone().try_into().unwrap()),
        )?;
        let Some(candidate) = build_operator_candidate(block.len() as u32, &key, steps, &words)?
        else {
            continue;
        };
        let operator = extract_operator_block(&candidate).unwrap();
        let overhead_bytes = encoded_size_of(&candidate)
            .checked_sub(operator.key.len())
            .and_then(|value| value.checked_sub(operator.terminal_payload.len()))
            .ok_or_else(|| "operator block accounting underflow".to_string())?;
        let budget = BitBudget {
            source_bits: (block.len() * 8) as u64,
            key_bits: 64,
            crumb_bits: (operator.terminal_payload.len() * 8) as u64,
            overhead_bits: (overhead_bytes * 8) as u64,
        };
        if !policy.accepts(budget)? {
            continue;
        }
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

fn build_operator_candidate(
    original_len: u32,
    key: &[u8],
    steps: u8,
    words: &[u64],
) -> Result<Option<BlockEncoding>, String> {
    let candidate = encode_best_operator_terminal(words)?.map(|terminal| {
        BlockEncoding::Operator(OperatorBlock {
            original_len,
            key: key.to_vec(),
            steps,
            terminal_mode: terminal.mode,
            terminal_payload: terminal.payload,
        })
    });
    Ok(candidate)
}

fn debug_strict_operator_block(block: &[u8], offset: usize) -> Result<GeneratorDebugBlock, String> {
    if block.len() < 16 || block.len() % 8 != 0 || !block.len().is_power_of_two() {
        return Ok(GeneratorDebugBlock {
            offset,
            window_bytes: block.len(),
            key_hex: String::new(),
            steps: None,
            terminal_mode: None,
            key_bits: 0,
            crumb_bits: (block.len() * 8) as u64,
            overhead_bits: 0,
            total_bits: (block.len() * 8) as u64,
            accepted: false,
            reject_reason: "window-not-operator-eligible".to_string(),
        });
    }
    let signature = analyze_topology(block)?;
    let key_u64 = compile_strict_operator_key(&signature, block.len() * 8)?;
    let key = key_u64.to_le_bytes().to_vec();
    let mut words = block
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let policy = CompressionPolicy {
        minimum_gain_fraction: STRICT_GENERATOR_GAIN_FRACTION,
        ..CompressionPolicy::MVP
    };
    let mut best_observed: Option<(BlockEncoding, BitBudget)> = None;
    let mut best: Option<(BlockEncoding, BitBudget)> = None;
    for steps in 1..=MAX_STRICT_OPERATOR_STEPS {
        apply_strict_operator_step(&mut words, key_u64)?;
        let Some(candidate) = build_operator_candidate(block.len() as u32, &key, steps, &words)?
        else {
            continue;
        };
        let operator = extract_operator_block(&candidate).unwrap();
        let overhead_bytes = encoded_size_of(&candidate)
            .checked_sub(operator.key.len())
            .and_then(|value| value.checked_sub(operator.terminal_payload.len()))
            .ok_or_else(|| "operator block accounting underflow".to_string())?;
        let budget = BitBudget {
            source_bits: (block.len() * 8) as u64,
            key_bits: 64,
            crumb_bits: (operator.terminal_payload.len() * 8) as u64,
            overhead_bits: (overhead_bytes * 8) as u64,
        };
        if best_observed
            .as_ref()
            .map(|(current, _)| encoded_size_of(&candidate) < encoded_size_of(current))
            .unwrap_or(true)
        {
            best_observed = Some((candidate.clone(), budget));
        }
        if !policy.accepts(budget)? {
            continue;
        }
        if best
            .as_ref()
            .map(|(current, _)| encoded_size_of(&candidate) < encoded_size_of(current))
            .unwrap_or(true)
        {
            best = Some((candidate, budget));
        }
    }

    Ok(match best {
        Some((candidate, budget)) => {
            let operator = extract_operator_block(&candidate).unwrap();
            GeneratorDebugBlock {
                offset,
                window_bytes: block.len(),
                key_hex: hex_string(&key),
                steps: Some(operator.steps),
                terminal_mode: Some(operator.terminal_mode),
                key_bits: budget.key_bits,
                crumb_bits: budget.crumb_bits,
                overhead_bits: budget.overhead_bits,
                total_bits: budget.encoded_bits()?,
                accepted: true,
                reject_reason: "accepted".to_string(),
            }
        }
        None => match best_observed {
            Some((candidate, budget)) => {
                let operator = extract_operator_block(&candidate).unwrap();
                GeneratorDebugBlock {
                    offset,
                    window_bytes: block.len(),
                    key_hex: hex_string(&key),
                    steps: Some(operator.steps),
                    terminal_mode: Some(operator.terminal_mode),
                    key_bits: budget.key_bits,
                    crumb_bits: budget.crumb_bits,
                    overhead_bits: budget.overhead_bits,
                    total_bits: budget.encoded_bits()?,
                    accepted: false,
                    reject_reason: "best-candidate-above-10x-budget".to_string(),
                }
            }
            None => GeneratorDebugBlock {
                offset,
                window_bytes: block.len(),
                key_hex: hex_string(&key),
                steps: None,
                terminal_mode: None,
                key_bits: 64,
                crumb_bits: 0,
                overhead_bits: 0,
                total_bits: 64,
                accepted: false,
                reject_reason: "no-operator-candidate".to_string(),
            },
        },
    })
}

fn encode_best_operator_terminal(
    words: &[u64],
) -> Result<Option<OperatorTerminalCandidate>, String> {
    if words.is_empty() {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    if words.iter().all(|word| *word == words[0]) {
        candidates.push(OperatorTerminalCandidate {
            mode: OperatorTerminalMode::UniformWord,
            payload: words[0].to_le_bytes().to_vec(),
        });
    }
    if let Some(palette) = encode_operator_small_palette(words)? {
        candidates.push(palette);
    }
    if let Some(sparse) = encode_operator_sparse_exceptions(words)? {
        candidates.push(sparse);
    }
    candidates.push(OperatorTerminalCandidate {
        mode: OperatorTerminalMode::RawTerminal,
        payload: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    });
    Ok(candidates
        .into_iter()
        .min_by_key(|candidate| (candidate.payload.len(), candidate.mode as u8)))
}

fn encode_operator_small_palette(
    words: &[u64],
) -> Result<Option<OperatorTerminalCandidate>, String> {
    let palette = words
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if palette.is_empty() || palette.len() > 4 {
        return Ok(None);
    }
    let bit_width = bit_width_for_cardinality(palette.len());
    let index_by_word = palette
        .iter()
        .copied()
        .enumerate()
        .map(|(index, word)| (word, index as u8))
        .collect::<BTreeMap<_, _>>();
    let indices = words
        .iter()
        .map(|word| {
            index_by_word
                .get(word)
                .copied()
                .ok_or_else(|| "operator small-palette lookup failed".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut payload = Vec::new();
    payload.push(palette.len() as u8);
    payload.push(bit_width);
    for word in &palette {
        payload.extend_from_slice(&word.to_le_bytes());
    }
    payload.extend_from_slice(&pack_indices(&indices, bit_width));
    Ok(Some(OperatorTerminalCandidate {
        mode: OperatorTerminalMode::SmallPalette,
        payload,
    }))
}

fn encode_operator_sparse_exceptions(
    words: &[u64],
) -> Result<Option<OperatorTerminalCandidate>, String> {
    let counts = words
        .iter()
        .copied()
        .fold(BTreeMap::new(), |mut map, word| {
            *map.entry(word).or_insert(0_usize) += 1;
            map
        });
    let Some((&dominant_word, &dominant_count)) = counts.iter().max_by_key(|(_, count)| **count)
    else {
        return Ok(None);
    };
    if dominant_count == words.len() {
        return Ok(None);
    }
    let mut exception_positions = Vec::new();
    let mut exception_values = Vec::new();
    for (index, word) in words.iter().copied().enumerate() {
        if word != dominant_word {
            exception_positions.push(index);
            exception_values.push(word);
        }
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&dominant_word.to_le_bytes());
    push_u16(&mut payload, exception_positions.len() as u16);
    let position_bytes = encode_position_deltas(&exception_positions);
    push_u32(&mut payload, position_bytes.len() as u32);
    payload.extend_from_slice(&position_bytes);
    for value in &exception_values {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    Ok(Some(OperatorTerminalCandidate {
        mode: OperatorTerminalMode::SparseWordExceptions,
        payload,
    }))
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
    if block.key.len() != 8 {
        return Err("strict operator K must be exactly 8 bytes".to_string());
    }
    if block.steps == 0 {
        return Err("strict operator step count must be non-zero".to_string());
    }
    let word_count = block.original_len as usize / 8;
    let total_steps = word_count
        .checked_mul(block.steps as usize)
        .ok_or_else(|| "strict operator decoder step overflow".to_string())?;
    if total_steps > MAX_DECODER_STEPS {
        return Err("strict operator decoder gas limit exceeded".to_string());
    }
    let mut words = decode_operator_terminal(
        block.terminal_mode,
        &block.terminal_payload,
        word_count,
        block.original_len as usize,
    )?;
    let key = u64::from_le_bytes(block.key.clone().try_into().unwrap());
    for _ in 0..block.steps {
        apply_strict_operator_inverse_step(&mut words, key)?;
    }
    Ok(words.into_iter().flat_map(u64::to_le_bytes).collect())
}

fn build_trajectory_key(block: &[u8], steps: u8) -> Result<MagicKey, String> {
    let fingerprint = block_fingerprint(block)?;
    MagicKey::from_trajectory_payload(fingerprint, steps)
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

fn decode_operator_terminal(
    mode: OperatorTerminalMode,
    payload: &[u8],
    word_count: usize,
    original_len: usize,
) -> Result<Vec<u64>, String> {
    match mode {
        OperatorTerminalMode::UniformWord => {
            if payload.len() != 8 {
                return Err("uniform-word operator payload must be exactly 8 bytes".to_string());
            }
            let word = u64::from_le_bytes(payload.try_into().unwrap());
            Ok(vec![word; word_count])
        }
        OperatorTerminalMode::SmallPalette => decode_operator_small_palette(payload, word_count),
        OperatorTerminalMode::SparseWordExceptions => {
            decode_operator_sparse_exceptions(payload, word_count)
        }
        OperatorTerminalMode::RawTerminal => {
            if payload.len() != original_len {
                return Err("raw operator terminal payload length is not canonical".to_string());
            }
            Ok(payload
                .chunks_exact(8)
                .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
                .collect())
        }
    }
}

fn decode_operator_small_palette(payload: &[u8], word_count: usize) -> Result<Vec<u64>, String> {
    if payload.len() < 2 {
        return Err("operator small-palette payload is truncated".to_string());
    }
    let palette_len = payload[0] as usize;
    let bit_width = payload[1];
    if palette_len == 0 || palette_len > 4 {
        return Err("operator small-palette length must be in 1..=4".to_string());
    }
    let table_bytes = palette_len
        .checked_mul(8)
        .ok_or_else(|| "operator small-palette table overflow".to_string())?;
    if payload.len() < 2 + table_bytes {
        return Err("operator small-palette table is truncated".to_string());
    }
    let palette = payload[2..2 + table_bytes]
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let packed = &payload[2 + table_bytes..];
    let expected_bytes = ceil_div_u64(word_count as u64 * bit_width as u64, 8) as usize;
    if packed.len() != expected_bytes {
        return Err("operator small-palette index payload length is not canonical".to_string());
    }
    let indices = unpack_indices(packed, bit_width, word_count)?;
    indices
        .into_iter()
        .map(|index| {
            palette
                .get(index as usize)
                .copied()
                .ok_or_else(|| "operator small-palette index is out of range".to_string())
        })
        .collect()
}

fn decode_operator_sparse_exceptions(
    payload: &[u8],
    word_count: usize,
) -> Result<Vec<u64>, String> {
    if payload.len() < 14 {
        return Err("operator sparse-exception payload is truncated".to_string());
    }
    let dominant_word = u64::from_le_bytes(payload[..8].try_into().unwrap());
    let mut cursor = 8;
    let exception_count = read_u16(payload, &mut cursor)? as usize;
    let position_len = read_u32(payload, &mut cursor)? as usize;
    let position_bytes = read_vec(payload, &mut cursor, position_len)?;
    let positions = decode_position_deltas(&position_bytes)?;
    if positions.len() != exception_count {
        return Err("operator sparse-exception position count does not match header".to_string());
    }
    let expected_value_bytes = exception_count
        .checked_mul(8)
        .ok_or_else(|| "operator sparse-exception value length overflow".to_string())?;
    let values = read_vec(payload, &mut cursor, expected_value_bytes)?;
    if cursor != payload.len() {
        return Err("operator sparse-exception payload has trailing bytes".to_string());
    }
    let mut out = vec![dominant_word; word_count];
    for (position, chunk) in positions.iter().copied().zip(values.chunks_exact(8)) {
        if position >= word_count {
            return Err("operator sparse-exception position is out of range".to_string());
        }
        out[position] = u64::from_le_bytes(chunk.try_into().unwrap());
    }
    Ok(out)
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

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn encoded_size_of(block: &BlockEncoding) -> usize {
    let mut out = Vec::new();
    append_serialized_block(
        &mut out,
        &BlockRecord {
            window_index: 0,
            encoding: block.clone(),
        },
    );
    out.len()
}

fn serialize_single_layer_archive(archive: &Archive) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC_SINGLE);
    out.push(archive.header.base_version);
    out.push(archive.header.window_min_exp);
    out.push(archive.header.window_max_exp);
    push_u64(&mut out, archive.header.original_size);
    push_u32(&mut out, archive.header.block_count);

    for block in &archive.blocks {
        append_serialized_block(&mut out, block);
    }

    Ok(out)
}

fn append_serialized_block(out: &mut Vec<u8>, block: &BlockRecord) {
    out.push(block.window_index);
    match &block.encoding {
        BlockEncoding::Raw(raw) => {
            out.push(block.encoding.mode() as u8);
            push_u32(out, raw.original_len);
            push_u32(out, raw.payload.len() as u32);
            out.extend_from_slice(&raw.payload);
        }
        BlockEncoding::Alphabet(alpha) => {
            out.push(block.encoding.mode() as u8);
            push_u32(out, alpha.original_len);
            out.push(alpha.alphabet.len() as u8);
            out.push(alpha.bit_width);
            out.extend_from_slice(&alpha.alphabet);
            push_u32(out, alpha.breadcrumbs.len() as u32);
            out.extend_from_slice(&alpha.breadcrumbs);
        }
        BlockEncoding::SparseAlphabet(alpha) => {
            out.push(block.encoding.mode() as u8);
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
            out.push(block.encoding.mode() as u8);
            push_u32(out, spectral.original_len);
            push_u16(out, spectral.key.len() as u16);
            out.extend_from_slice(&spectral.key);
            push_u32(out, spectral.residual.len() as u32);
            out.extend_from_slice(&spectral.residual);
        }
        BlockEncoding::Trajectory(trajectory) => {
            out.push(block.encoding.mode() as u8);
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
            out.push(block.encoding.mode() as u8);
            push_u32(out, operator.original_len);
            push_u16(out, operator.key.len() as u16);
            out.extend_from_slice(&operator.key);
            out.push(operator.steps);
            out.push(operator.terminal_mode as u8);
            push_u32(out, operator.terminal_payload.len() as u32);
            out.extend_from_slice(&operator.terminal_payload);
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
        push_u32(&mut out, layer.window_min_bits);
        push_u32(&mut out, layer.window_max_bits);
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
    let window_min_exp = read_u8(input, &mut cursor)?;
    let window_max_exp = read_u8(input, &mut cursor)?;
    if window_min_exp > window_max_exp {
        return Err("window min exponent exceeds the max exponent".to_string());
    }
    let original_size = read_u64(input, &mut cursor)?;
    let block_count = read_u32(input, &mut cursor)?;
    if block_count as usize > input.len() {
        return Err("block count exceeds archive size".to_string());
    }
    let block_windows = window_sizes_for_band(PassWindowBand {
        min_window_bits: 1_u32 << window_min_exp,
        max_window_bits: 1_u32 << window_max_exp,
    })?;

    let mut blocks = Vec::with_capacity(block_count as usize);
    for _ in 0..block_count {
        let window_index = read_u8(input, &mut cursor)?;
        let window_size = *block_windows
            .get(window_index as usize)
            .ok_or_else(|| "block window index is outside the pass ladder".to_string())?;
        let mode = read_u8(input, &mut cursor)?;
        let original_len = read_u32(input, &mut cursor)?;
        if original_len as usize > window_size {
            return Err("block original length exceeds its declared window".to_string());
        }
        let encoding = match mode {
            0 => {
                let payload_len = read_u32(input, &mut cursor)? as usize;
                if payload_len != original_len as usize {
                    return Err("raw payload length does not match original length".to_string());
                }
                let payload = read_vec(input, &mut cursor, payload_len)?;
                BlockEncoding::Raw(RawBlock {
                    original_len,
                    payload,
                })
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
                BlockEncoding::Alphabet(AlphabetBlock {
                    original_len,
                    alphabet,
                    bit_width,
                    breadcrumbs,
                })
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
                BlockEncoding::SparseAlphabet(SparseAlphabetBlock {
                    original_len,
                    dense_alphabet,
                    dense_bit_width,
                    dense_breadcrumbs,
                    exception_alphabet,
                    exception_indices,
                    exception_positions,
                })
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
                BlockEncoding::Spectral(SpectralBlock {
                    original_len,
                    key,
                    residual,
                })
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
                BlockEncoding::Trajectory(TrajectoryBlock {
                    original_len,
                    key,
                    terminals,
                    terminal_indices,
                    steps,
                    breadcrumbs,
                })
            }
            4 => {
                let key_len = read_u16(input, &mut cursor)? as usize;
                let key = read_vec(input, &mut cursor, key_len)?;
                if key.len() != 8 {
                    return Err("strict operator K must be exactly 8 bytes".to_string());
                }
                let steps = read_u8(input, &mut cursor)?;
                let terminal_mode = match read_u8(input, &mut cursor)? {
                    0 => OperatorTerminalMode::UniformWord,
                    1 => OperatorTerminalMode::SmallPalette,
                    2 => OperatorTerminalMode::SparseWordExceptions,
                    3 => OperatorTerminalMode::RawTerminal,
                    _ => return Err("unknown operator terminal mode".to_string()),
                };
                let terminal_payload_len = read_u32(input, &mut cursor)? as usize;
                let terminal_payload = read_vec(input, &mut cursor, terminal_payload_len)?;
                decode_operator_terminal(
                    terminal_mode,
                    &terminal_payload,
                    original_len as usize / 8,
                    original_len as usize,
                )?;
                BlockEncoding::Operator(OperatorBlock {
                    original_len,
                    key,
                    steps,
                    terminal_mode,
                    terminal_payload,
                })
            }
            other => return Err(format!("unknown block mode: {other}")),
        };
        blocks.push(BlockRecord {
            window_index,
            encoding,
        });
    }
    ensure_fully_consumed(input, cursor)?;

    Ok(Archive {
        header: ArchiveHeader {
            base_version,
            window_min_exp,
            window_max_exp,
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
            window_min_bits: read_u32(input, &mut cursor)?,
            window_max_bits: read_u32(input, &mut cursor)?,
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
    if layers.iter().any(|layer| {
        layer.window_min_bits == 0
            || layer.window_max_bits == 0
            || layer.window_min_bits > layer.window_max_bits
            || layer.output_size >= layer.input_size
    }) {
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
        analyze_block, analyze_pass_band, apply_strict_operator_step, compress_bytes,
        compress_bytes_generator_only_strict, compress_bytes_with_outcome,
        compress_single_layer_bytes, decode_operator_block, decompress_bytes, encoded_size_of,
        inspect_archive, parse_archive, serialize_single_layer_archive, topology_score,
        try_encode_alphabet_block, try_encode_operator_block, Archive, ArchiveHeader,
        BlockEncoding, BlockRecord, CompressionProfile, OperatorBlock, PassWindowBand, RawBlock,
        MAGIC_MULTI, SINGLE_LAYER_HEADER_BYTES,
    };
    use crate::domain::kernel::operator::{contract_block, expand_block, materialize_runtime};
    use crate::domain::kernel::topology::{
        analyze_topology, compile_strict_operator_key, compile_topology_to_constant,
        TopologySignature,
    };
    use crate::domain::model::OperatorTerminalMode;

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
    fn strict_generator_only_rejects_inputs_without_a_10x_first_layer() {
        let input = (0..1024)
            .map(|index| ((index * 37 + 11) % 251) as u8)
            .collect::<Vec<_>>();
        let error = compress_bytes_generator_only_strict(&input, Some(512)).unwrap_err();
        assert!(error.contains("InsufficientFirstLayerGain"));
    }

    #[test]
    fn recursive_layers_are_used_only_when_the_complete_archive_shrinks() {
        let input = b"0123456789".repeat(500);
        let outcome = compress_bytes_with_outcome(&input, None).unwrap();
        assert_eq!(decompress_bytes(&outcome.archive).unwrap(), input);
        assert!(outcome.archive.len() < input.len() + 64);
        assert!(outcome
            .layer_summaries
            .windows(2)
            .all(|pair| pair[0].output_size == pair[1].input_size));
    }

    /// inspect_archive output_size is the payload bytes (archive minus header),
    /// not the full archive blob.  Verify against SINGLE_LAYER_HEADER_BYTES.
    #[test]
    fn inspect_archive_reports_real_input_size_and_window_band_for_single_layer_archives() {
        let input = b"0123456789".repeat(200);
        let packed = compress_single_layer_bytes(
            &input,
            PassWindowBand {
                min_window_bits: 1024,
                max_window_bits: 4096,
            },
        )
        .unwrap();
        let layers = inspect_archive(&packed).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].input_size, input.len() as u64);
        assert_eq!(
            layers[0].output_size,
            (packed.len() - SINGLE_LAYER_HEADER_BYTES) as u64
        );
        assert_eq!(layers[0].window_min_bits, 1024);
        assert_eq!(layers[0].window_max_bits, 4096);
    }

    #[test]
    fn pass_band_is_determined_per_stream_and_forms_a_dyadic_ladder() {
        let raw = vec![0xA5_u8; 4096];
        let band0 = analyze_pass_band(&raw, None, 0, CompressionProfile::General).unwrap();
        let sizes0 = super::window_sizes_for_band(band0).unwrap();
        assert!(sizes0.windows(2).all(|pair| pair[1] == pair[0] * 2));

        let packed = compress_bytes(&raw, None).unwrap();
        let band1 = analyze_pass_band(&packed, None, 1, CompressionProfile::General).unwrap();
        assert!(band1.max_window_bits <= band0.max_window_bits);
    }

    #[test]
    fn operator_constant_is_variable_length_and_roundtrips_seed_and_branch_log() {
        let input = [0x3C_u8; 512];
        let signature = analyze_topology(&input).unwrap();
        let code = compile_topology_to_constant(&signature, input.len() * 8).unwrap();
        assert!(code.as_bytes().len() > 8);
        let runtime = materialize_runtime(&code, input.len() * 8).unwrap();
        let (seed, branches) = contract_block(&runtime, &input).unwrap();
        let restored = expand_block(&runtime, &seed, &branches).unwrap();
        assert_eq!(restored, input);
    }

    #[test]
    fn operator_block_wire_format_roundtrips_through_decoder() {
        let input = [0x5A_u8; 512];
        let block = match try_encode_operator_block(&input, None, CompressionProfile::General)
            .unwrap()
            .and_then(|candidate| match candidate {
                BlockEncoding::Operator(block) => Some(block),
                _ => None,
            }) {
            Some(block) => block,
            None => {
                let signature = analyze_topology(&input).unwrap();
                let key_u64 = compile_strict_operator_key(&signature, input.len() * 8).unwrap();
                let key = key_u64.to_le_bytes().to_vec();
                let mut words = input
                    .chunks_exact(8)
                    .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
                    .collect::<Vec<_>>();
                apply_strict_operator_step(&mut words, key_u64).unwrap();
                let payload = words
                    .iter()
                    .flat_map(|word| word.to_le_bytes())
                    .collect::<Vec<_>>();
                OperatorBlock {
                    original_len: input.len() as u32,
                    key,
                    steps: 1,
                    terminal_mode: OperatorTerminalMode::RawTerminal,
                    terminal_payload: payload,
                }
            }
        };
        assert_eq!(decode_operator_block(&block).unwrap(), input);
        assert_eq!(block.key.len(), 8);
        assert!(block.steps > 0);
    }

    #[test]
    fn malformed_program_code_is_rejected_by_archive_parser() {
        let archive = Archive {
            header: ArchiveHeader {
                base_version: 1,
                window_min_exp: 6,
                window_max_exp: 6,
                original_size: 64,
                block_count: 1,
            },
            blocks: vec![BlockRecord {
                window_index: 0,
                encoding: BlockEncoding::Operator(OperatorBlock {
                    original_len: 64,
                    key: vec![0; 7],
                    steps: 1,
                    terminal_mode: OperatorTerminalMode::RawTerminal,
                    terminal_payload: vec![0; 64],
                }),
            }],
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
                window_min_exp: 3,
                window_max_exp: 3,
                original_size: 8,
                block_count: 1,
            },
            blocks: vec![BlockRecord {
                window_index: 0,
                encoding: BlockEncoding::Raw(RawBlock {
                    original_len: 8,
                    payload: vec![0; 7],
                }),
            }],
        };
        assert!(super::decompress_single_layer_bytes(
            &serialize_single_layer_archive(&malformed).unwrap()
        )
        .is_err());
    }

    #[test]
    fn encoded_size_of_matches_the_single_block_wire_format() {
        let input = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz+/".repeat(8);
        for block in [
            BlockEncoding::Raw(RawBlock {
                original_len: input.len() as u32,
                payload: input.clone(),
            }),
            try_encode_operator_block(&input, None, CompressionProfile::General)
                .unwrap()
                .unwrap_or(BlockEncoding::Raw(RawBlock {
                    original_len: input.len() as u32,
                    payload: input.clone(),
                })),
        ] {
            let archive = Archive {
                header: ArchiveHeader {
                    base_version: 1,
                    window_min_exp: (input.len() * 8).ilog2() as u8,
                    window_max_exp: (input.len() * 8).ilog2() as u8,
                    original_size: input.len() as u64,
                    block_count: 1,
                },
                blocks: vec![BlockRecord {
                    window_index: 0,
                    encoding: block.clone(),
                }],
            };
            let wire = serialize_single_layer_archive(&archive).unwrap();
            let header_len = SINGLE_LAYER_HEADER_BYTES;
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
        let compressed = compress_single_layer_bytes(
            &input,
            PassWindowBand {
                min_window_bits: 4096,
                max_window_bits: 4096,
            },
        )
        .unwrap();
        let layers = inspect_archive(&compressed).unwrap();
        assert_eq!(layers[0].input_size, input.len() as u64);
        assert_eq!(
            layers[0].output_size,
            (compressed.len() - SINGLE_LAYER_HEADER_BYTES) as u64
        );
    }

    /// topology_score must not panic when walsh_peaks is empty.
    /// This is reachable for all-zero blocks after FWHT.
    #[test]
    fn topology_score_returns_zero_for_empty_walsh_peaks() {
        let sig = TopologySignature {
            bit_len: 64,
            walsh_peaks: vec![],
            shift_scores: vec![],
            derivative: vec![0u8; 64],
            popcnt_profile: vec![0u8; 8],
        };
        assert_eq!(topology_score(&sig), 0.0);
    }

    /// topology_score must not panic on white-noise input either.
    #[test]
    fn topology_score_does_not_panic_on_white_noise() {
        let mut state: u64 = 0xC0FFEE_DEAD_BEEF;
        let input: Vec<u8> = (0..128)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect();
        let sig = crate::domain::kernel::topology::analyze_topology(&input).unwrap();
        let score = topology_score(&sig);
        assert!(
            score.is_finite(),
            "topology_score must be finite for white noise"
        );
    }

    /// Alphabet encoding must return None for a single-symbol alphabet (bit_width == 0).
    #[test]
    fn alphabet_bit_width_zero_returns_none() {
        let input = vec![0xAAu8; 256];
        let analysis = analyze_block(&input);
        let result = try_encode_alphabet_block(&input, &analysis).unwrap();
        assert!(
            result.is_none(),
            "single-symbol alphabet must not produce an Alphabet encoding"
        );
    }

    /// WHITE NOISE: full compress→decompress roundtrip on 4 KB of xorshift data.
    #[test]
    fn white_noise_4kb_roundtrip() {
        let mut state: u64 = 0x0123_4567_89AB_CDEF;
        let input: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect();
        let compressed = compress_bytes(&input, None).unwrap();
        let restored = decompress_bytes(&compressed).unwrap();
        assert_eq!(restored, input, "roundtrip failed for 4 KB white noise");
        assert!(
            compressed.len() >= input.len().saturating_sub(64),
            "white noise compressed suspiciously: {} -> {} bytes",
            input.len(),
            compressed.len()
        );
    }
}
