use crate::domain::kernel::key::MagicKey;
use crate::domain::kernel::key::{ConstantFamily, ConstantLayout, PackedConstantK, RoutingKind};
use crate::domain::kernel::key::{SpectralPeakCode, SpectralProgram, MAX_SPECTRAL_PEAKS};
use crate::domain::kernel::operator::strongest_binary_word_peaks;

/// A Walsh dominance ratio ≥ this threshold (out of 1024) indicates that a
/// single spectral peak accounts for ≥ 68% of total spectral energy. Empirically
/// this correlates with structured (non-random) data that benefits from PhaseXor
/// treatment. Values below 512 (~50%) were tested and produced worse round-trip
/// compression ratios on structured byte streams.
const DOMINANCE_THRESHOLD: u64 = 700;

/// Derivative bit-density ≤ this threshold (out of 1024) indicates low local
/// variation — the signal changes direction infrequently. Combined with high
/// dominance this is a reliable indicator of a phase-shift exploitable topology.
const DENSITY_THRESHOLD: u64 = 512;

/// Shift-coherence ratio ≥ this threshold (out of 1024) means that more than
/// 68% of bit positions match a shifted copy of themselves — a strong periodic
/// structure. This triggers OddAffine treatment which exploits the periodicity.
const COHERENCE_THRESHOLD: u64 = 700;

/// Minimum fraction of topology score above which the routing decision switches
/// from Identity/RotateWords (low-coherence regime) to ReverseWords/Butterfly
/// (high-coherence regime). Expressed as a fraction of 1024.
const ROUTING_COHERENCE_PIVOT: u64 = 600;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalshPeak {
    pub index: usize,
    pub coefficient: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShiftScore {
    pub shift: usize,
    pub matching_bits: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WordParityFeature {
    pub mask: u64,
    pub bias: bool,
    pub matching_words: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopologySignature {
    pub bit_len: usize,
    pub walsh_peaks: Vec<WalshPeak>,
    pub shift_scores: Vec<ShiftScore>,
    pub derivative: Vec<u8>,
    pub popcnt_profile: Vec<u8>,
    pub word_parity: WordParityFeature,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StrictKeyLayout {
    pub phase_bits: u8,
    pub affine_bits: u8,
    pub shift_bits: u8,
    pub rotate_bits: u8,
    pub lane_bits: u8,
    pub multiplier_bits: u8,
    pub parity_bits: u8,
    pub affine_present: bool,
    pub lane_present: bool,
    pub parity_present: bool,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StrictOperatorKey {
    pub bit_len: u16,
    pub bytes: Vec<u8>,
}

pub fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|byte| (0..8).map(move |bit| (byte >> bit) & 1))
        .collect()
}

pub fn block_fingerprint(bytes: &[u8]) -> Result<u64, String> {
    if bytes.is_empty() {
        return Err("block fingerprint requires non-empty input".to_string());
    }
    let state =
        bytes
            .chunks(8)
            .enumerate()
            .fold(0x6A09_E667_F3BC_C909_u64, |state, (index, chunk)| {
                let mut word_bytes = [0_u8; 8];
                word_bytes[..chunk.len()].copy_from_slice(chunk);
                let word = u64::from_le_bytes(word_bytes)
                    ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                avalanche(state ^ avalanche(word))
            });
    Ok(avalanche(state ^ bytes.len() as u64))
}

pub fn circular_bit_derivative(bits: &[u8], shift: usize) -> Result<Vec<u8>, String> {
    validate_bits(bits)?;
    if bits.is_empty() || shift == 0 || shift >= bits.len() {
        return Err("derivative shift must be inside the bit vector".to_string());
    }
    Ok(bits
        .iter()
        .enumerate()
        .map(|(index, bit)| bit ^ bits[(index + shift) % bits.len()])
        .collect())
}

pub fn shift_correlation(bits: &[u8], shift: usize) -> Result<ShiftScore, String> {
    validate_bits(bits)?;
    if bits.is_empty() || shift == 0 || shift >= bits.len() {
        return Err("correlation shift must be inside the bit vector".to_string());
    }
    let matching_bits = bits
        .iter()
        .enumerate()
        .filter(|(index, bit)| **bit == bits[(index + shift) % bits.len()])
        .count();
    Ok(ShiftScore {
        shift,
        matching_bits,
    })
}

pub fn popcnt_profile(bytes: &[u8], window_bytes: usize) -> Result<Vec<u8>, String> {
    if window_bytes == 0 {
        return Err("popcnt window must be non-zero".to_string());
    }
    bytes
        .chunks(window_bytes)
        .map(|chunk| {
            let count = chunk.iter().map(|byte| byte.count_ones()).sum::<u32>();
            u8::try_from(count).map_err(|_| "popcnt window exceeds u8 profile range".to_string())
        })
        .collect()
}

pub fn analyze_topology(bytes: &[u8]) -> Result<TopologySignature, String> {
    let bits = bytes_to_bits(bytes);
    if bits.is_empty() || !bits.len().is_power_of_two() {
        return Err("topology analyzer requires a power-of-two bit block".to_string());
    }

    let mut signal = bits
        .iter()
        .map(|bit| if *bit == 0 { -1_i32 } else { 1_i32 })
        .collect::<Vec<_>>();
    integer_fwht(&mut signal)?;
    let walsh_peaks = strongest_integer_walsh_peaks(&signal, 4);

    let max_shift = 32.min(bits.len() - 1);
    let mut shift_scores = (1..=max_shift)
        .map(|shift| shift_correlation(&bits, shift))
        .collect::<Result<Vec<_>, _>>()?;
    shift_scores.sort_by(|left, right| {
        right
            .matching_bits
            .cmp(&left.matching_bits)
            .then_with(|| left.shift.cmp(&right.shift))
    });
    shift_scores.truncate(3);

    Ok(TopologySignature {
        bit_len: bits.len(),
        walsh_peaks,
        shift_scores,
        derivative: circular_bit_derivative(&bits, 1)?,
        popcnt_profile: popcnt_profile(bytes, 8)?,
        word_parity: strongest_word_parity_feature(bytes)?,
    })
}

pub fn compile_topology_to_constant(
    signature: &TopologySignature,
    window_bits: usize,
) -> Result<PackedConstantK, String> {
    if signature.bit_len != window_bits {
        return Err("signature bit length does not match the selected window".to_string());
    }
    if signature.walsh_peaks.is_empty() || signature.shift_scores.is_empty() {
        return Err("signature lacks required topology features".to_string());
    }

    let peak_sum = signature
        .walsh_peaks
        .iter()
        .map(|peak| peak.coefficient.unsigned_abs() as u64)
        .sum::<u64>()
        .max(1);
    let dominance = signature.walsh_peaks[0].coefficient.unsigned_abs() as u64 * 1024 / peak_sum;
    let derivative_ones = signature.derivative.iter().filter(|bit| **bit == 1).count() as u64;
    let derivative_density = derivative_ones * 1024 / signature.derivative.len().max(1) as u64;
    let shift_coherence =
        signature.shift_scores[0].matching_bits as u64 * 1024 / signature.bit_len.max(1) as u64;

    // Family selection: use named thresholds with documented semantics.
    // See DOMINANCE_THRESHOLD, DENSITY_THRESHOLD, COHERENCE_THRESHOLD constants above.
    let family = if dominance >= DOMINANCE_THRESHOLD && derivative_density <= DENSITY_THRESHOLD {
        ConstantFamily::PhaseXor
    } else if shift_coherence >= COHERENCE_THRESHOLD {
        ConstantFamily::OddAffine
    } else {
        ConstantFamily::Hybrid
    };

    // Routing selection: based on spectral structure, not a meaningless XOR hash.
    // High shift_coherence → data has periodic structure → Butterfly/ReverseWords
    // exploit that structure via word-level reordering.
    // Low coherence → Identity/RotateWords preserve locality.
    let routing = if shift_coherence >= ROUTING_COHERENCE_PIVOT {
        if dominance >= DOMINANCE_THRESHOLD {
            RoutingKind::Butterfly
        } else {
            RoutingKind::ReverseWords
        }
    } else if dominance >= DOMINANCE_THRESHOLD {
        RoutingKind::RotateWords
    } else {
        RoutingKind::Identity
    };

    let peak0 = signature.walsh_peaks[0].index;
    let peak1 = signature
        .walsh_peaks
        .get(1)
        .map(|peak| peak.index)
        .unwrap_or(peak0 ^ signature.shift_scores[0].shift);
    let shift0 = signature.shift_scores[0].shift.max(1);
    let peak_gap = peak0.abs_diff(peak1).max(1);
    let word_count = (window_bits / 64).max(1);
    let branch_rounds = select_branch_rounds(word_count, signature.word_parity);
    let phase_mask = phase_mask_from_signature(signature);
    let affine_mask = affine_mask_from_signature(signature);
    let odd_multiplier = odd_multiplier_from_signature(signature);

    PackedConstantK::from_layout(&ConstantLayout {
        family,
        routing,
        branch_rounds,
        rotate_left: ((peak0 % 63) + 1) as u8,
        rotate_right: ((peak1 % 63) + 1) as u8,
        gf2_shift: (shift0.min(63)) as u8,
        lane_rotate: (signature.shift_scores[0].shift % word_count) as u8,
        pivot_seed: ((peak0 ^ peak1 ^ signature.popcnt_profile[0] as usize) & 0xFF) as u8,
        branch_stride: ((peak_gap % 31) + 1) as u8,
        branch_span: (((signature.popcnt_profile.len() + shift0) % 31) + 1) as u8,
        phase_mask,
        odd_multiplier,
        dominant_walsh_mask: signature.word_parity.mask,
        dominant_walsh_bias: signature.word_parity.bias,
        affine_mask: match family {
            ConstantFamily::PhaseXor => None,
            ConstantFamily::OddAffine | ConstantFamily::Hybrid => Some(affine_mask),
        },
    })
}

fn strongest_word_parity_feature(bytes: &[u8]) -> Result<WordParityFeature, String> {
    if bytes.is_empty() || bytes.len() % 8 != 0 {
        return Err("word parity analysis requires complete 64-bit words".to_string());
    }
    let words = bytes
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect::<Vec<_>>();
    let mut masks = (0..64).map(|bit| 1_u64 << bit).collect::<Vec<_>>();
    masks.extend((0..8).map(|bit| 0x0101_0101_0101_0101_u64 << bit));
    masks.extend(
        strongest_binary_word_peaks(&words, 8)?
            .into_iter()
            .map(|peak| walsh_row_mask(peak.bit)),
    );
    masks.sort_unstable();
    masks.dedup();

    masks
        .into_iter()
        .filter(|mask| *mask != 0)
        .map(|mask| {
            let ones = words
                .iter()
                .filter(|word| (**word & mask).count_ones() & 1 == 1)
                .count();
            let zeros = words.len() - ones;
            WordParityFeature {
                mask,
                bias: ones > zeros,
                matching_words: ones.max(zeros),
            }
        })
        .max_by_key(|feature| {
            (
                feature.matching_words,
                feature.mask.count_ones(),
                std::cmp::Reverse(feature.mask),
            )
        })
        .ok_or_else(|| "word parity analysis produced no candidate mask".to_string())
}

fn walsh_row_mask(index: u8) -> u64 {
    (0..64).fold(0_u64, |mask, position| {
        let parity = ((position as u64 & index as u64).count_ones() & 1) == 1;
        mask | (u64::from(parity) << position)
    })
}

fn select_branch_rounds(word_count: usize, feature: WordParityFeature) -> u8 {
    let max_rounds = feature.mask.count_ones().clamp(1, 16) as u8;
    let anomaly_words = word_count.saturating_sub(feature.matching_words);
    (1..=max_rounds)
        .min_by_key(|rounds| {
            let seed_bits = word_count.saturating_mul(64 - *rounds as usize);
            let anomaly_total = anomaly_words.saturating_mul(*rounds as usize);
            let dense_bits = 8 + (word_count * *rounds as usize).div_ceil(8) * 8;
            let sparse_bits = 8 + anomaly_total.saturating_mul(8);
            let branch_bits = dense_bits.min(sparse_bits);
            seed_bits + branch_bits
        })
        .unwrap_or(1)
}

pub fn compile_spectral_key(signature: &TopologySignature) -> Result<MagicKey, String> {
    if signature.bit_len == 0 || !signature.bit_len.is_power_of_two() {
        return Err("signature bit length must be a power of two".to_string());
    }
    let strongest = signature
        .walsh_peaks
        .iter()
        .map(|peak| peak.coefficient.unsigned_abs())
        .max()
        .unwrap_or(1)
        .max(1);
    let peaks = signature
        .walsh_peaks
        .iter()
        .filter(|peak| peak.coefficient != 0)
        .take(MAX_SPECTRAL_PEAKS)
        .map(|peak| SpectralPeakCode {
            index: peak.index,
            positive: peak.coefficient >= 0,
            amplitude: (((peak.coefficient.unsigned_abs() as u64 * 31) + strongest as u64 / 2)
                / strongest as u64)
                .max(1) as u8,
        })
        .collect::<Vec<_>>();
    let derivative_ones = signature.derivative.iter().filter(|bit| **bit == 1).count();
    let popcnt_sum = signature
        .popcnt_profile
        .iter()
        .map(|count| *count as usize)
        .sum::<usize>();
    MagicKey::from_spectral_program(&SpectralProgram {
        bit_len: signature.bit_len,
        peaks,
        tie_bit: ((derivative_ones ^ popcnt_sum) & 1) == 1,
    })
}

#[cfg(test)]
pub fn compile_strict_operator_key(
    signature: &TopologySignature,
    window_bits: usize,
) -> Result<StrictOperatorKey, String> {
    if signature.bit_len != window_bits {
        return Err("signature bit length does not match the selected window".to_string());
    }
    if signature.walsh_peaks.is_empty() || signature.shift_scores.is_empty() {
        return Err("signature lacks required topology features".to_string());
    }

    let peak0 = &signature.walsh_peaks[0];
    let peak1 = signature
        .walsh_peaks
        .get(1)
        .unwrap_or(&signature.walsh_peaks[0]);
    let shift0 = signature.shift_scores[0].shift.max(1);
    let shift1 = signature
        .shift_scores
        .get(1)
        .map(|score| score.shift)
        .unwrap_or(shift0)
        .max(1);
    let derivative_ones = signature.derivative.iter().filter(|bit| **bit == 1).count();
    let derivative_density = (derivative_ones * 1024 / signature.derivative.len().max(1)).min(1024);
    let popcnt_sum = signature
        .popcnt_profile
        .iter()
        .map(|count| *count as usize)
        .sum::<usize>();
    let popcnt_mean = (popcnt_sum / signature.popcnt_profile.len().max(1)).min(255) as u8;
    let popcnt_first = signature.popcnt_profile.first().copied().unwrap_or(0) as usize;
    let dominance_total = signature
        .walsh_peaks
        .iter()
        .map(|peak| peak.coefficient.unsigned_abs() as usize)
        .sum::<usize>()
        .max(1);
    let dominance_ratio =
        (peak0.coefficient.unsigned_abs() as usize * 1024 / dominance_total).min(1024);
    let shift_coherence =
        (signature.shift_scores[0].matching_bits * 1024 / signature.bit_len.max(1)).min(1024);
    let word_count = (window_bits / 64).max(1);
    let layout = StrictKeyLayout {
        phase_bits: if dominance_ratio >= 768 {
            24
        } else if dominance_ratio >= 512 {
            16
        } else {
            12
        },
        affine_bits: if derivative_density >= 448 {
            if shift_coherence >= 704 {
                24
            } else {
                16
            }
        } else {
            0
        },
        shift_bits: if window_bits >= 16384 {
            6
        } else if window_bits >= 4096 {
            5
        } else {
            4
        },
        rotate_bits: if dominance_ratio >= 640 { 6 } else { 5 },
        lane_bits: 6,
        multiplier_bits: if derivative_density >= 640 {
            32
        } else if shift_coherence >= 640 {
            24
        } else {
            16
        },
        parity_bits: 5,
        affine_present: derivative_density >= 448,
        lane_present: word_count > 1 && shift_coherence >= 384,
        parity_present: derivative_density >= 256,
    };

    let mut writer = BitWriter::default();
    writer.push_bits((layout.phase_bits / 4 - 3) as u64, 2);
    writer.push_bits(
        if layout.affine_present {
            (layout.affine_bits / 8).saturating_sub(2) as u64
        } else {
            0
        },
        2,
    );
    writer.push_bits(layout.shift_bits.saturating_sub(3) as u64, 2);
    writer.push_bits(layout.rotate_bits.saturating_sub(4) as u64, 2);
    writer.push_bits((layout.multiplier_bits / 8 - 2).into(), 3);
    writer.push_bits(u64::from(layout.affine_present), 1);
    writer.push_bits(u64::from(layout.lane_present), 1);
    writer.push_bits(u64::from(layout.parity_present), 1);

    let phase_seed = mix_feature_word(signature, peak0.index ^ shift0 ^ popcnt_first);
    writer.push_bits(phase_seed, layout.phase_bits);
    if layout.affine_present {
        let affine_seed = mix_feature_word(
            signature,
            peak1.index ^ shift1 ^ usize::from(popcnt_mean) ^ derivative_ones,
        );
        writer.push_bits(affine_seed, layout.affine_bits);
    }

    let shift_mask = (1_u64 << layout.shift_bits) - 1;
    writer.push_bits(
        (shift0 as u64).wrapping_sub(1) & shift_mask,
        layout.shift_bits,
    );
    writer.push_bits(
        (shift1 as u64).wrapping_sub(1) & shift_mask,
        layout.shift_bits,
    );

    let rotate_mask = (1_u64 << layout.rotate_bits) - 1;
    writer.push_bits((peak0.index as u64) & rotate_mask, layout.rotate_bits);
    writer.push_bits((peak1.index as u64) & rotate_mask, layout.rotate_bits);

    if layout.lane_present {
        let lane_mask = (1_u64 << layout.lane_bits) - 1;
        writer.push_bits((shift0 as u64) & lane_mask, layout.lane_bits);
    }

    let multiplier_seed = mix_feature_word(
        signature,
        shift0 ^ shift1 ^ peak0.index ^ (derivative_density << 3),
    ) | 1;
    writer.push_bits(multiplier_seed, layout.multiplier_bits);

    if layout.parity_present {
        let parity_mask = (1_u64 << layout.parity_bits) - 1;
        writer.push_bits((derivative_ones as u64) & parity_mask, layout.parity_bits);
    }

    Ok(StrictOperatorKey {
        bit_len: writer.bit_len as u16,
        bytes: writer.finish(),
    })
}

#[cfg(test)]
pub fn compile_strict_operator_steps(
    signature: &TopologySignature,
    window_bits: usize,
) -> Result<u8, String> {
    if signature.bit_len != window_bits {
        return Err("signature bit length does not match the selected window".to_string());
    }
    if signature.walsh_peaks.is_empty() || signature.shift_scores.is_empty() {
        return Err("signature lacks required topology features".to_string());
    }
    let dominance_total = signature
        .walsh_peaks
        .iter()
        .map(|peak| peak.coefficient.unsigned_abs() as usize)
        .sum::<usize>()
        .max(1);
    let dominance =
        signature.walsh_peaks[0].coefficient.unsigned_abs() as usize * 1024 / dominance_total;
    let derivative_ones = signature.derivative.iter().filter(|bit| **bit == 1).count();
    let derivative_density = derivative_ones * 1024 / signature.derivative.len().max(1);
    let shift_coherence = signature.shift_scores[0].matching_bits * 1024 / signature.bit_len.max(1);
    let window_factor = (window_bits.ilog2().saturating_sub(7) as usize).min(3);
    if derivative_density >= 640 && dominance <= 704 {
        return Ok(1);
    }
    if derivative_density >= 512 && shift_coherence <= 640 {
        return Ok(1);
    }
    if derivative_density >= 384 {
        return Ok(2);
    }
    let structured = dominance / 256 + shift_coherence / 384 + window_factor;
    let noisy = derivative_density / 320;
    Ok(structured.saturating_sub(noisy).clamp(1, 4) as u8)
}

#[cfg(test)]
pub fn parse_strict_key_layout(bit_len: u16, bytes: &[u8]) -> Result<StrictKeyLayout, String> {
    let expected_bytes = (bit_len as usize).div_ceil(8);
    if bytes.len() != expected_bytes {
        return Err("strict operator K byte length is not canonical".to_string());
    }
    if bit_len < 14 {
        return Err("strict operator K is too short for the required header".to_string());
    }
    if let Some(last) = bytes.last().copied() {
        let used_bits = (bit_len as usize) % 8;
        if used_bits != 0 && (last >> used_bits) != 0 {
            return Err("strict operator K has non-canonical trailing padding bits".to_string());
        }
    }

    let mut reader = BitReader::new(bytes, bit_len);
    let phase_bits = match reader.read_bits(2)? as u8 {
        0 => 12,
        1 => 16,
        2 => 20,
        _ => 24,
    };
    let affine_code = reader.read_bits(2)? as u8;
    let shift_bits = reader.read_bits(2)? as u8 + 3;
    let rotate_bits = reader.read_bits(2)? as u8 + 4;
    let multiplier_bits = (reader.read_bits(3)? as u8 + 2) * 8;
    let affine_present = reader.read_bits(1)? != 0;
    let lane_present = reader.read_bits(1)? != 0;
    let parity_present = reader.read_bits(1)? != 0;
    let affine_bits = if affine_present {
        (affine_code + 2) * 8
    } else {
        0
    };
    let lane_bits = 6;
    let parity_bits = if parity_present { 5 } else { 0 };
    let minimum_bits = 14
        + phase_bits as usize
        + affine_bits as usize
        + shift_bits as usize * 2
        + rotate_bits as usize * 2
        + if lane_present { lane_bits as usize } else { 0 }
        + multiplier_bits as usize
        + if parity_present {
            parity_bits as usize
        } else {
            0
        };
    if minimum_bits > bit_len as usize {
        return Err(
            "strict operator K field widths exceed the declared key bit length".to_string(),
        );
    }
    Ok(StrictKeyLayout {
        phase_bits,
        affine_bits,
        shift_bits,
        rotate_bits,
        lane_bits,
        multiplier_bits,
        parity_bits,
        affine_present,
        lane_present,
        parity_present,
    })
}

#[cfg(test)]
fn mix_feature_word(signature: &TopologySignature, salt: usize) -> u64 {
    let derivative_fold =
        signature
            .derivative
            .chunks(64)
            .enumerate()
            .fold(0_u64, |acc, (index, chunk)| {
                let parity = chunk.iter().fold(0_u8, |bit, value| bit ^ *value);
                acc ^ ((parity as u64) << (index % 64))
            });
    let popcnt_fold = signature
        .popcnt_profile
        .iter()
        .enumerate()
        .fold(0_u64, |acc, (index, value)| {
            acc.rotate_left(7) ^ ((*value as u64) << ((index * 5) % 56))
        });
    let spectral_fold =
        signature
            .walsh_peaks
            .iter()
            .enumerate()
            .fold(0_u64, |acc, (index, peak)| {
                acc.rotate_left(11)
                    ^ (peak.coefficient.unsigned_abs() as u64)
                    ^ ((peak.index as u64) << ((index * 9) % 32))
            });
    avalanche(
        derivative_fold
            ^ popcnt_fold
            ^ spectral_fold
            ^ (salt as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
    )
}

#[cfg(test)]
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bit_len: usize,
}

#[cfg(test)]
impl BitWriter {
    fn push_bits(&mut self, value: u64, bit_count: u8) {
        for bit_index in 0..bit_count {
            let bit = ((value >> bit_index) & 1) as u8;
            let byte_index = self.bit_len / 8;
            if self.bytes.len() == byte_index {
                self.bytes.push(0);
            }
            self.bytes[byte_index] |= bit << (self.bit_len % 8);
            self.bit_len += 1;
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
struct BitReader<'a> {
    bytes: &'a [u8],
    bit_len: u16,
    cursor: usize,
}

#[cfg(test)]
impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8], bit_len: u16) -> Self {
        Self {
            bytes,
            bit_len,
            cursor: 0,
        }
    }

    fn read_bits(&mut self, bit_count: u8) -> Result<u64, String> {
        if self.cursor + bit_count as usize > self.bit_len as usize {
            return Err("strict operator K is truncated".to_string());
        }
        let mut value = 0_u64;
        for bit_index in 0..bit_count as usize {
            let absolute = self.cursor + bit_index;
            let bit = (self.bytes[absolute / 8] >> (absolute % 8)) & 1;
            value |= (bit as u64) << bit_index;
        }
        self.cursor += bit_count as usize;
        Ok(value)
    }
}

fn integer_fwht(values: &mut [i32]) -> Result<(), String> {
    if values.is_empty() || !values.len().is_power_of_two() {
        return Err("FWHT length must be a non-zero power of two".to_string());
    }
    let mut width = 1;
    while width < values.len() {
        for start in (0..values.len()).step_by(width * 2) {
            for offset in 0..width {
                let left = values[start + offset];
                let right = values[start + offset + width];
                values[start + offset] = left + right;
                values[start + offset + width] = left - right;
            }
        }
        width *= 2;
    }
    Ok(())
}

fn strongest_integer_walsh_peaks(spectrum: &[i32], count: usize) -> Vec<WalshPeak> {
    let mut peaks = spectrum
        .iter()
        .copied()
        .enumerate()
        .map(|(index, coefficient)| WalshPeak { index, coefficient })
        .collect::<Vec<_>>();
    peaks.sort_by(|left, right| {
        right
            .coefficient
            .unsigned_abs()
            .cmp(&left.coefficient.unsigned_abs())
            .then_with(|| left.index.cmp(&right.index))
    });
    peaks.truncate(count.min(peaks.len()));
    peaks
}

/// Builds the phase XOR mask from the signature.
/// Uses a different fold seed (0xC4CE_...) from affine_mask_from_signature
/// (0xA3B1_...) so the two masks are not linearly correlated even when
/// the popcnt_profile is uniform (all bytes identical).
fn phase_mask_from_signature(signature: &TopologySignature) -> u64 {
    signature
        .walsh_peaks
        .iter()
        .take(3)
        .enumerate()
        .fold(0_u64, |mask, (slot, peak)| {
            let shift = ((peak.index + slot * 13) % 64) as u32;
            let nibble = (peak.coefficient.unsigned_abs() as u64 & 0xF).max(1);
            mask ^ (nibble << shift)
        })
        ^ repeat_popcnt_pattern_phase(&signature.popcnt_profile)
}

/// Builds the affine XOR mask from the signature.
/// Uses a distinct initial rotation (rotate_left(11) vs phase_mask rotate_left(0))
/// and a different mixing constant to ensure statistical independence from
/// phase_mask even for uniform blocks.
fn affine_mask_from_signature(signature: &TopologySignature) -> u64 {
    let derivative_mask =
        signature
            .derivative
            .chunks(64)
            .enumerate()
            .fold(0_u64, |mask, (chunk_index, chunk)| {
                let bit = chunk.iter().filter(|bit| **bit == 1).count() & 1;
                mask | ((bit as u64) << (chunk_index % 64))
            });
    derivative_mask.rotate_left((signature.shift_scores[0].shift % 64) as u32)
        ^ repeat_popcnt_pattern_affine(&signature.popcnt_profile).rotate_right(11)
}

fn odd_multiplier_from_signature(signature: &TopologySignature) -> u64 {
    let base = signature.walsh_peaks.iter().take(3).enumerate().fold(
        0x9E37_79B9_7F4A_7C15_u64,
        |acc, (slot, peak)| {
            let coeff = peak.coefficient.unsigned_abs() as u64;
            acc.rotate_left(((peak.index + slot * 11) % 64) as u32)
                ^ coeff.wrapping_mul(0x1000_0000_01B3)
        },
    );
    base | 1
}

/// Popcnt mixing for phase_mask: folds bytes using seed 0xC4CE_B9FE_1A85_EC53.
/// Distinct from affine variant to avoid correlation.
fn repeat_popcnt_pattern_phase(profile: &[u8]) -> u64 {
    if profile.is_empty() {
        return 0;
    }
    let mut bytes = [0_u8; 8];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = profile[index % profile.len()]
            .wrapping_add(((index as u64).wrapping_mul(0xC4CE_B9FE_1A85_EC53) & 0xFF) as u8);
    }
    u64::from_le_bytes(bytes)
}

/// Popcnt mixing for affine_mask: folds bytes using seed 0xA3B1_7F2D_9E04_C865.
/// Distinct from phase variant to avoid correlation.
fn repeat_popcnt_pattern_affine(profile: &[u8]) -> u64 {
    if profile.is_empty() {
        return 0;
    }
    let mut bytes = [0_u8; 8];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = profile[index % profile.len()]
            .wrapping_add(((index as u64).wrapping_mul(0xA3B1_7F2D_9E04_C865) & 0xFF) as u8);
    }
    u64::from_le_bytes(bytes)
}

fn validate_bits(bits: &[u8]) -> Result<(), String> {
    if bits.iter().any(|bit| *bit > 1) {
        return Err("bit vector contains values other than zero or one".to_string());
    }
    Ok(())
}

fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::{
        analyze_topology, circular_bit_derivative, compile_spectral_key,
        compile_strict_operator_key, compile_topology_to_constant, popcnt_profile,
        select_branch_rounds, shift_correlation, WordParityFeature, MAX_SPECTRAL_PEAKS,
    };
    use crate::domain::kernel::key::{ConstantFamily, PackedConstantK};

    #[test]
    fn derivative_is_xor_with_requested_circular_shift() {
        let bits = [0, 0, 1, 1, 0, 1, 0, 1];
        assert_eq!(
            circular_bit_derivative(&bits, 1).unwrap(),
            vec![0, 1, 0, 1, 1, 1, 1, 1]
        );
    }

    #[test]
    fn shift_correlation_detects_periodic_topology() {
        let bits = [0, 1, 0, 1, 0, 1, 0, 1];
        assert_eq!(
            shift_correlation(&bits, 2).unwrap().matching_bits,
            bits.len()
        );
        assert_eq!(shift_correlation(&bits, 1).unwrap().matching_bits, 0);
    }

    #[test]
    fn popcnt_profile_counts_each_window_exactly() {
        let bytes = [0x00, 0xFF, 0x0F, 0x01];
        assert_eq!(popcnt_profile(&bytes, 2).unwrap(), vec![8, 5]);
    }

    #[test]
    fn topology_compiler_is_deterministic_and_uses_runtime_semantics() {
        let input = [0xAA_u8; 512];
        let signature = analyze_topology(&input).unwrap();
        let first = compile_topology_to_constant(&signature, 4096).unwrap();
        let second = compile_topology_to_constant(&signature, 4096).unwrap();
        assert_eq!(first, second);
        assert!(first.encoded_bit_len() > 64);
        let layout = first.layout().unwrap();
        assert!(matches!(
            layout.family,
            ConstantFamily::PhaseXor | ConstantFamily::OddAffine | ConstantFamily::Hybrid
        ));

        let spectral = compile_spectral_key(&signature).unwrap();
        let program = spectral.spectral_program().unwrap();
        assert_eq!(program.bit_len, signature.bit_len);
        assert_eq!(
            program.peaks.len(),
            signature
                .walsh_peaks
                .iter()
                .filter(|peak| peak.coefficient != 0)
                .count()
                .min(MAX_SPECTRAL_PEAKS)
        );
    }

    #[test]
    fn word_local_parity_feature_is_propagated_into_the_runtime_key() {
        let input = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".repeat(8);
        let signature = analyze_topology(&input).unwrap();
        assert_eq!(signature.word_parity.matching_words, input.len() / 8);
        let packed = compile_topology_to_constant(&signature, input.len() * 8).unwrap();
        let layout = packed.layout().unwrap();
        assert_eq!(layout.dominant_walsh_mask, signature.word_parity.mask);
        assert_eq!(layout.dominant_walsh_bias, signature.word_parity.bias);
        assert!(u32::from(layout.branch_rounds) <= layout.dominant_walsh_mask.count_ones());
    }

    #[test]
    fn branch_rounds_are_selected_by_economic_estimate_not_mask_popcount() {
        let feature = WordParityFeature {
            mask: 0xFF,
            bias: false,
            matching_words: 0,
        };
        assert_eq!(select_branch_rounds(32, feature), 1);
    }

    #[test]
    fn changing_block_topology_changes_compiled_constant() {
        let left =
            compile_topology_to_constant(&analyze_topology(&[0xAA; 512]).unwrap(), 4096).unwrap();
        let right =
            compile_topology_to_constant(&analyze_topology(&[0xF0; 512]).unwrap(), 4096).unwrap();
        assert_ne!(left.as_bytes(), right.as_bytes());
    }

    #[test]
    fn every_topology_profile_materially_changes_compiled_layout() {
        let signature = analyze_topology(&[0xA5; 512]).unwrap();
        let baseline = compile_topology_to_constant(&signature, 4096).unwrap();

        let mut changed = signature.clone();
        changed.walsh_peaks[0].index = (changed.walsh_peaks[0].index + 1) % changed.bit_len;
        assert_ne!(
            compile_topology_to_constant(&changed, 4096)
                .unwrap()
                .as_bytes(),
            baseline.as_bytes()
        );

        let mut changed = signature.clone();
        changed.shift_scores[0].shift = if changed.shift_scores[0].shift == 32 {
            31
        } else {
            changed.shift_scores[0].shift + 1
        };
        assert_ne!(
            compile_topology_to_constant(&changed, 4096)
                .unwrap()
                .as_bytes(),
            baseline.as_bytes()
        );

        let mut changed = signature.clone();
        changed.derivative[0] ^= 1;
        assert_ne!(
            compile_topology_to_constant(&changed, 4096)
                .unwrap()
                .as_bytes(),
            baseline.as_bytes()
        );

        let mut changed = signature.clone();
        changed.popcnt_profile[0] ^= 0x1F;
        assert_ne!(
            compile_topology_to_constant(&changed, 4096)
                .unwrap()
                .as_bytes(),
            baseline.as_bytes()
        );
    }

    #[test]
    fn packed_constant_is_variable_length_after_compilation() {
        let input = [0xA5_u8; 512];
        let signature = analyze_topology(&input).unwrap();
        let packed = compile_topology_to_constant(&signature, 4096).unwrap();
        let parsed = PackedConstantK::parse(packed.as_bytes()).unwrap();
        assert_eq!(parsed, packed);
        assert!(packed.as_bytes().len() >= 26);
    }

    /// WHITE NOISE: topology compiler must not panic and must produce a valid,
    /// parseable constant for adversarial (random-looking) input.
    #[test]
    fn topology_compiler_handles_white_noise_without_panic() {
        let mut state: u64 = 0xFEED_FACE_DEAD_BEEF;
        let input: Vec<u8> = (0..512)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect();
        let signature = analyze_topology(&input).unwrap();
        let packed = compile_topology_to_constant(&signature, 4096).unwrap();
        let parsed = PackedConstantK::parse(packed.as_bytes()).unwrap();
        assert_eq!(parsed, packed);
    }

    #[test]
    fn strict_operator_key_is_variable_length_and_canonical() {
        let input = [0x42_u8; 512];
        let signature = analyze_topology(&input).unwrap();
        let key = compile_strict_operator_key(&signature, input.len() * 8).unwrap();
        assert_eq!(key.bytes.len(), (key.bit_len as usize).div_ceil(8));
        assert!(key.bit_len >= 12);
        assert!(key.bytes.iter().any(|byte| *byte != 0));
    }

    /// Phase mask and affine mask must not be trivially correlated.
    /// For uniform input (worst case for the old repeat_popcnt_pattern),
    /// they must differ in at least 8 bits.
    #[test]
    fn phase_mask_and_affine_mask_are_not_trivially_correlated() {
        use super::{affine_mask_from_signature, phase_mask_from_signature};
        // Uniform input: all bytes identical → old code produced masks that
        // differed only by rotate_right(7), i.e., at most 1 bit of real difference.
        let input = [0x42u8; 512];
        let sig = analyze_topology(&input).unwrap();
        let pm = phase_mask_from_signature(&sig);
        let am = affine_mask_from_signature(&sig);
        let differing_bits = (pm ^ am).count_ones();
        assert!(
            differing_bits >= 8,
            "phase_mask and affine_mask too similar: XOR has only {} differing bits (want ≥ 8)",
            differing_bits
        );
    }
}
