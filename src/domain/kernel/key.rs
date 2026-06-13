pub const MAGIC_KEY_BYTES: usize = 8;
pub const MAX_SPECTRAL_PEAKS: usize = 3;
pub const MAX_SPECTRAL_BITS: usize = 1 << 13;
pub const MAX_OPERATOR_BITS: usize = 1 << 12;

const SPECTRAL_INDEX_BITS: u64 = 13;
const SPECTRAL_AMPLITUDE_BITS: u64 = 5;
const OPERATOR_INDEX_BITS: u64 = 12;
const TRAJECTORY_STEP_BITS: u64 = 6;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MagicKeyKind {
    Spectral,
    Trajectory,
    Operator,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MagicKey {
    raw: u64,
    kind: Option<MagicKeyKind>,
}

impl MagicKey {
    pub fn from_spectral_program(program: &SpectralProgram) -> Result<Self, String> {
        Ok(Self {
            raw: SpectralKey::from_program(program)?.raw(),
            kind: Some(MagicKeyKind::Spectral),
        })
    }

    pub fn from_operator_blueprint(blueprint: &OperatorBlueprint) -> Result<Self, String> {
        Ok(Self {
            raw: OperatorKey::from_blueprint(blueprint)?.raw(),
            kind: Some(MagicKeyKind::Operator),
        })
    }

    pub fn from_trajectory_payload(fingerprint: u64, steps: u8) -> Result<Self, String> {
        Ok(Self {
            raw: TrajectoryKey::from_payload(fingerprint, steps)?.raw(),
            kind: Some(MagicKeyKind::Trajectory),
        })
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        Ok(Self {
            raw: parse_u64(bytes)?,
            kind: None,
        })
    }

    pub fn require_kind(mut self, expected: MagicKeyKind) -> Result<Self, String> {
        self.kind = Some(expected);
        Ok(self)
    }

    pub fn kind(self) -> Result<MagicKeyKind, String> {
        self.kind
            .ok_or_else(|| "magic key kind is unknown until the block mode is supplied".to_string())
    }

    pub fn spectral_program(self) -> Result<SpectralProgram, String> {
        SpectralKey(self.raw).program()
    }

    pub fn operator_blueprint(self) -> Result<OperatorBlueprint, String> {
        OperatorKey(self.raw).blueprint()
    }

    pub fn trajectory_steps(self) -> Result<u8, String> {
        TrajectoryKey(self.raw).steps()
    }

    pub fn serialize(self) -> [u8; MAGIC_KEY_BYTES] {
        self.raw.to_le_bytes()
    }

    pub fn raw(self) -> u64 {
        self.raw
    }

    pub fn encoded_bit_len(self) -> usize {
        64
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpectralPeakCode {
    pub index: usize,
    pub positive: bool,
    pub amplitude: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpectralProgram {
    pub bit_len: usize,
    pub peaks: Vec<SpectralPeakCode>,
    pub tie_bit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperatorBlueprint {
    pub peak_indices: [u16; 3],
    pub peak_signs: [bool; 3],
    pub primary_shift: u8,
    pub round_count: u8,
    pub derivative_density: u8,
    pub popcnt_density: u8,
    pub phase_parity: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SpectralKey(u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OperatorKey(u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TrajectoryKey(u64);

impl SpectralKey {
    pub fn from_program(program: &SpectralProgram) -> Result<Self, String> {
        validate_spectral_program(program)?;
        let mut raw = 0_u64;
        for (slot, peak) in program.peaks.iter().enumerate() {
            raw |= (peak.index as u64) << (slot as u64 * SPECTRAL_INDEX_BITS);
            raw |= (peak.amplitude as u64) << (39 + slot as u64 * SPECTRAL_AMPLITUDE_BITS);
            raw |= u64::from(peak.positive) << (54 + slot as u64);
        }
        raw |= ((program.peaks.len() - 1) as u64) << 57;
        raw |= (program.bit_len.ilog2() as u64) << 59;
        raw |= u64::from(program.tie_bit) << 63;
        Ok(Self(raw))
    }

    pub fn program(self) -> Result<SpectralProgram, String> {
        let block_log2 = ((self.0 >> 59) & 0xF) as u32;
        if !(1..=13).contains(&block_log2) {
            return Err("spectral K contains an unsupported block size".to_string());
        }
        let peak_count = (((self.0 >> 57) & 0x3) as usize) + 1;
        if peak_count > MAX_SPECTRAL_PEAKS {
            return Err("spectral K contains too many peaks".to_string());
        }
        let index_mask = (1_u64 << SPECTRAL_INDEX_BITS) - 1;
        let peaks = (0..peak_count)
            .map(|slot| SpectralPeakCode {
                index: ((self.0 >> (slot as u64 * SPECTRAL_INDEX_BITS)) & index_mask) as usize,
                positive: ((self.0 >> (54 + slot as u64)) & 1) == 1,
                amplitude: ((self.0 >> (39 + slot as u64 * SPECTRAL_AMPLITUDE_BITS)) & 0x1F) as u8,
            })
            .collect::<Vec<_>>();
        let program = SpectralProgram {
            bit_len: 1_usize << block_log2,
            peaks,
            tie_bit: (self.0 >> 63) == 1,
        };
        validate_spectral_program(&program)?;
        Ok(program)
    }

    pub fn serialize(self) -> [u8; MAGIC_KEY_BYTES] {
        self.0.to_le_bytes()
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        Ok(Self(parse_u64(bytes)?))
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn encoded_bit_len(self) -> usize {
        64
    }
}

impl OperatorKey {
    pub fn from_blueprint(blueprint: &OperatorBlueprint) -> Result<Self, String> {
        validate_operator_blueprint(blueprint)?;
        let mut raw = 0_u64;
        for (slot, index) in blueprint.peak_indices.iter().enumerate() {
            raw |= (u64::from(*index)) << (slot as u64 * OPERATOR_INDEX_BITS);
            raw |= u64::from(blueprint.peak_signs[slot]) << (36 + slot as u64);
        }
        raw |= u64::from(blueprint.primary_shift - 1) << 39;
        raw |= u64::from(blueprint.round_count - 1) << 44;
        raw |= u64::from(blueprint.derivative_density) << 47;
        raw |= u64::from(blueprint.popcnt_density) << 55;
        raw |= u64::from(blueprint.phase_parity) << 63;
        Ok(Self(raw))
    }

    pub fn blueprint(self) -> Result<OperatorBlueprint, String> {
        let index_mask = (1_u64 << OPERATOR_INDEX_BITS) - 1;
        let blueprint = OperatorBlueprint {
            peak_indices: [
                (self.0 & index_mask) as u16,
                ((self.0 >> 12) & index_mask) as u16,
                ((self.0 >> 24) & index_mask) as u16,
            ],
            peak_signs: [
                ((self.0 >> 36) & 1) == 1,
                ((self.0 >> 37) & 1) == 1,
                ((self.0 >> 38) & 1) == 1,
            ],
            primary_shift: (((self.0 >> 39) & 0x1F) as u8) + 1,
            round_count: (((self.0 >> 44) & 0x7) as u8) + 1,
            derivative_density: ((self.0 >> 47) & 0xFF) as u8,
            popcnt_density: ((self.0 >> 55) & 0xFF) as u8,
            phase_parity: ((self.0 >> 63) & 1) == 1,
        };
        validate_operator_blueprint(&blueprint)?;
        Ok(blueprint)
    }

    pub fn serialize(self) -> [u8; MAGIC_KEY_BYTES] {
        self.0.to_le_bytes()
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let key = Self(parse_u64(bytes)?);
        key.blueprint()?;
        Ok(key)
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn encoded_bit_len(self) -> usize {
        64
    }
}

impl TrajectoryKey {
    pub fn from_payload(fingerprint: u64, steps: u8) -> Result<Self, String> {
        if !(1..64).contains(&steps) {
            return Err("trajectory step count must be in 1..64".to_string());
        }
        let raw = (fingerprint & ((1_u64 << 58) - 1)) | (u64::from(steps) << 58);
        Ok(Self(raw))
    }

    pub fn payload(self) -> u64 {
        self.0 & ((1_u64 << 58) - 1)
    }

    pub fn steps(self) -> Result<u8, String> {
        let steps = ((self.0 >> 58) & ((1_u64 << TRAJECTORY_STEP_BITS) - 1)) as u8;
        if !(1..64).contains(&steps) {
            return Err("trajectory K stores an invalid step count".to_string());
        }
        Ok(steps)
    }

    pub fn serialize(self) -> [u8; MAGIC_KEY_BYTES] {
        self.0.to_le_bytes()
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let key = Self(parse_u64(bytes)?);
        key.steps()?;
        Ok(key)
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn encoded_bit_len(self) -> usize {
        64
    }
}

fn parse_u64(bytes: &[u8]) -> Result<u64, String> {
    let encoded: [u8; MAGIC_KEY_BYTES] = bytes
        .try_into()
        .map_err(|_| "K must be exactly one 64-bit magic constant".to_string())?;
    Ok(u64::from_le_bytes(encoded))
}

fn validate_spectral_program(program: &SpectralProgram) -> Result<(), String> {
    if program.bit_len == 0 || !program.bit_len.is_power_of_two() {
        return Err("spectral bit_len must be a non-zero power of two".to_string());
    }
    if !(2..=MAX_SPECTRAL_BITS).contains(&program.bit_len) {
        return Err("spectral bit_len is outside the supported range".to_string());
    }
    if program.peaks.is_empty() || program.peaks.len() > MAX_SPECTRAL_PEAKS {
        return Err("spectral program must contain between 1 and 3 peaks".to_string());
    }
    for peak in &program.peaks {
        if peak.index >= program.bit_len {
            return Err("spectral peak index is outside the block".to_string());
        }
        if peak.amplitude == 0 || peak.amplitude > 31 {
            return Err("spectral peak amplitude must be in 1..=31".to_string());
        }
    }
    Ok(())
}

fn validate_operator_blueprint(blueprint: &OperatorBlueprint) -> Result<(), String> {
    for index in blueprint.peak_indices {
        if usize::from(index) >= MAX_OPERATOR_BITS {
            return Err("operator peak index is outside the 4096-bit block".to_string());
        }
    }
    if !(1..=32).contains(&blueprint.primary_shift) {
        return Err("operator primary shift must be in 1..=32".to_string());
    }
    if !(1..=8).contains(&blueprint.round_count) {
        return Err("operator round count must be in 1..=8".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_operator_blueprint() -> OperatorBlueprint {
        OperatorBlueprint {
            peak_indices: [17, 511, 4095],
            peak_signs: [true, false, true],
            primary_shift: 5,
            round_count: 3,
            derivative_density: 0xAB,
            popcnt_density: 0xCD,
            phase_parity: true,
        }
    }

    #[test]
    fn operator_blueprint_roundtrips_through_typed_key() {
        let bp = sample_operator_blueprint();
        let key = OperatorKey::from_blueprint(&bp).unwrap();
        assert_eq!(key.blueprint().unwrap(), bp);
        assert_eq!(key.serialize().len(), 8);
        assert_eq!(key.encoded_bit_len(), 64);
    }

    #[test]
    fn spectral_program_roundtrips_through_typed_key() {
        let program = SpectralProgram {
            bit_len: 4096,
            peaks: vec![
                SpectralPeakCode {
                    index: 17,
                    positive: true,
                    amplitude: 31,
                },
                SpectralPeakCode {
                    index: 513,
                    positive: false,
                    amplitude: 7,
                },
                SpectralPeakCode {
                    index: 1023,
                    positive: true,
                    amplitude: 1,
                },
            ],
            tie_bit: true,
        };
        let key = SpectralKey::from_program(&program).unwrap();
        assert_eq!(key.program().unwrap(), program);
        assert_eq!(key.serialize().len(), 8);
    }

    #[test]
    fn trajectory_key_roundtrips_payload_and_steps() {
        let key = TrajectoryKey::from_payload(0x0123_4567_89AB_CDEF, 17).unwrap();
        assert_eq!(key.steps().unwrap(), 17);
        assert_eq!(key.payload(), 0x0123_4567_89AB_CDEF & ((1_u64 << 58) - 1));
    }

    #[test]
    fn malformed_or_non_64_bit_k_is_rejected() {
        assert!(SpectralKey::parse(&[0_u8; 7]).is_err());
        assert!(OperatorKey::parse(&[0_u8; 7]).is_err());
        assert!(TrajectoryKey::parse(&[0_u8; 7]).is_err());
    }

    #[test]
    fn zero_amplitude_spectral_program_is_rejected() {
        let program = SpectralProgram {
            bit_len: 512,
            peaks: vec![SpectralPeakCode {
                index: 0,
                positive: true,
                amplitude: 0,
            }],
            tie_bit: false,
        };
        assert!(SpectralKey::from_program(&program).is_err());
    }

    #[test]
    fn operator_index_above_4095_is_rejected() {
        let mut bp = sample_operator_blueprint();
        bp.peak_indices[0] = 4096;
        assert!(OperatorKey::from_blueprint(&bp).is_err());
    }
}
