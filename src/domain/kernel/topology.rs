use crate::domain::kernel::key::{
    BranchSchema, MagicKey, OperatorBlueprint, PipelineKind, ProgramCode, ProgramIR, ProgramStage,
    SpectralPeakCode, SpectralProgram, StageOpcode, WindowClass, MAX_SPECTRAL_PEAKS,
};
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
    if signature.bit_len != 4096 {
        return Err("operator topology compiler currently requires a 4096-bit block".to_string());
    }
    if signature.walsh_peaks.is_empty() || signature.shift_scores.is_empty() {
        return Err("signature lacks required topology features".to_string());
    }

    let dominant = &signature.walsh_peaks[0];
    let second = signature.walsh_peaks.get(1).unwrap_or(dominant);
    let third = signature.walsh_peaks.get(2).unwrap_or(dominant);
    let shift = &signature.shift_scores[0];
    let derivative_density = quantize_ratio(
        signature.derivative.iter().filter(|bit| **bit == 1).count(),
        signature.derivative.len(),
        8,
    ) as u8;
    let popcnt_sum = signature
        .popcnt_profile
        .iter()
        .map(|count| *count as usize)
        .sum::<usize>();
    let popcnt_capacity = signature.popcnt_profile.len().saturating_mul(64);
    let popcnt_density = quantize_ratio(popcnt_sum, popcnt_capacity.max(1), 8) as u8;
    let round_count = (((signature.fingerprint & 0x7) as u8) % 8) + 1;
    let phase_parity = ((signature.fingerprint
        ^ signature
            .derivative
            .iter()
            .map(|bit| *bit as u64)
            .sum::<u64>()
        ^ popcnt_sum as u64)
        & 1)
        == 1;

    MagicKey::from_operator_blueprint(&OperatorBlueprint {
        peak_indices: [
            dominant.index as u16,
            second.index as u16,
            third.index as u16,
        ],
        peak_signs: [
            !dominant.coefficient.is_sign_negative(),
            !second.coefficient.is_sign_negative(),
            !third.coefficient.is_sign_negative(),
        ],
        primary_shift: shift.shift.min(32) as u8,
        round_count,
        derivative_density,
        popcnt_density,
        phase_parity,
    })
}

pub fn compile_topology_to_program(
    signature: &TopologySignature,
    window_bits: usize,
) -> Result<ProgramCode, String> {
    if signature.bit_len != window_bits {
        return Err("signature bit length does not match the selected window".to_string());
    }
    if signature.walsh_peaks.is_empty() || signature.shift_scores.is_empty() {
        return Err("signature lacks required topology features".to_string());
    }
    let derivative_ones = signature.derivative.iter().filter(|bit| **bit == 1).count();
    let derivative_density = derivative_ones as f64 / signature.derivative.len().max(1) as f64;
    let spectral_prominence = signature.walsh_peaks[0].coefficient.abs()
        / signature
            .walsh_peaks
            .iter()
            .map(|peak| peak.coefficient.abs())
            .sum::<f64>()
            .max(1e-12);
    let pipeline_kind = if spectral_prominence > 0.6 {
        PipelineKind::Walsh
    } else if derivative_density > 0.55 {
        PipelineKind::Recurrence
    } else {
        PipelineKind::Hybrid
    };
    let window_class = if spectral_prominence > 0.7 {
        WindowClass::MacroCoherent
    } else if derivative_density > 0.65 {
        WindowClass::LocalTransient
    } else {
        WindowClass::Balanced
    };
    let branch_schema = if derivative_density > 0.6 {
        BranchSchema::ShiftParity
    } else if spectral_prominence > 0.6 {
        BranchSchema::PeakParity
    } else {
        BranchSchema::Mixed
    };
    let top0 = signature.walsh_peaks[0].index as u8;
    let top1 = signature
        .walsh_peaks
        .get(1)
        .map(|peak| peak.index as u8)
        .unwrap_or(top0);
    let shift0 = signature.shift_scores[0].shift as u8;
    let shift1 = signature
        .shift_scores
        .get(1)
        .map(|score| score.shift as u8)
        .unwrap_or(shift0);
    let branch_budget = if window_bits >= 4096 {
        9
    } else if window_bits >= 2048 {
        5
    } else if window_bits >= 1024 {
        3
    } else {
        2
    };

    let mut stages = vec![
        ProgramStage {
            opcode: StageOpcode::HadamardAxis,
            arg0: top0,
            arg1: top1,
        },
        ProgramStage {
            opcode: StageOpcode::GF2Recurrence,
            arg0: shift0.max(1),
            arg1: shift1,
        },
        ProgramStage {
            opcode: StageOpcode::LanePermute,
            arg0: (signature.popcnt_profile.len() as u8).max(1),
            arg1: signature.popcnt_profile.first().copied().unwrap_or(0),
        },
    ];
    for offset in 0..branch_budget {
        stages.push(ProgramStage {
            opcode: StageOpcode::BranchGate,
            arg0: top0.wrapping_add(offset as u8),
            arg1: shift0.wrapping_add(offset as u8),
        });
    }
    stages.push(ProgramStage {
        opcode: StageOpcode::Halt,
        arg0: 0,
        arg1: 0,
    });

    ProgramCode::from_ir(&ProgramIR {
        pipeline_kind,
        window_class,
        branch_schema,
        stages,
    })
}

fn quantize_ratio(numerator: usize, denominator: usize, bits: u8) -> u64 {
    if denominator == 0 {
        return 0;
    }
    let max = (1_u64 << bits) - 1;
    ((numerator as u128 * max as u128 + (denominator as u128 / 2)) / denominator as u128) as u64
}

pub fn compile_spectral_key(signature: &TopologySignature) -> Result<MagicKey, String> {
    if signature.bit_len == 0 || !signature.bit_len.is_power_of_two() {
        return Err("signature bit length must be a power of two".to_string());
    }
    let strongest = signature
        .walsh_peaks
        .iter()
        .map(|peak| peak.coefficient.abs())
        .fold(0.0_f64, f64::max);
    let peaks = signature
        .walsh_peaks
        .iter()
        .filter(|peak| peak.coefficient.abs() > 1e-12)
        .take(MAX_SPECTRAL_PEAKS)
        .map(|peak| SpectralPeakCode {
            index: peak.index,
            positive: !peak.coefficient.is_sign_negative(),
            amplitude: ((peak.coefficient.abs() / strongest * 31.0).round() as u8).max(1),
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
        analyze_topology, block_fingerprint, circular_bit_derivative, compile_spectral_key,
        compile_topology_to_key, popcnt_profile, shift_correlation, MAX_SPECTRAL_PEAKS,
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
        let input = [0xAA_u8; 512];
        let signature = analyze_topology(&input).unwrap();
        let first = compile_topology_to_key(&signature).unwrap();
        let second = compile_topology_to_key(&signature).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.encoded_bit_len(), 64);
        assert_ne!(first.raw(), 0);

        let spectral = compile_spectral_key(&signature).unwrap();
        let program = spectral.spectral_program().unwrap();
        assert_eq!(program.bit_len, signature.bit_len);
        assert_eq!(
            program.peaks.len(),
            signature
                .walsh_peaks
                .iter()
                .filter(|peak| peak.coefficient.abs() > 1e-12)
                .count()
                .min(MAX_SPECTRAL_PEAKS)
        );
        assert_eq!(spectral.serialize().len(), 8);
    }

    #[test]
    fn changing_block_topology_changes_compiled_k() {
        let left = compile_topology_to_key(&analyze_topology(&[0xAA; 512]).unwrap()).unwrap();
        let right = compile_topology_to_key(&analyze_topology(&[0xF0; 512]).unwrap()).unwrap();
        assert_ne!(left.serialize(), right.serialize());
    }

    #[test]
    fn operator_key_exposes_structural_topology_fields() {
        let signature = analyze_topology(&[0xA5; 512]).unwrap();
        let key = compile_topology_to_key(&signature).unwrap();
        let blueprint = key.operator_blueprint().unwrap();
        assert_eq!(
            usize::from(blueprint.peak_indices[0]),
            signature.walsh_peaks[0].index
        );
        assert_eq!(
            blueprint.peak_signs[0],
            !signature.walsh_peaks[0].coefficient.is_sign_negative()
        );
        assert_eq!(
            usize::from(blueprint.primary_shift),
            signature.shift_scores[0].shift.min(32)
        );
    }

    #[test]
    fn every_topology_profile_materially_changes_compiled_k() {
        let signature = analyze_topology(&[0xA5; 512]).unwrap();
        let baseline = compile_topology_to_key(&signature).unwrap().serialize();

        let mut changed = signature.clone();
        changed.walsh_peaks[0].index = (changed.walsh_peaks[0].index + 1) % changed.bit_len;
        assert_ne!(
            compile_topology_to_key(&changed).unwrap().serialize(),
            baseline
        );

        let mut changed = signature.clone();
        changed.shift_scores[0].shift = if changed.shift_scores[0].shift == 32 {
            31
        } else {
            changed.shift_scores[0].shift + 1
        };
        assert_ne!(
            compile_topology_to_key(&changed).unwrap().serialize(),
            baseline
        );

        let mut changed = signature.clone();
        changed.derivative[0] ^= 1;
        assert_ne!(
            compile_topology_to_key(&changed).unwrap().serialize(),
            baseline
        );

        let mut changed = signature.clone();
        changed.popcnt_profile[0] ^= 0x1F;
        assert_ne!(
            compile_topology_to_key(&changed).unwrap().serialize(),
            baseline
        );
    }

    #[test]
    fn fingerprint_is_deterministic_for_the_same_block() {
        let input = [0x42_u8; 512];
        assert_eq!(
            block_fingerprint(&input).unwrap(),
            block_fingerprint(&input).unwrap()
        );
    }
}
