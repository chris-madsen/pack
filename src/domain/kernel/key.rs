use crate::domain::kernel::base::PatternId;

const KEY_MAGIC: &[u8; 4] = b"KIR1";
const MAX_KEY_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum SegmentKind {
    RevMix = 0x01,
    PhaseMask = 0x02,
    WalshConfig = 0x03,
    CrumbConfig = 0x04,
    AuxConst = 0x05,
}

impl TryFrom<u8> for SegmentKind {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::RevMix),
            0x02 => Ok(Self::PhaseMask),
            0x03 => Ok(Self::WalshConfig),
            0x04 => Ok(Self::CrumbConfig),
            0x05 => Ok(Self::AuxConst),
            _ => Err(format!("unknown K segment kind: {value}")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyHeader {
    pub version: u8,
    pub main_pattern_id: PatternId,
    pub rounds: u8,
    pub block_log2: u8,
    pub flags: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeySegment {
    pub kind: SegmentKind,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MagicKey {
    pub header: KeyHeader,
    pub segments: Vec<KeySegment>,
}

impl MagicKey {
    pub fn validate(&self) -> Result<(), String> {
        if self.header.version != 1 {
            return Err("unsupported K version".to_string());
        }
        if self.header.rounds == 0 {
            return Err("K must encode at least one round".to_string());
        }
        if !(1..=31).contains(&self.header.block_log2) {
            return Err("K block_log2 is outside the supported range".to_string());
        }
        if self.segments.is_empty() {
            return Err("K must contain at least one segment".to_string());
        }
        if self
            .segments
            .windows(2)
            .any(|pair| pair[0].kind >= pair[1].kind)
        {
            return Err("K segments must be unique and canonically ordered".to_string());
        }
        if self
            .segments
            .iter()
            .any(|segment| segment.payload.is_empty())
        {
            return Err("K segments must not be empty".to_string());
        }
        if self.serialize_unchecked().len() > MAX_KEY_BYTES {
            return Err("K exceeds 512-bit MVP limit".to_string());
        }
        Ok(())
    }

    pub fn serialize(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        Ok(self.serialize_unchecked())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_KEY_BYTES {
            return Err("K exceeds 512-bit MVP limit".to_string());
        }
        if bytes.len() < 12 || &bytes[..4] != KEY_MAGIC {
            return Err("invalid K header".to_string());
        }
        let pattern = parse_pattern(bytes[5])?;
        let segment_count = bytes[10] as usize;
        let mut cursor = 12_usize;
        let mut segments = Vec::with_capacity(segment_count);

        for _ in 0..segment_count {
            let kind = SegmentKind::try_from(read_u8(bytes, &mut cursor)?)?;
            let len = read_u16(bytes, &mut cursor)? as usize;
            let end = cursor
                .checked_add(len)
                .ok_or_else(|| "K cursor overflow".to_string())?;
            let payload = bytes
                .get(cursor..end)
                .ok_or_else(|| "truncated K segment".to_string())?
                .to_vec();
            cursor = end;
            segments.push(KeySegment { kind, payload });
        }
        if cursor != bytes.len() {
            return Err("K contains trailing bytes".to_string());
        }

        let key = Self {
            header: KeyHeader {
                version: bytes[4],
                main_pattern_id: pattern,
                rounds: bytes[6],
                block_log2: bytes[7],
                flags: u16::from_le_bytes([bytes[8], bytes[9]]),
            },
            segments,
        };
        key.validate()?;
        Ok(key)
    }

    pub fn bit_len(&self) -> Result<usize, String> {
        self.serialize().map(|bytes| bytes.len() * 8)
    }

    pub fn unchecked_serialized_len(&self) -> usize {
        self.serialize_unchecked().len()
    }

    fn serialize_unchecked(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(KEY_MAGIC);
        out.push(self.header.version);
        out.push(self.header.main_pattern_id as u8);
        out.push(self.header.rounds);
        out.push(self.header.block_log2);
        out.extend_from_slice(&self.header.flags.to_le_bytes());
        out.push(self.segments.len() as u8);
        out.push(0);
        for segment in &self.segments {
            out.push(segment.kind as u8);
            out.extend_from_slice(&(segment.payload.len() as u16).to_le_bytes());
            out.extend_from_slice(&segment.payload);
        }
        out
    }
}

fn parse_pattern(value: u8) -> Result<PatternId, String> {
    match value {
        0x00 => Ok(PatternId::Raw),
        0x01 => Ok(PatternId::SpectralInvolution),
        0x02 => Ok(PatternId::AlphabetDiagnostic),
        0x03 => Ok(PatternId::Trajectory),
        _ => Err(format!("unknown pattern id: {value}")),
    }
}

fn read_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8, String> {
    let value = bytes
        .get(*cursor)
        .copied()
        .ok_or_else(|| "truncated K".to_string())?;
    *cursor += 1;
    Ok(value)
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, String> {
    let lo = read_u8(bytes, cursor)?;
    let hi = read_u8(bytes, cursor)?;
    Ok(u16::from_le_bytes([lo, hi]))
}

#[cfg(test)]
mod tests {
    use crate::domain::kernel::base::PatternId;

    use super::{KeyHeader, KeySegment, MagicKey, SegmentKind};

    fn sample_key() -> MagicKey {
        MagicKey {
            header: KeyHeader {
                version: 1,
                main_pattern_id: PatternId::SpectralInvolution,
                rounds: 3,
                block_log2: 12,
                flags: 0xA5,
            },
            segments: vec![
                KeySegment {
                    kind: SegmentKind::RevMix,
                    payload: vec![3, 7, 13, 5],
                },
                KeySegment {
                    kind: SegmentKind::PhaseMask,
                    payload: vec![0xAA, 0x55],
                },
                KeySegment {
                    kind: SegmentKind::WalshConfig,
                    payload: vec![1, 2, 3],
                },
                KeySegment {
                    kind: SegmentKind::CrumbConfig,
                    payload: vec![1],
                },
            ],
        }
    }

    #[test]
    fn hybrid_k_roundtrips_canonically_and_is_versioned() {
        let key = sample_key();
        let encoded = key.serialize().unwrap();
        assert_eq!(MagicKey::parse(&encoded).unwrap(), key);
        assert_eq!(
            MagicKey::parse(&encoded).unwrap().serialize().unwrap(),
            encoded
        );
        assert_eq!(encoded[4], 1);
    }

    #[test]
    fn segment_order_is_semantic_and_noncanonical_order_is_rejected() {
        let mut key = sample_key();
        key.segments.swap(0, 1);
        assert!(key.serialize().is_err());
    }

    #[test]
    fn k_over_512_bits_is_rejected() {
        let mut key = sample_key();
        key.segments[3].payload = vec![0x11; 64];
        assert!(key.serialize().unwrap_err().contains("512-bit"));
    }

    #[test]
    fn malformed_or_extended_k_is_rejected() {
        let mut encoded = sample_key().serialize().unwrap();
        encoded.push(0);
        assert!(MagicKey::parse(&encoded).is_err());
        let mut unknown = sample_key().serialize().unwrap();
        unknown[5] = 0xFF;
        assert!(MagicKey::parse(&unknown).is_err());
    }
}
