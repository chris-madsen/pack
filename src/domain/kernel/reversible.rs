#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeistelRoundKey {
    pub shift_left: u8,
    pub shift_right: u8,
    pub rotate: u8,
    pub odd_multiplier: u32,
    pub add: u32,
    pub mask: u32,
}

impl FeistelRoundKey {
    pub fn validate(&self) -> Result<(), String> {
        if self.shift_left == 0 || self.shift_left >= 32 {
            return Err("left shift must be in 1..32".to_string());
        }
        if self.shift_right == 0 || self.shift_right >= 32 {
            return Err("right shift must be in 1..32".to_string());
        }
        if self.rotate >= 32 {
            return Err("rotate must be below 32".to_string());
        }
        if self.odd_multiplier & 1 == 0 {
            return Err("Feistel multiplier must be odd".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeistelKey {
    pub rounds: Vec<FeistelRoundKey>,
}

impl FeistelKey {
    pub fn validate(&self) -> Result<(), String> {
        if self.rounds.is_empty() {
            return Err("Feistel key must contain rounds".to_string());
        }
        for round in &self.rounds {
            round.validate()?;
        }
        Ok(())
    }
}

pub fn feistel_forward(value: u64, key: &FeistelKey) -> Result<u64, String> {
    key.validate()?;
    let mut left = (value >> 32) as u32;
    let mut right = value as u32;
    for round in &key.rounds {
        let next_left = right;
        let next_right = left ^ round_function(right, round);
        left = next_left;
        right = next_right;
    }
    Ok(((left as u64) << 32) | right as u64)
}

pub fn feistel_inverse(value: u64, key: &FeistelKey) -> Result<u64, String> {
    key.validate()?;
    let mut left = (value >> 32) as u32;
    let mut right = value as u32;
    for round in key.rounds.iter().rev() {
        let previous_right = left;
        let previous_left = right ^ round_function(left, round);
        left = previous_left;
        right = previous_right;
    }
    Ok(((left as u64) << 32) | right as u64)
}

fn round_function(value: u32, key: &FeistelRoundKey) -> u32 {
    let mut mixed = value;
    mixed ^= mixed.wrapping_shl(key.shift_left as u32);
    mixed ^= mixed.wrapping_shr(key.shift_right as u32);
    mixed = mixed.rotate_left(key.rotate as u32);
    mixed = mixed.wrapping_mul(key.odd_multiplier);
    mixed = mixed.wrapping_add(key.add);
    mixed ^= (mixed & key.mask).count_ones();
    mixed
}

#[cfg(test)]
mod tests {
    use super::{feistel_forward, feistel_inverse, FeistelKey, FeistelRoundKey};

    fn sample_key() -> FeistelKey {
        FeistelKey {
            rounds: vec![
                FeistelRoundKey {
                    shift_left: 5,
                    shift_right: 11,
                    rotate: 7,
                    odd_multiplier: 0x9E37_79B1,
                    add: 0xA5A5_5A5A,
                    mask: 0x0F0F_F0F0,
                },
                FeistelRoundKey {
                    shift_left: 13,
                    shift_right: 3,
                    rotate: 17,
                    odd_multiplier: 0x85EB_CA6B,
                    add: 0xC2B2_AE35,
                    mask: 0x3333_CCCC,
                },
            ],
        }
    }

    #[test]
    fn feistel_arx_fk_roundtrips_many_states() {
        let key = sample_key();
        for index in 0..10_000_u64 {
            let value = index
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .rotate_left((index % 64) as u32);
            let encoded = feistel_forward(value, &key).unwrap();
            let decoded = feistel_inverse(encoded, &key).unwrap();
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn popcnt_inside_round_does_not_break_feistel_invertibility() {
        let key = sample_key();
        let value = 0xDEAD_BEEF_0123_4567;
        assert_eq!(
            feistel_inverse(feistel_forward(value, &key).unwrap(), &key).unwrap(),
            value
        );
    }

    #[test]
    fn even_multiplier_is_rejected() {
        let mut key = sample_key();
        key.rounds[0].odd_multiplier = 2;
        assert!(feistel_forward(42, &key)
            .unwrap_err()
            .contains("must be odd"));
    }
}
