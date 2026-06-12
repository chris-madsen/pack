#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParityTrajectory {
    pub terminal: u64,
    pub crumbs: Vec<bool>,
}

pub fn encode_parity_trajectory(mut value: u64, steps: usize) -> Result<ParityTrajectory, String> {
    if steps > 64 {
        return Err("u64 trajectory cannot contain more than 64 parity steps".to_string());
    }
    let mut crumbs = Vec::with_capacity(steps);
    for _ in 0..steps {
        crumbs.push(value & 1 == 1);
        value >>= 1;
    }
    Ok(ParityTrajectory {
        terminal: value,
        crumbs,
    })
}

pub fn decode_parity_trajectory(trajectory: &ParityTrajectory) -> Result<u64, String> {
    if trajectory.crumbs.len() > 64 {
        return Err("u64 trajectory cannot contain more than 64 parity steps".to_string());
    }
    let mut value = trajectory.terminal;
    for crumb in trajectory.crumbs.iter().rev() {
        value = value
            .checked_shl(1)
            .ok_or_else(|| "trajectory reconstruction overflow".to_string())?;
        value |= u64::from(*crumb);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{decode_parity_trajectory, encode_parity_trajectory, ParityTrajectory};

    #[test]
    fn v_contains_exact_forward_parity_order_and_reconstructs_backwards() {
        let value = 13_u64;
        let trajectory = encode_parity_trajectory(value, 4).unwrap();
        assert_eq!(trajectory.crumbs, vec![true, false, true, true]);
        assert_eq!(decode_parity_trajectory(&trajectory).unwrap(), value);
    }

    #[test]
    fn every_crumb_is_semantically_required_for_unique_reconstruction() {
        let trajectory = encode_parity_trajectory(0b1011_0110, 8).unwrap();
        let original = decode_parity_trajectory(&trajectory).unwrap();
        for index in 0..trajectory.crumbs.len() {
            let mut changed = trajectory.clone();
            changed.crumbs[index] = !changed.crumbs[index];
            assert_ne!(decode_parity_trajectory(&changed).unwrap(), original);
        }
    }

    #[test]
    fn decoder_requires_only_terminal_and_v_without_hidden_state() {
        let portable = ParityTrajectory {
            terminal: 0x1234,
            crumbs: vec![true, false, true, false],
        };
        assert_eq!(
            decode_parity_trajectory(&portable).unwrap(),
            (0x1234_u64 << 4) | 0b0101
        );
    }
}
