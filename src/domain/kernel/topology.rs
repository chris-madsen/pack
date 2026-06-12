use crate::domain::kernel::base::PatternId;
use crate::domain::kernel::key::{KeyHeader, KeySegment, MagicKey, SegmentKind};
use crate::domain::kernel::spectral::{normalized_fwht, strongest_walsh_peaks, SpectralPeak};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShiftScore {
    pub shift: usize,
    pub matching_bits: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopologySignature {
    pub bit_len: usize,
    pub walsh_peaks: Vec<SpectralPeak>,
    pub shift_scores: Vec<ShiftScore>,
    pub derivative: Vec<u8>,
    pub popcnt_profile: Vec<u8>,
    pub spectrum_fold: u64,
    pub derivative_fold: u64,
}

pub fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|byte| (0..8).map(move |bit| (byte >> bit) & 1))
        .collect()
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
        .map(|bit| if *bit == 0 { -1.0 } else { 1.0 })
        .collect::<Vec<_>>();
    normalized_fwht(&mut signal)?;
    let walsh_peaks = strongest_walsh_peaks(&signal, 4);

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

    let derivative = circular_bit_derivative(&bits, 1)?;
    let popcnt_profile = popcnt_profile(bytes, 8)?;
    let spectrum_fold = fold_spectrum(&signal);
    let derivative_fold = fold_bits(&derivative);

    Ok(TopologySignature {
        bit_len: bits.len(),
        walsh_peaks,
        shift_scores,
        derivative,
        popcnt_profile,
        spectrum_fold,
        derivative_fold,
    })
}

pub fn compile_topology_to_key(signature: &TopologySignature) -> Result<MagicKey, String> {
    if signature.bit_len == 0 || !signature.bit_len.is_power_of_two() {
        return Err("signature bit length must be a power of two".to_string());
    }
    if signature.walsh_peaks.is_empty() || signature.shift_scores.is_empty() {
        return Err("signature lacks required topology features".to_string());
    }

    let shifts = signature
        .shift_scores
        .iter()
        .map(|score| score.shift as u8)
        .collect::<Vec<_>>();
    let odd_multiplier = signature.spectrum_fold | 1;
    let phase_mask = signature
        .walsh_peaks
        .iter()
        .fold(0_u64, |mask, peak| mask ^ (1_u64 << (peak.index % 64)));

    let mut rev_mix = shifts;
    rev_mix.extend_from_slice(&odd_multiplier.to_le_bytes());
    rev_mix.extend_from_slice(&signature.derivative_fold.to_le_bytes());

    let walsh_payload = signature
        .walsh_peaks
        .iter()
        .flat_map(|peak| (peak.index as u16).to_le_bytes())
        .collect::<Vec<_>>();
    let popcnt_fold = signature
        .popcnt_profile
        .iter()
        .enumerate()
        .fold(0_u8, |acc, (index, count)| {
            acc.rotate_left(1) ^ count.wrapping_add(index as u8)
        });

    let key = MagicKey {
        header: KeyHeader {
            version: 1,
            main_pattern_id: PatternId::SpectralInvolution,
            rounds: signature.shift_scores.len() as u8,
            block_log2: signature.bit_len.ilog2() as u8,
            flags: 0,
        },
        segments: vec![
            KeySegment {
                kind: SegmentKind::RevMix,
                payload: rev_mix,
            },
            KeySegment {
                kind: SegmentKind::PhaseMask,
                payload: phase_mask.to_le_bytes().to_vec(),
            },
            KeySegment {
                kind: SegmentKind::WalshConfig,
                payload: walsh_payload,
            },
            KeySegment {
                kind: SegmentKind::CrumbConfig,
                payload: vec![1, popcnt_fold],
            },
        ],
    };
    key.validate()?;
    Ok(key)
}

fn validate_bits(bits: &[u8]) -> Result<(), String> {
    if bits.iter().any(|bit| *bit > 1) {
        return Err("bit vector contains values other than zero or one".to_string());
    }
    Ok(())
}

fn fold_spectrum(spectrum: &[f64]) -> u64 {
    spectrum
        .iter()
        .enumerate()
        .fold(0_u64, |acc, (index, value)| {
            let quantized = value.to_bits().rotate_left((index % 64) as u32);
            acc ^ quantized.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        })
}

fn fold_bits(bits: &[u8]) -> u64 {
    bits.iter().enumerate().fold(0_u64, |acc, (index, bit)| {
        acc.rotate_left(5) ^ ((*bit as u64) << (index % 8)) ^ index as u64
    })
}

#[cfg(test)]
mod tests {
    use crate::domain::kernel::key::SegmentKind;

    use super::{
        analyze_topology, circular_bit_derivative, compile_topology_to_key, popcnt_profile,
        shift_correlation,
    };

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
    fn topology_compiler_is_deterministic_and_uses_all_required_profiles() {
        let input = [0xAA_u8; 64];
        let signature = analyze_topology(&input).unwrap();
        let first = compile_topology_to_key(&signature).unwrap();
        let second = compile_topology_to_key(&signature).unwrap();
        assert_eq!(first, second);

        let rev_mix = first
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::RevMix)
            .unwrap();
        assert!(rev_mix.payload.starts_with(
            &signature
                .shift_scores
                .iter()
                .map(|score| score.shift as u8)
                .collect::<Vec<_>>()
        ));
        let multiplier_offset = signature.shift_scores.len();
        let multiplier = u64::from_le_bytes(
            rev_mix.payload[multiplier_offset..multiplier_offset + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(multiplier, signature.spectrum_fold | 1);
        assert_eq!(multiplier & 1, 1);
        let derivative = u64::from_le_bytes(
            rev_mix.payload[multiplier_offset + 8..multiplier_offset + 16]
                .try_into()
                .unwrap(),
        );
        assert_eq!(derivative, signature.derivative_fold);

        let walsh = first
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::WalshConfig)
            .unwrap();
        let encoded_peaks = walsh
            .payload
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]) as usize)
            .collect::<Vec<_>>();
        assert_eq!(
            encoded_peaks,
            signature
                .walsh_peaks
                .iter()
                .map(|peak| peak.index)
                .collect::<Vec<_>>()
        );

        let phase = first
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::PhaseMask)
            .unwrap();
        let phase_mask = u64::from_le_bytes(phase.payload.clone().try_into().unwrap());
        let expected_phase_mask = signature
            .walsh_peaks
            .iter()
            .fold(0_u64, |mask, peak| mask ^ (1_u64 << (peak.index % 64)));
        assert_eq!(phase_mask, expected_phase_mask);

        let crumb = first
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::CrumbConfig)
            .unwrap();
        let expected_popcnt_fold = signature
            .popcnt_profile
            .iter()
            .enumerate()
            .fold(0_u8, |acc, (index, count)| {
                acc.rotate_left(1) ^ count.wrapping_add(index as u8)
            });
        assert_eq!(crumb.payload, vec![1, expected_popcnt_fold]);
    }

    #[test]
    fn changing_block_topology_changes_compiled_k() {
        let left = compile_topology_to_key(&analyze_topology(&[0xAA; 64]).unwrap()).unwrap();
        let right = compile_topology_to_key(&analyze_topology(&[0xF0; 64]).unwrap()).unwrap();
        assert_ne!(left.serialize().unwrap(), right.serialize().unwrap());
    }
}
