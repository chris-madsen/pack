v// FILE: src/domain/kernel/operator.rs

use crate::domain::kernel::key::GenerativeBlueprint;

pub struct BifurcationStream<'a> {
    pub crumbs: &'a [bool],
    pub cursor: usize,
}

impl<'a> BifurcationStream<'a> {
    pub fn new(crumbs: &'a [bool]) -> Self {
        Self { crumbs, cursor: 0 }
    }

    pub fn read_bit(&mut self) -> Result<bool, String> {
        if self.cursor >= self.crumbs.len() {
            return Err("bifurcation stream exhausted".to_string());
        }
        let bit = self.crumbs[self.cursor];
        self.cursor += 1;
        Ok(bit)
    }
}

// Determines if a topological node is a structural ambiguity requiring a crumb
fn is_bifurcation_point(word_index: usize, mask: u8) -> bool {
    (word_index & (mask as usize)) == (mask as usize)
}

// Reversible operator U_K parameterized purely by the GenerativeBlueprint.
// Consumes Vector V strictly during execution.
pub fn apply_u_k_with_stream(
    words: &mut [u64],
    blueprint: &GenerativeBlueprint,
    stream: &mut BifurcationStream,
) -> Result<(), String> {
    let log_n = blueprint.hadamard_log_n as usize;
    if log_n == 0 {
        return Err("invalid blueprint hardware bounds".to_string());
    }

    let mask = blueprint.clean_mask;
    let parity = blueprint.hadamard_parity as u64;

    // Simulated structural pass where V resolves path collisions dynamically
    for (idx, word) in words.iter_mut().enumerate() {
        // Deterministic linear involution (XOR/Shift) directed by channels
        *word ^= parity;
        *word = word.rotate_left(blueprint.channel_m0 as u32);
        
        // Critical point: structural ambiguity detected via invariant index.
        if is_bifurcation_point(idx, mask) {
            let inversion_flag = stream.read_bit()?;
            if inversion_flag {
                *word = !*word; // Phase reflect
            }
        }

        // Complete the linear path
        *word ^= parity.rotate_right(blueprint.channel_m1 as u32);
    }

    Ok(())
}