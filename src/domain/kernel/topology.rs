use crate::domain::kernel::key::{MagicKey, SpectralPeakCode, SpectralProgram, MAX_SPECTRAL_PEAKS};
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

    let derivative_fold =
        signature
            .derivative
            .chunks(64)
            .enumerate()
            .fold(0_u64, |state, (chunk_index, chunk)| {
                let word = chunk
                    .iter()
                    .enumerate()
                    .fold(0_u64, |word, (bit, value)| word | ((*value as u64) << bit));
                avalanche(state ^ word.rotate_left((chunk_index % 64) as u32))
            });
    let popcnt_fold = signature
        .popcnt_profile
        .iter()
        .enumerate()
        .fold(0_u64, |state, (index, count)| {
            state ^ (*count as u64).rotate_left((index % 64) as u32)
        });
    let shift_fold =
        signature
            .shift_scores
            .iter()
            .enumerate()
            .fold(0_u64, |state, (rank, score)| {
                state
                    ^ (score.shift as u64).rotate_left((rank * 11) as u32)
                    ^ (score.matching_bits as u64).rotate_right((rank * 7) as u32)
            });
    let peak_fold = signature
        .walsh_peaks
        .iter()
        .enumerate()
        .fold(0_u64, |state, (rank, peak)| {
            let signed_index = (peak.index as u64)
                ^ if peak.coefficient.is_sign_negative() {
                    u64::MAX
                } else {
                    0
                };
            state ^ signed_index.rotate_left((rank * 13) as u32)
        });
    Ok(MagicKey::from_raw(avalanche(
        signature.fingerprint
            ^ derivative_fold.rotate_left(7)
            ^ popcnt_fold.rotate_left(19)
            ^ shift_fold.rotate_left(31)
            ^ peak_fold,
    )))
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
        compile_topology_to_key, popcnt_profile, shift_correlation,
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

        assert_eq!(first.bit_len(), 64);
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
                .min(4)
        );
        assert_eq!(spectral.serialize().len(), 8);
    }

    #[test]
    fn changing_block_topology_changes_compiled_k() {
        let left = compile_topology_to_key(&analyze_topology(&[0xAA; 64]).unwrap()).unwrap();
        let right = compile_topology_to_key(&analyze_topology(&[0xF0; 64]).unwrap()).unwrap();
        assert_ne!(left.serialize(), right.serialize());
    }

    #[test]
    fn every_topology_profile_materially_changes_compiled_k() {
        let signature = analyze_topology(&[0xA5; 64]).unwrap();
        let baseline = compile_topology_to_key(&signature).unwrap().serialize();

        let mut changed = signature.clone();
        changed.walsh_peaks[0].index = (changed.walsh_peaks[0].index + 1) % changed.bit_len;
        assert_ne!(
            compile_topology_to_key(&changed).unwrap().serialize(),
            baseline
        );

        let mut changed = signature.clone();
        changed.shift_scores[0].shift = changed.shift_scores[0].shift % 31 + 1;
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
        changed.popcnt_profile[0] ^= 1;
        assert_ne!(
            compile_topology_to_key(&changed).unwrap().serialize(),
            baseline
        );

        let mut changed = signature;
        changed.fingerprint ^= 1;
        assert_ne!(
            compile_topology_to_key(&changed).unwrap().serialize(),
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
