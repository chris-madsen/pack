pub const MAGIC_KEY_BYTES: usize = 8;
pub const MAX_SPECTRAL_PEAKS: usize = 3;
pub const MAX_SPECTRAL_BITS: usize = 1 << 13;

const SPECTRAL_INDEX_BITS: u64 = 13;
const SPECTRAL_AMPLITUDE_BITS: u64 = 5;
const TRAJECTORY_STEP_BITS: u64 = 6;
const PACKED_CONSTANT_VERSION: u8 = 2;
const PACKED_CONSTANT_FIXED_BYTES: usize = 35;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstantFamily {
    PhaseXor = 0,
    OddAffine = 1,
    Hybrid = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingKind {
    Identity = 0,
    RotateWords = 1,
    ReverseWords = 2,
    Butterfly = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConstantLayout {
    pub family: ConstantFamily,
    pub routing: RoutingKind,
    pub branch_rounds: u8,
    pub rotate_left: u8,
    pub rotate_right: u8,
    pub gf2_shift: u8,
    pub lane_rotate: u8,
    pub pivot_seed: u8,
    pub branch_stride: u8,
    pub branch_span: u8,
    pub phase_mask: u64,
    pub odd_multiplier: u64,
    pub dominant_walsh_mask: u64,
    pub dominant_walsh_bias: bool,
    pub affine_mask: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackedConstantK {
    bytes: Vec<u8>,
}

impl PackedConstantK {
    pub fn from_layout(layout: &ConstantLayout) -> Result<Self, String> {
        validate_constant_layout(layout)?;
        let mut bytes = Vec::with_capacity(PACKED_CONSTANT_FIXED_BYTES + 8);
        bytes.push(PACKED_CONSTANT_VERSION);
        bytes.push((layout.family as u8) | ((layout.routing as u8) << 2));
        bytes.push(layout.branch_rounds);
        bytes.push(layout.rotate_left);
        bytes.push(layout.rotate_right);
        bytes.push(layout.gf2_shift);
        bytes.push(layout.lane_rotate);
        bytes.push(layout.pivot_seed);
        bytes.push(layout.branch_stride);
        bytes.push(layout.branch_span);
        bytes.extend_from_slice(&layout.phase_mask.to_le_bytes());
        bytes.extend_from_slice(&layout.odd_multiplier.to_le_bytes());
        bytes.extend_from_slice(&layout.dominant_walsh_mask.to_le_bytes());
        bytes.push(u8::from(layout.dominant_walsh_bias));
        if let Some(mask) = layout.affine_mask {
            bytes.extend_from_slice(&mask.to_le_bytes());
        }
        Ok(Self { bytes })
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let code = Self {
            bytes: bytes.to_vec(),
        };
        code.layout()?;
        Ok(code)
    }

    pub fn layout(&self) -> Result<ConstantLayout, String> {
        if self.bytes.len() < PACKED_CONSTANT_FIXED_BYTES {
            return Err("packed constant is shorter than the fixed header".to_string());
        }
        if self.bytes[0] != PACKED_CONSTANT_VERSION {
            return Err("unsupported packed constant version".to_string());
        }
        let family = parse_constant_family(self.bytes[1] & 0b11)?;
        let routing = parse_routing_kind((self.bytes[1] >> 2) & 0b11)?;
        let affine_mask = match family {
            ConstantFamily::PhaseXor => {
                if self.bytes.len() != PACKED_CONSTANT_FIXED_BYTES {
                    return Err(
                        "phase-xor packed constant must not contain an affine payload".to_string(),
                    );
                }
                None
            }
            ConstantFamily::OddAffine | ConstantFamily::Hybrid => {
                if self.bytes.len() != PACKED_CONSTANT_FIXED_BYTES + 8 {
                    return Err(
                        "affine packed constant must contain exactly one affine payload"
                            .to_string(),
                    );
                }
                Some(parse_u64(&self.bytes[PACKED_CONSTANT_FIXED_BYTES..])?)
            }
        };
        let layout = ConstantLayout {
            family,
            routing,
            branch_rounds: self.bytes[2],
            rotate_left: self.bytes[3],
            rotate_right: self.bytes[4],
            gf2_shift: self.bytes[5],
            lane_rotate: self.bytes[6],
            pivot_seed: self.bytes[7],
            branch_stride: self.bytes[8],
            branch_span: self.bytes[9],
            phase_mask: parse_u64(&self.bytes[10..18])?,
            odd_multiplier: parse_u64(&self.bytes[18..26])?,
            dominant_walsh_mask: parse_u64(&self.bytes[26..34])?,
            dominant_walsh_bias: match self.bytes[34] {
                0 => false,
                1 => true,
                _ => return Err("dominant Walsh bias must be encoded as zero or one".to_string()),
            },
            affine_mask,
        };
        validate_constant_layout(&layout)?;
        Ok(layout)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn encoded_bit_len(&self) -> usize {
        self.bytes.len() * 8
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SpectralKey(u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TrajectoryKey(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MagicKeyKind {
    Spectral,
    Trajectory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

    pub fn spectral_program(self) -> Result<SpectralProgram, String> {
        SpectralKey(self.raw).program()
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

fn parse_constant_family(raw: u8) -> Result<ConstantFamily, String> {
    match raw {
        0 => Ok(ConstantFamily::PhaseXor),
        1 => Ok(ConstantFamily::OddAffine),
        2 => Ok(ConstantFamily::Hybrid),
        _ => Err("unsupported packed constant family".to_string()),
    }
}

fn parse_routing_kind(raw: u8) -> Result<RoutingKind, String> {
    match raw {
        0 => Ok(RoutingKind::Identity),
        1 => Ok(RoutingKind::RotateWords),
        2 => Ok(RoutingKind::ReverseWords),
        3 => Ok(RoutingKind::Butterfly),
        _ => Err("unsupported routing kind".to_string()),
    }
}

fn validate_constant_layout(layout: &ConstantLayout) -> Result<(), String> {
    if !(1..=16).contains(&layout.branch_rounds) {
        return Err("branch round budget must be in 1..=16".to_string());
    }
    if !(1..64).contains(&layout.rotate_left) {
        return Err("left rotation must be in 1..63".to_string());
    }
    if !(1..64).contains(&layout.rotate_right) {
        return Err("right rotation must be in 1..63".to_string());
    }
    if !(1..64).contains(&layout.gf2_shift) {
        return Err("GF(2) shift must be in 1..63".to_string());
    }
    if layout.branch_stride == 0 {
        return Err("branch stride must be non-zero".to_string());
    }
    if layout.branch_span == 0 {
        return Err("branch span must be non-zero".to_string());
    }
    if layout.odd_multiplier & 1 == 0 {
        return Err("odd multiplier must stay odd".to_string());
    }
    if layout.dominant_walsh_mask == 0 {
        return Err("dominant Walsh mask must be non-zero".to_string());
    }
    if u32::from(layout.branch_rounds) > layout.dominant_walsh_mask.count_ones() {
        return Err("branch rounds exceed the independent pivots in the Walsh mask".to_string());
    }
    match layout.family {
        ConstantFamily::PhaseXor => {
            if layout.affine_mask.is_some() {
                return Err("phase-xor family must not carry an affine mask".to_string());
            }
        }
        ConstantFamily::OddAffine | ConstantFamily::Hybrid => {
            if layout.affine_mask.is_none() {
                return Err("affine families must carry an affine mask".to_string());
            }
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layout() -> ConstantLayout {
        ConstantLayout {
            family: ConstantFamily::Hybrid,
            routing: RoutingKind::Butterfly,
            branch_rounds: 6,
            rotate_left: 13,
            rotate_right: 7,
            gf2_shift: 9,
            lane_rotate: 5,
            pivot_seed: 11,
            branch_stride: 3,
            branch_span: 17,
            phase_mask: 0x6996_0579_31EE_C0DE,
            odd_multiplier: 0x9E37_79B9_7F4A_7C15,
            dominant_walsh_mask: 0x8080_8080_8080_8080,
            dominant_walsh_bias: false,
            affine_mask: Some(0xA5A5_5A5A_C3C3_3C3C),
        }
    }

    #[test]
    fn packed_constant_roundtrips_through_variable_length_encoding() {
        let layout = sample_layout();
        let packed = PackedConstantK::from_layout(&layout).unwrap();
        assert!(packed.as_bytes().len() > 8);
        assert_eq!(
            PackedConstantK::parse(packed.as_bytes())
                .unwrap()
                .layout()
                .unwrap(),
            layout
        );
    }

    #[test]
    fn packed_constant_length_depends_on_family_payload() {
        let hybrid = PackedConstantK::from_layout(&sample_layout()).unwrap();
        let mut simple = sample_layout();
        simple.family = ConstantFamily::PhaseXor;
        simple.affine_mask = None;
        let simple = PackedConstantK::from_layout(&simple).unwrap();
        assert!(hybrid.as_bytes().len() > simple.as_bytes().len());
    }

    #[test]
    fn malformed_packed_constant_is_rejected() {
        assert!(PackedConstantK::parse(&[1, 0, 0]).is_err());
        let mut encoded = PackedConstantK::from_layout(&sample_layout())
            .unwrap()
            .into_bytes();
        encoded[0] = 9;
        assert!(PackedConstantK::parse(&encoded).is_err());
    }

    #[test]
    fn non_odd_multiplier_is_rejected() {
        let mut layout = sample_layout();
        layout.odd_multiplier = 4;
        assert!(PackedConstantK::from_layout(&layout).is_err());
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
    fn malformed_or_non_64_bit_magic_k_is_rejected() {
        assert!(SpectralKey::parse(&[0_u8; 7]).is_err());
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
}
