#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BitBudget {
    pub source_bits: u64,
    pub key_bits: u64,
    pub crumb_bits: u64,
    pub overhead_bits: u64,
}

impl BitBudget {
    pub fn encoded_bits(&self) -> Result<u64, String> {
        self.key_bits
            .checked_add(self.crumb_bits)
            .and_then(|value| value.checked_add(self.overhead_bits))
            .ok_or_else(|| "bit budget overflow".to_string())
    }

    pub fn gain_bits(&self) -> Result<i128, String> {
        Ok(self.source_bits as i128 - self.encoded_bits()? as i128)
    }

    pub fn crumb_ratio(&self) -> Result<f64, String> {
        if self.source_bits == 0 {
            return Err("source bit length must be non-zero".to_string());
        }
        Ok(self.crumb_bits as f64 / self.source_bits as f64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressionPolicy {
    pub max_key_bits: u64,
    pub max_key_fraction: f64,
    pub minimum_gain_fraction: f64,
}

impl CompressionPolicy {
    pub const MVP: Self = Self {
        max_key_bits: 512,
        max_key_fraction: 0.25,
        minimum_gain_fraction: 0.10,
    };

    pub fn accepts(&self, budget: BitBudget) -> Result<bool, String> {
        if budget.source_bits == 0 {
            return Err("source bit length must be non-zero".to_string());
        }
        if !(0.0..1.0).contains(&self.max_key_fraction)
            || !(0.0..1.0).contains(&self.minimum_gain_fraction)
        {
            return Err("compression policy fractions must be inside [0, 1)".to_string());
        }

        let fractional_key_limit =
            (budget.source_bits as f64 * self.max_key_fraction).floor() as u64;
        let key_limit = self.max_key_bits.min(fractional_key_limit);
        if budget.key_bits > key_limit {
            return Ok(false);
        }

        let encoded = budget.encoded_bits()?;
        if encoded >= budget.source_bits {
            return Ok(false);
        }
        let gain = budget.source_bits - encoded;
        Ok(gain as f64 / budget.source_bits as f64 >= self.minimum_gain_fraction)
    }
}

pub fn accept_incremental_rule(
    current_key_bits: u64,
    current_crumb_bits: u64,
    added_key_bits: u64,
    next_crumb_bits: u64,
) -> Result<bool, String> {
    if next_crumb_bits > current_crumb_bits {
        return Ok(false);
    }
    let crumb_reduction = current_crumb_bits - next_crumb_bits;
    let next_key_bits = current_key_bits
        .checked_add(added_key_bits)
        .ok_or_else(|| "key growth overflow".to_string())?;
    let key_growth = next_key_bits - current_key_bits;
    Ok(crumb_reduction > key_growth)
}

#[cfg(test)]
mod tests {
    use super::{accept_incremental_rule, BitBudget, CompressionPolicy};

    #[test]
    fn metrics_account_for_k_v_and_overhead_separately() {
        let budget = BitBudget {
            source_bits: 4096,
            key_bits: 256,
            crumb_bits: 3000,
            overhead_bits: 64,
        };
        assert_eq!(budget.encoded_bits().unwrap(), 3320);
        assert_eq!(budget.gain_bits().unwrap(), 776);
        assert!((budget.crumb_ratio().unwrap() - 3000.0 / 4096.0).abs() < 1e-12);
    }

    #[test]
    fn hard_constraint_rejects_non_compression_and_less_than_ten_percent_gain() {
        let policy = CompressionPolicy::MVP;
        assert!(!policy
            .accepts(BitBudget {
                source_bits: 4096,
                key_bits: 256,
                crumb_bits: 3776,
                overhead_bits: 64,
            })
            .unwrap());
        assert!(!policy
            .accepts(BitBudget {
                source_bits: 4096,
                key_bits: 256,
                crumb_bits: 3400,
                overhead_bits: 64,
            })
            .unwrap());
        assert!(policy
            .accepts(BitBudget {
                source_bits: 4096,
                key_bits: 256,
                crumb_bits: 3200,
                overhead_bits: 64,
            })
            .unwrap());
    }

    #[test]
    fn hard_constraint_enforces_variable_k_upper_bound() {
        let policy = CompressionPolicy::MVP;
        assert!(!policy
            .accepts(BitBudget {
                source_bits: 4096,
                key_bits: 513,
                crumb_bits: 1,
                overhead_bits: 1,
            })
            .unwrap());
        assert!(!policy
            .accepts(BitBudget {
                source_bits: 1024,
                key_bits: 257,
                crumb_bits: 1,
                overhead_bits: 1,
            })
            .unwrap());
    }

    #[test]
    fn exact_boundary_is_not_accepted_as_strict_compression() {
        let policy = CompressionPolicy {
            minimum_gain_fraction: 0.0,
            ..CompressionPolicy::MVP
        };
        assert!(!policy
            .accepts(BitBudget {
                source_bits: 1024,
                key_bits: 128,
                crumb_bits: 832,
                overhead_bits: 64,
            })
            .unwrap());
    }

    #[test]
    fn greedy_budget_accepts_only_profitable_rules() {
        assert!(accept_incremental_rule(64, 3000, 16, 2970).unwrap());
        assert!(!accept_incremental_rule(64, 3000, 16, 2985).unwrap());
        assert!(!accept_incremental_rule(64, 3000, 16, 3010).unwrap());
    }
}
