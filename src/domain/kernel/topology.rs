use crate::domain::kernel::base::{generate_base_from_root, standard_base, PatternId, RootSeed};
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
    pub fingerprint: u64,
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
    let fingerprint = block_fingerprint(bytes)?;

    Ok(TopologySignature {
        bit_len: bits.len(),
        walsh_peaks,
        shift_scores,
        derivative,
        popcnt_profile,
        fingerprint,
    })
}

pub fn compile_topology_to_key(signature: &TopologySignature) -> Result<MagicKey, String> {
    if signature.bit_len == 0 || !signature.bit_len.is_power_of_two() {
        return Err("signature bit length must be a power of two".to_string());
    }
    if signature.walsh_peaks.is_empty() || signature.shift_scores.is_empty() {
        return Err("signature lacks required topology features".to_string());
    }

    let strongest_shift = signature.shift_scores[0].shift;
    let secondary_shift = signature
        .shift_scores
        .get(1)
        .map(|score| score.shift)
        .unwrap_or(strongest_shift);
    let derivative_ones = signature
        .derivative
        .iter()
        .map(|bit| *bit as usize)
        .sum::<usize>();
    let popcnt_sum = signature
        .popcnt_profile
        .iter()
        .map(|count| *count as usize)
        .sum::<usize>();
    let peak_selector = signature
        .walsh_peaks
        .iter()
        .enumerate()
        .fold(0_usize, |acc, (rank, peak)| {
            acc ^ peak.index.rotate_left((rank % usize::BITS as usize) as u32)
        });
    let multipliers = [
        0x9E37_79B9_7F4A_7C15_u64,
        0xBF58_476D_1CE4_E5B9,
        0x94D0_49BB_1331_11EB,
        0xD6E8_FEB8_6659_FD93,
    ];
    let odd_multiplier =
        multipliers[(peak_selector ^ derivative_ones ^ popcnt_sum) % multipliers.len()];
    let phase_mask = signature.walsh_peaks.iter().fold(0_u64, |mask, peak| {
        let bit = 1_u64 << (peak.index % 64);
        if peak.coefficient.is_sign_negative() {
            mask ^ bit.rotate_left(1)
        } else {
            mask ^ bit
        }
    }) ^ (derivative_ones as u64).rotate_left(17);
    let program_root = signature.fingerprint
        ^ (popcnt_sum as u64).rotate_left(11)
        ^ (derivative_ones as u64).rotate_left(29)
        ^ phase_mask;
    let generated = generate_base_from_root(
        &standard_base(1)?,
        RootSeed {
            value: program_root,
        },
    )?;
    let program = generated
        .operation_schedule
        .iter()
        .map(|operation| *operation as u8)
        .collect::<Vec<_>>();

    let key = MagicKey {
        header: KeyHeader {
            version: 1,
            main_pattern_id: PatternId::SpectralInvolution,
            rounds: program.len() as u8,
            block_log2: signature.bit_len.ilog2() as u8,
            flags: 0,
        },
        segments: vec![
            KeySegment {
                kind: SegmentKind::RevMix,
                payload: vec![
                    (signature.walsh_peaks[0].index % 64) as u8,
                    (strongest_shift % 64) as u8,
                ],
            },
            KeySegment {
                kind: SegmentKind::PhaseMask,
                payload: phase_mask.to_le_bytes().to_vec(),
            },
            KeySegment {
                kind: SegmentKind::WalshConfig,
                payload: {
                    let mut payload = vec![
                        (strongest_shift % 31 + 1) as u8,
                        (secondary_shift % 31 + 1) as u8,
                    ];
                    payload.extend_from_slice(&odd_multiplier.to_le_bytes());
                    payload
                },
            },
            KeySegment {
                kind: SegmentKind::Program,
                payload: program,
            },
            KeySegment {
                kind: SegmentKind::AuxConst,
                payload: signature.fingerprint.to_le_bytes().to_vec(),
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

fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use crate::domain::kernel::base::OperationCode;
    use crate::domain::kernel::key::SegmentKind;

    use super::{
        analyze_topology, block_fingerprint, circular_bit_derivative, compile_topology_to_key,
        popcnt_profile, shift_correlation,
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
        assert_eq!(rev_mix.payload.len(), 2);

        let walsh = first
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::WalshConfig)
            .unwrap();
        assert_eq!(walsh.payload.len(), 10);
        let multiplier = u64::from_le_bytes(walsh.payload[2..10].try_into().unwrap());
        assert_eq!(multiplier & 1, 1);

        let phase = first
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::PhaseMask)
            .unwrap();
        let phase_mask = u64::from_le_bytes(phase.payload.clone().try_into().unwrap());
        assert_ne!(phase_mask, 0);

        let program = first
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::Program)
            .unwrap();
        assert_eq!(program.payload.len(), 8);
        assert!(program
            .payload
            .iter()
            .all(|code| OperationCode::try_from(*code).is_ok()));

        let aux = first
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::AuxConst)
            .unwrap();
        assert_eq!(
            u64::from_le_bytes(aux.payload.as_slice().try_into().unwrap()),
            signature.fingerprint
        );
    }

    #[test]
    fn changing_block_topology_changes_compiled_k() {
        let left = compile_topology_to_key(&analyze_topology(&[0xAA; 64]).unwrap()).unwrap();
        let right = compile_topology_to_key(&analyze_topology(&[0xF0; 64]).unwrap()).unwrap();
        assert_ne!(left.serialize().unwrap(), right.serialize().unwrap());
    }

    #[test]
    fn every_topology_profile_materially_changes_compiled_k() {
        let signature = analyze_topology(&[0xA5; 64]).unwrap();
        let baseline = compile_topology_to_key(&signature)
            .unwrap()
            .serialize()
            .unwrap();

        let mut changed = signature.clone();
        changed.walsh_peaks[0].index = (changed.walsh_peaks[0].index + 1) % changed.bit_len;
        assert_ne!(
            compile_topology_to_key(&changed)
                .unwrap()
                .serialize()
                .unwrap(),
            baseline
        );

        let mut changed = signature.clone();
        changed.shift_scores[0].shift = changed.shift_scores[0].shift % 31 + 1;
        assert_ne!(
            compile_topology_to_key(&changed)
                .unwrap()
                .serialize()
                .unwrap(),
            baseline
        );

        let mut changed = signature.clone();
        changed.derivative[0] ^= 1;
        assert_ne!(
            compile_topology_to_key(&changed)
                .unwrap()
                .serialize()
                .unwrap(),
            baseline
        );

        let mut changed = signature.clone();
        changed.popcnt_profile[0] ^= 1;
        assert_ne!(
            compile_topology_to_key(&changed)
                .unwrap()
                .serialize()
                .unwrap(),
            baseline
        );

        let mut changed = signature;
        changed.fingerprint ^= 1;
        assert_ne!(
            compile_topology_to_key(&changed)
                .unwrap()
                .serialize()
                .unwrap(),
            baseline
        );
    }

    #[test]
    fn block_fingerprint_depends_on_the_entire_block() {
        let mut left = vec![0xAA; 512];
        let mut right = left.clone();
        right[511] ^= 1;
        assert_ne!(
            block_fingerprint(&left).unwrap(),
            block_fingerprint(&right).unwrap()
        );

        left[0] ^= 1;
        assert_ne!(
            block_fingerprint(&left).unwrap(),
            block_fingerprint(&right).unwrap()
        );
    }
}
