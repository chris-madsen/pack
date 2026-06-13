// FILE: src/domain/kernel/key.rs

pub const MAGIC_KEY_BYTES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerativeBlueprint {
    pub hadamard_log_n: u8,   // 5 bits
    pub hadamard_parity: u16, // 16 bits
    pub channel_m0: u8,       // 4 bits
    pub channel_m1: u8,       // 4 bits
    pub channel_m2: u8,       // 4 bits
    pub channel_m3: u8,       // 4 bits
    pub clean_mask: u8,       // 4 bits
    pub reserved: u32,        // 23 bits
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MagicKey(u64);

impl MagicKey {
    pub fn from_blueprint(blueprint: &GenerativeBlueprint) -> Result<Self, String> {
        if blueprint.hadamard_log_n > 31 || blueprint.clean_mask > 15 {
            return Err("Blueprint field overflow".to_string());
        }

        let payload = (blueprint.hadamard_log_n as u64)
            | ((blueprint.hadamard_parity as u64) << 5)
            | ((blueprint.channel_m0 as u64) << 21)
            | ((blueprint.channel_m1 as u64) << 25)
            | ((blueprint.channel_m2 as u64) << 29)
            | ((blueprint.channel_m3 as u64) << 33)
            | ((blueprint.clean_mask as u64) << 37)
            | ((blueprint.reserved as u64) << 41);

        Ok(Self(payload))
    }

    pub fn blueprint(self) -> GenerativeBlueprint {
        let p = self.0;
        GenerativeBlueprint {
            hadamard_log_n: (p & 0x1F) as u8,
            hadamard_parity: ((p >> 5) & 0xFFFF) as u16,
            channel_m0: ((p >> 21) & 0x0F) as u8,
            channel_m1: ((p >> 25) & 0x0F) as u8,
            channel_m2: ((p >> 29) & 0x0F) as u8,
            channel_m3: ((p >> 33) & 0x0F) as u8,
            clean_mask: ((p >> 37) & 0x0F) as u8,
            reserved: (p >> 41) as u32,
        }
    }

    pub fn serialize(self) -> [u8; MAGIC_KEY_BYTES] {
        self.0.to_le_bytes()
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let array: [u8; MAGIC_KEY_BYTES] = bytes.try_into().map_err(|_| "Invalid key size")?;
        Ok(Self(u64::from_le_bytes(array)))
    }
}