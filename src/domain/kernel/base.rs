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
}
