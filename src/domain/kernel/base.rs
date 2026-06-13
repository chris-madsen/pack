use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum OperationCode {
    Xor = 0x01,
    RotateLeft = 0x02,
    RotateRight = 0x03,
    XorShiftLeft = 0x04,
    XorShiftRight = 0x05,
    Add = 0x06,
    Subtract = 0x07,
    MultiplyOdd = 0x08,
    BitPermutation = 0x09,
    WalshHadamard = 0x0A,
    PhaseReflect = 0x0B,
    Or = 0x0C,
    And = 0x0D,
    Not = 0x0E,
    Popcnt = 0x0F,
    CountLeadingZeros = 0x10,
    CountTrailingZeros = 0x11,
}

impl TryFrom<u8> for OperationCode {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::Xor),
            0x02 => Ok(Self::RotateLeft),
            0x03 => Ok(Self::RotateRight),
            0x04 => Ok(Self::XorShiftLeft),
            0x05 => Ok(Self::XorShiftRight),
            0x06 => Ok(Self::Add),
            0x07 => Ok(Self::Subtract),
            0x08 => Ok(Self::MultiplyOdd),
            0x09 => Ok(Self::BitPermutation),
            0x0A => Ok(Self::WalshHadamard),
            0x0B => Ok(Self::PhaseReflect),
            0x0C => Ok(Self::Or),
            0x0D => Ok(Self::And),
            0x0E => Ok(Self::Not),
            0x0F => Ok(Self::Popcnt),
            0x10 => Ok(Self::CountLeadingZeros),
            0x11 => Ok(Self::CountTrailingZeros),
            _ => Err(format!("unknown operation code: {value}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafetyClass {
    DirectlyReversible,
    FeistelOrPhaseOnly,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum PatternId {
    Raw = 0x00,
    SpectralInvolution = 0x01,
    AlphabetDiagnostic = 0x02,
    Trajectory = 0x03,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationSpec {
    pub code: OperationCode,
    pub safety: SafetyClass,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StandardBase {
    pub version: u8,
    pub operations: Vec<OperationSpec>,
    pub patterns: Vec<PatternId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootSeed {
    pub value: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedBase {
    pub root: RootSeed,
    pub operation_schedule: Vec<OperationCode>,
    pub primary_pattern: PatternId,
}

impl GeneratedBase {
    pub fn apply_forward(&self, value: u64) -> Result<u64, String> {
        self.operation_schedule
            .iter()
            .copied()
            .enumerate()
            .try_fold(value, |state, (round, operation)| {
                apply_operation(state, operation, round_constant(self.root, round))
            })
    }

    pub fn apply_inverse(&self, value: u64) -> Result<u64, String> {
        self.operation_schedule
            .iter()
            .copied()
            .enumerate()
            .rev()
            .try_fold(value, |state, (round, operation)| {
                invert_operation(state, operation, round_constant(self.root, round))
            })
    }
}

impl StandardBase {
    pub fn operation(&self, code: OperationCode) -> Option<&OperationSpec> {
        self.operations.iter().find(|item| item.code == code)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version == 0 {
            return Err("base version must be non-zero".to_string());
        }

        let operation_codes = self
            .operations
            .iter()
            .map(|item| item.code)
            .collect::<BTreeSet<_>>();
        if operation_codes.len() != self.operations.len() {
            return Err("operation codes must be unique".to_string());
        }

        let pattern_ids = self.patterns.iter().copied().collect::<BTreeSet<_>>();
        if pattern_ids.len() != self.patterns.len() {
            return Err("pattern ids must be unique".to_string());
        }

        for required in [
            OperationCode::Xor,
            OperationCode::RotateLeft,
            OperationCode::Add,
            OperationCode::MultiplyOdd,
            OperationCode::WalshHadamard,
            OperationCode::PhaseReflect,
        ] {
            if !operation_codes.contains(&required) {
                return Err(format!("missing required operation: {required:?}"));
            }
        }

        if !pattern_ids.contains(&PatternId::SpectralInvolution) {
            return Err("spectral involution pattern is required".to_string());
        }
        Ok(())
    }
}

pub fn standard_base(version: u8) -> Result<StandardBase, String> {
    if version != 1 {
        return Err(format!("unsupported standard base version: {version}"));
    }

    let directly_reversible = [
        OperationCode::Xor,
        OperationCode::RotateLeft,
        OperationCode::RotateRight,
        OperationCode::XorShiftLeft,
        OperationCode::XorShiftRight,
        OperationCode::Add,
        OperationCode::Subtract,
        OperationCode::MultiplyOdd,
        OperationCode::BitPermutation,
        OperationCode::WalshHadamard,
        OperationCode::PhaseReflect,
        OperationCode::Not,
    ];
    let feistel_or_phase_only = [
        OperationCode::Or,
        OperationCode::And,
        OperationCode::Popcnt,
        OperationCode::CountLeadingZeros,
        OperationCode::CountTrailingZeros,
    ];

    let operations = directly_reversible
        .into_iter()
        .map(|code| OperationSpec {
            code,
            safety: SafetyClass::DirectlyReversible,
        })
        .chain(feistel_or_phase_only.into_iter().map(|code| OperationSpec {
            code,
            safety: SafetyClass::FeistelOrPhaseOnly,
        }))
        .collect();

    let base = StandardBase {
        version,
        operations,
        patterns: vec![
            PatternId::Raw,
            PatternId::SpectralInvolution,
            PatternId::AlphabetDiagnostic,
            PatternId::Trajectory,
        ],
    };
    base.validate()?;
    Ok(base)
}

pub fn generate_base_from_root(
    base: &StandardBase,
    root: RootSeed,
) -> Result<GeneratedBase, String> {
    base.validate()?;
    let reversible = base
        .operations
        .iter()
        .filter(|item| item.safety == SafetyClass::DirectlyReversible)
        .map(|item| item.code)
        .collect::<Vec<_>>();
    if reversible.is_empty() {
        return Err("standard base does not contain reversible operations".to_string());
    }

    let mut state = root.value | 1;
    let mut operation_schedule = Vec::with_capacity(8);
    for round in 0..8_u32 {
        state ^= state << 13;
        state ^= state >> 7;
        state = state.rotate_left((round % 31) + 1);
        let index = (state as usize) % reversible.len();
        operation_schedule.push(reversible[index]);
    }

    Ok(GeneratedBase {
        root,
        operation_schedule,
        primary_pattern: PatternId::SpectralInvolution,
    })
}

fn round_constant(root: RootSeed, round: usize) -> u64 {
    let mixed = root
        .value
        .wrapping_add((round as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    avalanche(mixed)
}

fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn apply_operation(value: u64, operation: OperationCode, parameter: u64) -> Result<u64, String> {
    let shift = ((parameter % 63) + 1) as u32;
    let rotate = (parameter % 64) as u32;
    Ok(match operation {
        OperationCode::Xor => value ^ parameter,
        OperationCode::RotateLeft => value.rotate_left(rotate),
        OperationCode::RotateRight => value.rotate_right(rotate),
        OperationCode::XorShiftLeft => value ^ value.wrapping_shl(shift),
        OperationCode::XorShiftRight => value ^ value.wrapping_shr(shift),
        OperationCode::Add => value.wrapping_add(parameter),
        OperationCode::Subtract => value.wrapping_sub(parameter),
        OperationCode::MultiplyOdd => value.wrapping_mul(parameter | 1),
        OperationCode::BitPermutation | OperationCode::WalshHadamard => value.reverse_bits(),
        OperationCode::PhaseReflect => value ^ parameter.rotate_left(17),
        OperationCode::Not => !value,
        other => {
            return Err(format!(
                "operation {other:?} is not reversible at word level"
            ))
        }
    })
}

fn invert_operation(value: u64, operation: OperationCode, parameter: u64) -> Result<u64, String> {
    let shift = ((parameter % 63) + 1) as u32;
    let rotate = (parameter % 64) as u32;
    Ok(match operation {
        OperationCode::Xor => value ^ parameter,
        OperationCode::RotateLeft => value.rotate_right(rotate),
        OperationCode::RotateRight => value.rotate_left(rotate),
        OperationCode::XorShiftLeft => invert_xorshift_left(value, shift),
        OperationCode::XorShiftRight => invert_xorshift_right(value, shift),
        OperationCode::Add => value.wrapping_sub(parameter),
        OperationCode::Subtract => value.wrapping_add(parameter),
        OperationCode::MultiplyOdd => value.wrapping_mul(modular_inverse_odd(parameter | 1)),
        OperationCode::BitPermutation | OperationCode::WalshHadamard => value.reverse_bits(),
        OperationCode::PhaseReflect => value ^ parameter.rotate_left(17),
        OperationCode::Not => !value,
        other => {
            return Err(format!(
                "operation {other:?} is not reversible at word level"
            ))
        }
    })
}

fn invert_xorshift_left(value: u64, shift: u32) -> u64 {
    let mut restored = value;
    let mut offset = shift;
    while offset < 64 {
        restored ^= restored.wrapping_shl(offset);
        offset = offset.saturating_mul(2);
    }
    restored
}

fn invert_xorshift_right(value: u64, shift: u32) -> u64 {
    let mut restored = value;
    let mut offset = shift;
    while offset < 64 {
        restored ^= restored.wrapping_shr(offset);
        offset = offset.saturating_mul(2);
    }
    restored
}

fn modular_inverse_odd(value: u64) -> u64 {
    let mut inverse = value;
    for _ in 0..6 {
        inverse = inverse.wrapping_mul(2_u64.wrapping_sub(value.wrapping_mul(inverse)));
    }
    inverse
}

#[cfg(test)]
mod tests {
    use super::{
        generate_base_from_root, standard_base, OperationCode, PatternId, RootSeed, SafetyClass,
    };

    #[test]
    fn b_std_is_deterministic_and_contains_required_algebra() {
        let left = standard_base(1).unwrap();
        let right = standard_base(1).unwrap();
        assert_eq!(left, right);
        left.validate().unwrap();
        assert!(left.patterns.contains(&PatternId::SpectralInvolution));
        assert_eq!(
            left.operation(OperationCode::WalshHadamard).unwrap().safety,
            SafetyClass::DirectlyReversible
        );
    }

    #[test]
    fn irreversible_macros_are_restricted_to_feistel_or_phase_context() {
        let base = standard_base(1).unwrap();
        for code in [
            OperationCode::Or,
            OperationCode::And,
            OperationCode::Popcnt,
            OperationCode::CountLeadingZeros,
            OperationCode::CountTrailingZeros,
        ] {
            assert_eq!(
                base.operation(code).unwrap().safety,
                SafetyClass::FeistelOrPhaseOnly
            );
        }
    }

    #[test]
    fn unknown_base_version_is_rejected() {
        assert!(standard_base(0).is_err());
        assert!(standard_base(2).is_err());
    }

    #[test]
    fn k_root_generates_the_same_file_base_for_both_sides() {
        let base = standard_base(1).unwrap();
        let left = generate_base_from_root(&base, RootSeed { value: 0xDEAD_BEEF }).unwrap();
        let right = generate_base_from_root(&base, RootSeed { value: 0xDEAD_BEEF }).unwrap();
        assert_eq!(left, right);
    }

    #[test]
    fn changing_k_root_changes_generated_base_schedule() {
        let base = standard_base(1).unwrap();
        let left = generate_base_from_root(&base, RootSeed { value: 1 }).unwrap();
        let right = generate_base_from_root(&base, RootSeed { value: 2 }).unwrap();
        assert_ne!(left.operation_schedule, right.operation_schedule);
    }

    #[test]
    fn generated_base_schedule_is_executable_and_reversible() {
        let base = standard_base(1).unwrap();
        let generated = generate_base_from_root(
            &base,
            RootSeed {
                value: 0xDEAD_BEEF_CAFE_BABE,
            },
        )
        .unwrap();
        for value in [0, 1, u64::MAX, 0x0123_4567_89AB_CDEF] {
            let encoded = generated.apply_forward(value).unwrap();
            assert_ne!(encoded, value);
            assert_eq!(generated.apply_inverse(encoded).unwrap(), value);
        }
    }

    #[test]
    fn changing_generated_schedule_changes_execution() {
        let base = standard_base(1).unwrap();
        let left = generate_base_from_root(&base, RootSeed { value: 11 }).unwrap();
        let right = generate_base_from_root(&base, RootSeed { value: 12 }).unwrap();
        let value = 0x0123_4567_89AB_CDEF;
        assert_ne!(
            left.apply_forward(value).unwrap(),
            right.apply_forward(value).unwrap()
        );
    }
}
