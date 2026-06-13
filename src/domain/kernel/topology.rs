use crate::domain::kernel::key::MagicKey;
use crate::domain::kernel::key::{
    ConstantFamily, ConstantLayout, PackedConstantK, RoutingKind, SpectralPeakCode,
    SpectralProgram, MAX_SPECTRAL_PEAKS,
};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopologySignature {
    pub bit_len: usize,
    pub walsh_peaks: Vec<WalshPeak>,
    pub shift_scores: Vec<ShiftScore>,
    pub derivative: Vec<u8>,
    pub popcnt_profile: Vec<u8>,
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
    let family = if dominance >= 700 && derivative_density <= 512 {
        ConstantFamily::PhaseXor
    } else if shift_coherence >= 700 {
        ConstantFamily::OddAffine
    } else {
        ConstantFamily::Hybrid
    };
    let routing = match (signature.walsh_peaks[0].index
        ^ signature.shift_scores[0].shift
        ^ signature.popcnt_profile.len())
        & 0x3
    {
        0 => RoutingKind::Identity,
        1 => RoutingKind::RotateWords,
        2 => RoutingKind::ReverseWords,
        _ => RoutingKind::Butterfly,
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
    let branch_rounds = ((dominance + shift_coherence) / 256).clamp(2, 12) as u8;
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
        affine_mask: match family {
            ConstantFamily::PhaseXor => None,
            ConstantFamily::OddAffine | ConstantFamily::Hybrid => Some(affine_mask),
        },
    })
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
        ^ repeat_popcnt_pattern(&signature.popcnt_profile)
}

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
        ^ repeat_popcnt_pattern(&signature.popcnt_profile).rotate_right(7)
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

fn repeat_popcnt_pattern(profile: &[u8]) -> u64 {
    if profile.is_empty() {
        return 0;
    }
    let mut bytes = [0_u8; 8];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = profile[index % profile.len()];
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
        compile_topology_to_constant, popcnt_profile, shift_correlation, MAX_SPECTRAL_PEAKS,
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
}
