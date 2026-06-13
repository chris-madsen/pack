// FILE: src/domain/kernel/key.rs

pub const MAGIC_KEY_BYTES: usize = 8;

// ── Kind tag ─────────────────────────────────────────────────────────────────

/// Two-bit tag stored in bits [62:63] of every MagicKey.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MagicKeyKind {
    Trajectory = 0,
    Operator   = 1,
    Spectral   = 2,
}

impl MagicKeyKind {
    fn encode(self) -> u64 {
        (self as u64) << 62
    }

    fn decode(raw: u64) -> Result<Self, String> {
        match raw >> 62 {
            0 => Ok(Self::Trajectory),
            1 => Ok(Self::Operator),
            2 => Ok(Self::Spectral),
            other => Err(format!("unknown MagicKey kind tag: {other}")),
        }
    }
}

// ── OperatorBlueprint ─────────────────────────────────────────────────────────
//
// Packed into 62 payload bits of MagicKey (kind = Operator):
//
//   [0..6]    dominant_index     (6 bits,  0..63)
//   [6]       dominant_positive  (1 bit)
//   [7..12]   primary_shift      (5 bits,  1..31)
//   [12..21]  shift_match        (9 bits)
//   [21..29]  derivative_density (8 bits)
//   [29..37]  popcnt_density     (8 bits)
//   [37..42]  secondary_delta    (5 bits)
//   [42..47]  tertiary_delta     (5 bits)
//   [47..52]  fingerprint_bias   (5 bits)
//   [52..62]  reserved           (10 bits, 0)

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperatorBlueprint {
    pub dominant_index:     u16,  // 6 bits
    pub dominant_positive:  bool,
    pub primary_shift:      u8,   // 5 bits, value 1..31
    pub shift_match:        u16,  // 9 bits
    pub derivative_density: u8,
    pub popcnt_density:     u8,
    pub secondary_delta:    u8,   // 5 bits
    pub tertiary_delta:     u8,   // 5 bits
    pub fingerprint_bias:   u8,   // 5 bits
}

impl OperatorBlueprint {
    fn pack(self) -> u64 {
        let mut v = 0_u64;
        v |=  (self.dominant_index as u64 & 0x3F);
        v |=  (self.dominant_positive as u64)  << 6;
        v |=  (self.primary_shift    as u64 & 0x1F) << 7;
        v |=  (self.shift_match      as u64 & 0x1FF) << 12;
        v |=  (self.derivative_density as u64) << 21;
        v |=  (self.popcnt_density     as u64) << 29;
        v |=  (self.secondary_delta    as u64 & 0x1F) << 37;
        v |=  (self.tertiary_delta     as u64 & 0x1F) << 42;
        v |=  (self.fingerprint_bias   as u64 & 0x1F) << 47;
        v
    }

    fn unpack(v: u64) -> Self {
        Self {
            dominant_index:     (v & 0x3F) as u16,
            dominant_positive:  (v >> 6) & 1 == 1,
            primary_shift:      ((v >> 7)  & 0x1F) as u8,
            shift_match:        ((v >> 12) & 0x1FF) as u16,
            derivative_density: ((v >> 21) & 0xFF) as u8,
            popcnt_density:     ((v >> 29) & 0xFF) as u8,
            secondary_delta:    ((v >> 37) & 0x1F) as u8,
            tertiary_delta:     ((v >> 42) & 0x1F) as u8,
            fingerprint_bias:   ((v >> 47) & 0x1F) as u8,
        }
    }
}

// ── SpectralBlueprint / SpectralProgram ───────────────────────────────────────
//
// Packed into 62 payload bits of MagicKey (kind = Spectral):
//
//   [0..13]  bit_len_log2  — ilog2(bit_len), 13 bits
//   [13..26] peak0_index   — 13 bits
//   [26]     peak0_pos     — 1 bit
//   [27..33] peak0_amp     — 6 bits
//   [33..46] peak1_index   — 13 bits
//   [46]     peak1_pos     — 1 bit
//   [47..53] peak1_amp     — 6 bits
//   [53]     tie_bit       — 1 bit

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpectralPeakCode {
    pub index:    usize,
    pub positive: bool,
    pub amplitude: u8,   // 6 bits
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpectralProgram {
    pub bit_len: usize,
    pub peaks:   Vec<SpectralPeakCode>,
    pub tie_bit: bool,
}

// ── Trajectory payload ────────────────────────────────────────────────────────
//
// Packed into 62 payload bits of MagicKey (kind = Trajectory):
//
//   [0..48]  fingerprint  (48 bits)
//   [48..54] steps        (6 bits, 0..63)

// ── MagicKey ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MagicKey(u64);

impl MagicKey {
    // ── constructors ─────────────────────────────────────────────────────────

    pub fn from_operator_blueprint(bp: &OperatorBlueprint) -> Result<Self, String> {
        if bp.primary_shift == 0 || bp.primary_shift > 31 {
            return Err("primary_shift must be in 1..=31".to_string());
        }
        Ok(Self(MagicKeyKind::Operator.encode() | bp.pack()))
    }

    pub fn from_spectral_program(prog: &SpectralProgram) -> Result<Self, String> {
        if prog.bit_len == 0 || !prog.bit_len.is_power_of_two() {
            return Err("spectral bit_len must be a non-zero power of two".to_string());
        }
        let log2 = prog.bit_len.ilog2() as u64;
        let p0 = prog.peaks.get(0);
        let p1 = prog.peaks.get(1);
        let enc_peak = |p: Option<&SpectralPeakCode>| -> u64 {
            match p {
                Some(pk) => {
                    (pk.index as u64 & 0x1FFF)
                        | ((pk.positive as u64) << 13)
                        | ((pk.amplitude as u64 & 0x3F) << 14)
                }
                None => 0,
            }
        };
        let v = log2
            | (enc_peak(p0) << 13)
            | (enc_peak(p1) << 33)
            | ((prog.tie_bit as u64) << 53);
        Ok(Self(MagicKeyKind::Spectral.encode() | v))
    }

    pub fn from_trajectory_payload(fingerprint: u64, steps: u8) -> Result<Self, String> {
        if steps > 63 {
            return Err("trajectory steps must be <= 63".to_string());
        }
        let v = (fingerprint & 0x0000_FFFF_FFFF_FFFF)
            | ((steps as u64) << 48);
        Ok(Self(MagicKeyKind::Trajectory.encode() | v))
    }

    // ── kind guard ────────────────────────────────────────────────────────────

    pub fn require_kind(self, expected: MagicKeyKind) -> Result<Self, String> {
        let got = MagicKeyKind::decode(self.0)?;
        if got != expected {
            return Err(format!("expected MagicKey kind {expected:?}, got {got:?}"));
        }
        Ok(self)
    }

    // ── extractors ───────────────────────────────────────────────────────────

    pub fn operator_blueprint(self) -> Result<OperatorBlueprint, String> {
        self.require_kind(MagicKeyKind::Operator)?;
        Ok(OperatorBlueprint::unpack(self.0 & !(3_u64 << 62)))
    }

    pub fn spectral_program(self) -> Result<SpectralProgram, String> {
        self.require_kind(MagicKeyKind::Spectral)?;
        let v = self.0 & !(3_u64 << 62);
        let log2 = (v & 0x1FFF) as u32;
        let bit_len = if log2 == 0 { 1 } else { 1_usize << log2 };

        let dec_peak = |bits: u64| SpectralPeakCode {
            index:     (bits & 0x1FFF) as usize,
            positive:  (bits >> 13) & 1 == 1,
            amplitude: ((bits >> 14) & 0x3F) as u8,
        };

        let raw0 = (v >> 13) & 0xFFFFF;
        let raw1 = (v >> 33) & 0xFFFFF;
        let tie_bit = (v >> 53) & 1 == 1;

        let mut peaks = vec![dec_peak(raw0)];
        if raw1 != 0 {
            peaks.push(dec_peak(raw1));
        }
        Ok(SpectralProgram { bit_len, peaks, tie_bit })
    }

    pub fn trajectory_payload(self) -> Result<(u64, u8), String> {
        self.require_kind(MagicKeyKind::Trajectory)?;
        let v = self.0 & !(3_u64 << 62);
        let fingerprint = v & 0x0000_FFFF_FFFF_FFFF;
        let steps = ((v >> 48) & 0x3F) as u8;
        Ok((fingerprint, steps))
    }

    // ── serde ─────────────────────────────────────────────────────────────────

    pub fn serialize(self) -> [u8; MAGIC_KEY_BYTES] {
        self.0.to_le_bytes()
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let array: [u8; MAGIC_KEY_BYTES] = bytes
            .try_into()
            .map_err(|_| format!("MagicKey expects {MAGIC_KEY_BYTES} bytes, got {}", bytes.len()))?;
        Ok(Self(u64::from_le_bytes(array)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_operator_blueprint() -> OperatorBlueprint {
        OperatorBlueprint {
            dominant_index:     17,
            dominant_positive:  true,
            primary_shift:      5,
            shift_match:        300,
            derivative_density: 0xAB,
            popcnt_density:     0xCD,
            secondary_delta:    11,
            tertiary_delta:     3,
            fingerprint_bias:   7,
        }
    }

    #[test]
    fn operator_blueprint_roundtrips_through_magic_key() {
        let bp = sample_operator_blueprint();
        let key = MagicKey::from_operator_blueprint(&bp).unwrap();
        let recovered = key.operator_blueprint().unwrap();
        assert_eq!(recovered, bp);
    }

    #[test]
    fn spectral_program_roundtrips_through_magic_key() {
        let prog = SpectralProgram {
            bit_len: 512,
            peaks: vec![
                SpectralPeakCode { index: 17,  positive: true,  amplitude: 31 },
                SpectralPeakCode { index: 103, positive: false, amplitude: 14 },
            ],
            tie_bit: true,
        };
        let key = MagicKey::from_spectral_program(&prog).unwrap();
        let recovered = key.spectral_program().unwrap();
        assert_eq!(recovered.bit_len, prog.bit_len);
        assert_eq!(recovered.tie_bit,  prog.tie_bit);
        assert_eq!(recovered.peaks[0].index,    prog.peaks[0].index);
        assert_eq!(recovered.peaks[0].positive, prog.peaks[0].positive);
        assert_eq!(recovered.peaks[0].amplitude,prog.peaks[0].amplitude);
    }

    #[test]
    fn trajectory_payload_roundtrips() {
        let fingerprint = 0x0000_DEAD_BEEF_0000_u64;
        let steps = 42_u8;
        let key = MagicKey::from_trajectory_payload(fingerprint, steps).unwrap();
        let (fp2, st2) = key.trajectory_payload().unwrap();
        assert_eq!(fp2, fingerprint);
        assert_eq!(st2, steps);
    }

    #[test]
    fn require_kind_rejects_wrong_kind() {
        let key = MagicKey::from_operator_blueprint(&sample_operator_blueprint()).unwrap();
        assert!(key.require_kind(MagicKeyKind::Spectral).is_err());
        assert!(key.require_kind(MagicKeyKind::Trajectory).is_err());
        assert!(key.require_kind(MagicKeyKind::Operator).is_ok());
    }

    #[test]
    fn serialize_parse_roundtrip() {
        let key = MagicKey::from_operator_blueprint(&sample_operator_blueprint()).unwrap();
        let bytes = key.serialize();
        let parsed = MagicKey::parse(&bytes).unwrap();
        assert_eq!(parsed, key);
    }

    #[test]
    fn zero_primary_shift_is_rejected() {
        let mut bp = sample_operator_blueprint();
        bp.primary_shift = 0;
        assert!(MagicKey::from_operator_blueprint(&bp).is_err());
    }
}
