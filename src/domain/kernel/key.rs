pub const MAGIC_KEY_BYTES: usize = 8;
pub const MAX_SPECTRAL_PEAKS: usize = 3;
pub const SPECTRAL_INDEX_BITS: usize = 13;
pub const SPECTRAL_AMPLITUDE_BITS: usize = 5;
pub const MAX_SPECTRAL_BITS: usize = 1 << SPECTRAL_INDEX_BITS;
const NON_SPECTRAL_KIND_SHIFT: u64 = 59;
const NON_SPECTRAL_KIND_MASK: u64 = 0xF << NON_SPECTRAL_KIND_SHIFT;
const TRAJECTORY_KIND: u64 = 0xE;
const OPERATOR_KIND: u64 = 0xF;
const NON_SPECTRAL_PAYLOAD_MASK: u64 = (1_u64 << NON_SPECTRAL_KIND_SHIFT) - 1;
const TRAJECTORY_STEP_BITS: u64 = 6;
const TRAJECTORY_STEP_MASK: u64 = (1_u64 << TRAJECTORY_STEP_BITS) - 1;
const OPERATOR_DOMINANT_INDEX_BITS: u64 = 13;
const OPERATOR_DOMINANT_SIGN_BITS: u64 = 1;
const OPERATOR_PRIMARY_SHIFT_BITS: u64 = 5;
const OPERATOR_SHIFT_MATCH_BITS: u64 = 9;
const OPERATOR_DERIVATIVE_DENSITY_BITS: u64 = 8;
const OPERATOR_POPCNT_DENSITY_BITS: u64 = 8;
const OPERATOR_SECONDARY_DELTA_BITS: u64 = 5;
const OPERATOR_TERTIARY_DELTA_BITS: u64 = 5;
const OPERATOR_FINGERPRINT_BIAS_BITS: u64 = 5;

const OPERATOR_DOMINANT_INDEX_SHIFT: u64 = 0;
const OPERATOR_DOMINANT_SIGN_SHIFT: u64 =
    OPERATOR_DOMINANT_INDEX_SHIFT + OPERATOR_DOMINANT_INDEX_BITS;
const OPERATOR_PRIMARY_SHIFT_SHIFT: u64 =
    OPERATOR_DOMINANT_SIGN_SHIFT + OPERATOR_DOMINANT_SIGN_BITS;
const OPERATOR_SHIFT_MATCH_SHIFT: u64 = OPERATOR_PRIMARY_SHIFT_SHIFT + OPERATOR_PRIMARY_SHIFT_BITS;
const OPERATOR_DERIVATIVE_DENSITY_SHIFT: u64 =
    OPERATOR_SHIFT_MATCH_SHIFT + OPERATOR_SHIFT_MATCH_BITS;
const OPERATOR_POPCNT_DENSITY_SHIFT: u64 =
    OPERATOR_DERIVATIVE_DENSITY_SHIFT + OPERATOR_DERIVATIVE_DENSITY_BITS;
const OPERATOR_SECONDARY_DELTA_SHIFT: u64 =
    OPERATOR_POPCNT_DENSITY_SHIFT + OPERATOR_POPCNT_DENSITY_BITS;
const OPERATOR_TERTIARY_DELTA_SHIFT: u64 =
    OPERATOR_SECONDARY_DELTA_SHIFT + OPERATOR_SECONDARY_DELTA_BITS;
const OPERATOR_FINGERPRINT_BIAS_SHIFT: u64 =
    OPERATOR_TERTIARY_DELTA_SHIFT + OPERATOR_TERTIARY_DELTA_BITS;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MagicKey(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MagicKeyKind {
    Spectral,
    Trajectory,
    Operator,
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
    pub dominant_index: u16,
    pub dominant_positive: bool,
    pub primary_shift: u8,
    pub shift_match: u16,
    pub derivative_density: u8,
    pub popcnt_density: u8,
    pub secondary_delta: u8,
    pub tertiary_delta: u8,
    pub fingerprint_bias: u8,
}

impl MagicKey {
    pub fn from_operator_payload(payload: u64) -> Self {
        Self(with_non_spectral_kind(payload, OPERATOR_KIND))
    }

    pub fn from_trajectory_payload(payload: u64, steps: u8) -> Result<Self, String> {
        if !(1..64).contains(&steps) {
            return Err("trajectory step count must be in 1..64".to_string());
        }
        let payload = (payload & !TRAJECTORY_STEP_MASK) | steps as u64;
        Ok(Self(with_non_spectral_kind(payload, TRAJECTORY_KIND)))
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn payload(self) -> u64 {
        self.0 & NON_SPECTRAL_PAYLOAD_MASK
    }

    pub fn kind(self) -> Result<MagicKeyKind, String> {
        let discriminator = (self.0 & NON_SPECTRAL_KIND_MASK) >> NON_SPECTRAL_KIND_SHIFT;
        match discriminator {
            TRAJECTORY_KIND => Ok(MagicKeyKind::Trajectory),
            OPERATOR_KIND => Ok(MagicKeyKind::Operator),
            value if (1..=SPECTRAL_INDEX_BITS as u64).contains(&value) => {
                Ok(MagicKeyKind::Spectral)
            }
            _ => Err("magic key contains an unsupported kind discriminator".to_string()),
        }
    }

    pub fn require_kind(self, expected: MagicKeyKind) -> Result<Self, String> {
        let actual = self.kind()?;
        if actual != expected {
            return Err(format!(
                "magic key kind mismatch: expected {expected:?}, got {actual:?}"
            ));
        }
        Ok(self)
    }

    pub fn serialize(self) -> [u8; MAGIC_KEY_BYTES] {
        self.0.to_le_bytes()
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let encoded: [u8; MAGIC_KEY_BYTES] = bytes
            .try_into()
            .map_err(|_| "K must be exactly one 64-bit magic constant".to_string())?;
        Ok(Self(u64::from_le_bytes(encoded)))
    }

    pub fn encoded_bit_len(self) -> usize {
        u64::BITS as usize
    }

    pub fn trajectory_steps(self) -> Result<u8, String> {
        self.require_kind(MagicKeyKind::Trajectory)?;
        let steps = (self.payload() & TRAJECTORY_STEP_MASK) as u8;
        if !(1..64).contains(&steps) {
            return Err("trajectory K stores an invalid step count".to_string());
        }
        Ok(steps)
    }

    pub fn from_operator_blueprint(blueprint: &OperatorBlueprint) -> Result<Self, String> {
        validate_operator_blueprint(blueprint)?;
        let payload = (u64::from(blueprint.dominant_index) << OPERATOR_DOMINANT_INDEX_SHIFT)
            | (u64::from(blueprint.dominant_positive) << OPERATOR_DOMINANT_SIGN_SHIFT)
            | (u64::from(blueprint.primary_shift - 1) << OPERATOR_PRIMARY_SHIFT_SHIFT)
            | (u64::from(blueprint.shift_match) << OPERATOR_SHIFT_MATCH_SHIFT)
            | (u64::from(blueprint.derivative_density) << OPERATOR_DERIVATIVE_DENSITY_SHIFT)
            | (u64::from(blueprint.popcnt_density) << OPERATOR_POPCNT_DENSITY_SHIFT)
            | (u64::from(blueprint.secondary_delta) << OPERATOR_SECONDARY_DELTA_SHIFT)
            | (u64::from(blueprint.tertiary_delta) << OPERATOR_TERTIARY_DELTA_SHIFT)
            | (u64::from(blueprint.fingerprint_bias) << OPERATOR_FINGERPRINT_BIAS_SHIFT);
        Ok(Self(with_non_spectral_kind(payload, OPERATOR_KIND)))
    }

    pub fn operator_blueprint(self) -> Result<OperatorBlueprint, String> {
        self.require_kind(MagicKeyKind::Operator)?;
        let payload = self.payload();
        let blueprint = OperatorBlueprint {
            dominant_index: field_u16(
                payload,
                OPERATOR_DOMINANT_INDEX_SHIFT,
                OPERATOR_DOMINANT_INDEX_BITS,
            ),
            dominant_positive: field_u8(
                payload,
                OPERATOR_DOMINANT_SIGN_SHIFT,
                OPERATOR_DOMINANT_SIGN_BITS,
            ) == 1,
            primary_shift: field_u8(
                payload,
                OPERATOR_PRIMARY_SHIFT_SHIFT,
                OPERATOR_PRIMARY_SHIFT_BITS,
            ) + 1,
            shift_match: field_u16(
                payload,
                OPERATOR_SHIFT_MATCH_SHIFT,
                OPERATOR_SHIFT_MATCH_BITS,
            ),
            derivative_density: field_u8(
                payload,
                OPERATOR_DERIVATIVE_DENSITY_SHIFT,
                OPERATOR_DERIVATIVE_DENSITY_BITS,
            ),
            popcnt_density: field_u8(
                payload,
                OPERATOR_POPCNT_DENSITY_SHIFT,
                OPERATOR_POPCNT_DENSITY_BITS,
            ),
            secondary_delta: field_u8(
                payload,
                OPERATOR_SECONDARY_DELTA_SHIFT,
                OPERATOR_SECONDARY_DELTA_BITS,
            ),
            tertiary_delta: field_u8(
                payload,
                OPERATOR_TERTIARY_DELTA_SHIFT,
                OPERATOR_TERTIARY_DELTA_BITS,
            ),
            fingerprint_bias: field_u8(
                payload,
                OPERATOR_FINGERPRINT_BIAS_SHIFT,
                OPERATOR_FINGERPRINT_BIAS_BITS,
            ),
        };
        validate_operator_blueprint(&blueprint)?;
        Ok(blueprint)
    }

    pub fn from_spectral_program(program: &SpectralProgram) -> Result<Self, String> {
        validate_spectral_program(program)?;
        let block_log2 = program.bit_len.ilog2() as u64;
        let mut value = 0_u64;
        for (slot, peak) in program.peaks.iter().enumerate() {
            value |= (peak.index as u64) << (slot * SPECTRAL_INDEX_BITS);
            value |= (peak.amplitude as u64) << (39 + slot * SPECTRAL_AMPLITUDE_BITS);
            value |= u64::from(peak.positive) << (54 + slot);
        }
        value |= ((program.peaks.len() - 1) as u64) << 57;
        value |= block_log2 << 59;
        value |= u64::from(program.tie_bit) << 63;
        Ok(Self(value))
    }

    pub fn spectral_program(self) -> Result<SpectralProgram, String> {
        self.require_kind(MagicKeyKind::Spectral)?;
        let block_log2 = ((self.0 >> 59) & 0xF) as u32;
        if !(1..=SPECTRAL_INDEX_BITS as u32).contains(&block_log2) {
            return Err("spectral K contains an unsupported block size".to_string());
        }
        let bit_len = 1_usize << block_log2;
        let peak_count = (((self.0 >> 57) & 0x3) as usize) + 1;
        if peak_count > MAX_SPECTRAL_PEAKS {
            return Err("spectral K contains too many peaks".to_string());
        }
        let index_mask = (1_u64 << SPECTRAL_INDEX_BITS) - 1;
        let peaks = (0..peak_count)
            .map(|slot| SpectralPeakCode {
                index: ((self.0 >> (slot * SPECTRAL_INDEX_BITS)) & index_mask) as usize,
                positive: ((self.0 >> (54 + slot)) & 1) == 1,
                amplitude: ((self.0 >> (39 + slot * SPECTRAL_AMPLITUDE_BITS)) & 0x1F) as u8,
            })
            .collect::<Vec<_>>();
        let program = SpectralProgram {
            bit_len,
            peaks,
            tie_bit: (self.0 >> 63) == 1,
        };
        validate_spectral_program(&program)?;
        Ok(program)
    }
}

fn with_non_spectral_kind(payload: u64, kind: u64) -> u64 {
    (payload & NON_SPECTRAL_PAYLOAD_MASK) | (kind << NON_SPECTRAL_KIND_SHIFT)
}

fn field_mask(bits: u64) -> u64 {
    (1_u64 << bits) - 1
}

fn field_u8(payload: u64, shift: u64, bits: u64) -> u8 {
    ((payload >> shift) & field_mask(bits)) as u8
}

fn field_u16(payload: u64, shift: u64, bits: u64) -> u16 {
    ((payload >> shift) & field_mask(bits)) as u16
}

fn validate_spectral_program(program: &SpectralProgram) -> Result<(), String> {
    if program.bit_len < 2
        || program.bit_len > MAX_SPECTRAL_BITS
        || !program.bit_len.is_power_of_two()
    {
        return Err("spectral program bit length is unsupported".to_string());
    }
    if program.peaks.is_empty() || program.peaks.len() > MAX_SPECTRAL_PEAKS {
        return Err("spectral program must contain one to three peaks".to_string());
    }
    if program
        .peaks
        .iter()
        .any(|peak| peak.index >= program.bit_len)
    {
        return Err("spectral peak index is outside the block".to_string());
    }
    if program.peaks.iter().any(|peak| peak.amplitude == 0) {
        return Err("spectral peak amplitude must be non-zero".to_string());
    }
    Ok(())
}

fn validate_operator_blueprint(blueprint: &OperatorBlueprint) -> Result<(), String> {
    if blueprint.primary_shift == 0 || blueprint.primary_shift > 32 {
        return Err("operator blueprint primary shift must be in 1..=32".to_string());
    }
    if usize::from(blueprint.dominant_index) >= MAX_SPECTRAL_BITS {
        return Err("operator blueprint dominant index is outside the supported range".to_string());
    }
    if u64::from(blueprint.shift_match) > field_mask(OPERATOR_SHIFT_MATCH_BITS) {
        return Err("operator blueprint shift match is outside the supported range".to_string());
    }
    if u64::from(blueprint.secondary_delta) > field_mask(OPERATOR_SECONDARY_DELTA_BITS) {
        return Err(
            "operator blueprint secondary delta is outside the supported range".to_string(),
        );
    }
    if u64::from(blueprint.tertiary_delta) > field_mask(OPERATOR_TERTIARY_DELTA_BITS) {
        return Err("operator blueprint tertiary delta is outside the supported range".to_string());
    }
    if u64::from(blueprint.fingerprint_bias) > field_mask(OPERATOR_FINGERPRINT_BIAS_BITS) {
        return Err(
            "operator blueprint fingerprint bias is outside the supported range".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MagicKey, MagicKeyKind, OperatorBlueprint, SpectralPeakCode, SpectralProgram,
        MAGIC_KEY_BYTES,
    };

    fn sample_program() -> SpectralProgram {
        SpectralProgram {
            bit_len: 4096,
            peaks: vec![
                SpectralPeakCode {
                    index: 17,
                    positive: true,
                    amplitude: 31,
                },
                SpectralPeakCode {
                    index: 2047,
                    positive: false,
                    amplitude: 19,
                },
                SpectralPeakCode {
                    index: 4095,
                    positive: true,
                    amplitude: 7,
                },
            ],
            tie_bit: false,
        }
    }

    #[test]
    fn k_is_exactly_one_64_bit_magic_constant() {
        let key = MagicKey::from_spectral_program(&sample_program()).unwrap();
        assert_eq!(key.encoded_bit_len(), 64);
        assert_eq!(key.serialize().len(), MAGIC_KEY_BYTES);
    }

    #[test]
    fn magic_constant_roundtrips_the_complete_spectral_program() {
        let program = sample_program();
        let key = MagicKey::from_spectral_program(&program).unwrap();
        assert_eq!(MagicKey::parse(&key.serialize()).unwrap(), key);
        assert_eq!(key.spectral_program().unwrap(), program);
    }

    #[test]
    fn malformed_or_non_64_bit_k_is_rejected() {
        assert!(MagicKey::parse(&[0; 7]).is_err());
        assert!(MagicKey::parse(&[0; 9]).is_err());
        assert!(MagicKey::parse(&0_u64.to_le_bytes())
            .unwrap()
            .spectral_program()
            .is_err());
    }

    #[test]
    fn non_spectral_keys_are_tagged_and_reject_wrong_interpretation() {
        let operator = MagicKey::from_operator_payload(0x1234);
        assert_eq!(operator.kind().unwrap(), MagicKeyKind::Operator);
        assert!(operator.spectral_program().is_err());

        let trajectory = MagicKey::from_trajectory_payload(0xABCD, 12).unwrap();
        assert_eq!(trajectory.kind().unwrap(), MagicKeyKind::Trajectory);
        assert_eq!(trajectory.trajectory_steps().unwrap(), 12);
        assert!(trajectory.require_kind(MagicKeyKind::Operator).is_err());
    }

    #[test]
    fn operator_blueprint_roundtrips_as_a_typed_magic_key() {
        let blueprint = OperatorBlueprint {
            dominant_index: 4095,
            dominant_positive: false,
            primary_shift: 17,
            shift_match: 301,
            derivative_density: 211,
            popcnt_density: 93,
            secondary_delta: 12,
            tertiary_delta: 27,
            fingerprint_bias: 19,
        };
        let key = MagicKey::from_operator_blueprint(&blueprint).unwrap();
        assert_eq!(key.kind().unwrap(), MagicKeyKind::Operator);
        assert_eq!(key.operator_blueprint().unwrap(), blueprint);
        assert!(key.spectral_program().is_err());
    }
}
