pub const MAGIC_KEY_BYTES: usize = 8;
pub const MAX_SPECTRAL_PEAKS: usize = 3;
pub const SPECTRAL_INDEX_BITS: usize = 13;
pub const SPECTRAL_AMPLITUDE_BITS: usize = 5;
pub const MAX_SPECTRAL_BITS: usize = 1 << SPECTRAL_INDEX_BITS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MagicKey(u64);

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

impl MagicKey {
    pub fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub fn raw(self) -> u64 {
        self.0
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

    pub fn bit_len(self) -> usize {
        u64::BITS as usize
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

#[cfg(test)]
mod tests {
    use super::{MagicKey, SpectralPeakCode, SpectralProgram, MAGIC_KEY_BYTES};

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
        assert_eq!(key.bit_len(), 64);
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
        assert!(MagicKey::from_raw(0).spectral_program().is_err());
    }
}
