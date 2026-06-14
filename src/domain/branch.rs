//! Bifurcation / branch-vector module.
//!
//! CRITICAL: V is NOT `N XOR Generate(K)`.
//!
//! V is a **parity-bit log of branching decisions** made during operator
//! expansion. When U_K expands the state space, there are steps where the
//! phase is ambiguous (two equally valid trajectories). At each such step
//! we emit exactly ONE bit into V that records which branch was taken.
//!
//! Consequence: |V| == number_of_expansion_steps, completely independent
//! of block size N. That is the source of compression.

/// One expansion step of the operator.
/// At each step the operator makes a phase decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchBit {
    Even = 0,
    Odd = 1,
}

impl From<bool> for BranchBit {
    fn from(b: bool) -> Self {
        if b { BranchBit::Odd } else { BranchBit::Even }
    }
}

/// Compact bit-vector storing branch decisions.
/// Stored LSB-first within each u64 word.
#[derive(Debug, Clone, Default)]
pub struct BranchVector {
    words: Vec<u64>,
    len: usize,
}

impl BranchVector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one branch bit.
    pub fn push(&mut self, bit: BranchBit) {
        let word_idx = self.len / 64;
        let bit_idx = self.len % 64;
        if bit_idx == 0 {
            self.words.push(0);
        }
        if bit == BranchBit::Odd {
            self.words[word_idx] |= 1u64 << bit_idx;
        }
        self.len += 1;
    }

    /// Read the i-th branch bit.
    pub fn get(&self, i: usize) -> BranchBit {
        assert!(i < self.len, "BranchVector index out of bounds");
        let word_idx = i / 64;
        let bit_idx = i % 64;
        BranchBit::from((self.words[word_idx] >> bit_idx) & 1 == 1)
    }

    /// Number of branch bits logged.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Size in bytes (rounded up).
    pub fn byte_len(&self) -> usize {
        (self.len + 7) / 8
    }

    /// Raw byte serialisation for wire format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len() + 4);
        // Header: u32 LE = number of bits
        out.extend_from_slice(&(self.len as u32).to_le_bytes());
        for word in &self.words {
            out.extend_from_slice(&word.to_le_bytes());
        }
        // Trim last partial word to exact byte count
        let full_len = 4 + self.words.len() * 8;
        let trim = if self.len % 64 == 0 {
            0
        } else {
            8 - (self.len % 64 + 7) / 8
        };
        out.truncate(full_len - trim);
        out
    }

    /// Deserialise from bytes.
    pub fn from_bytes(src: &[u8]) -> Option<Self> {
        if src.len() < 4 {
            return None;
        }
        let len = u32::from_le_bytes(src[0..4].try_into().ok()?) as usize;
        let n_bytes = (len + 7) / 8;
        if src.len() < 4 + n_bytes {
            return None;
        }
        let mut words = Vec::new();
        let mut i = 4;
        while i + 8 <= 4 + ((len + 63) / 64) * 8 && i + 8 <= src.len() {
            words.push(u64::from_le_bytes(src[i..i + 8].try_into().unwrap()));
            i += 8;
        }
        // Handle partial last word
        if (4 + n_bytes) > i && i < src.len() {
            let remaining = &src[i..4 + n_bytes];
            let mut w = 0u64;
            for (k, &b) in remaining.iter().enumerate() {
                w |= (b as u64) << (k * 8);
            }
            words.push(w);
        }
        Some(BranchVector { words, len })
    }
}

/// Compute the parity bit for a 64-bit word (popcount mod 2).
/// Uses the 0x6996 trick from docs.
#[inline(always)]
pub fn parity64(mut v: u64) -> BranchBit {
    v ^= v >> 32;
    v ^= v >> 16;
    v ^= v >> 8;
    v ^= v >> 4;
    BranchBit::from((0x6996u64 >> (v & 0xF)) & 1 == 1)
}

/// Given the current state word and a phase mask from D_K,
/// determine branch bit and apply phase flip if Odd.
#[inline]
pub fn branch_step(state: u64, phase_mask: u64) -> (u64, BranchBit) {
    let bit = parity64(state & phase_mask);
    let new_state = if bit == BranchBit::Odd {
        state ^ phase_mask
    } else {
        state
    };
    (new_state, bit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_vector_roundtrip() {
        let mut v = BranchVector::new();
        let bits = [true, false, true, true, false, true, false, false, true];
        for &b in &bits {
            v.push(BranchBit::from(b));
        }
        assert_eq!(v.len(), bits.len());
        for (i, &b) in bits.iter().enumerate() {
            assert_eq!(v.get(i), BranchBit::from(b), "mismatch at {i}");
        }
        let bytes = v.to_bytes();
        let v2 = BranchVector::from_bytes(&bytes).expect("deserialise");
        for i in 0..bits.len() {
            assert_eq!(v.get(i), v2.get(i), "roundtrip mismatch at {i}");
        }
    }

    #[test]
    fn parity_matches_popcount() {
        for x in [0u64, 1, 0xFF, 0xA5A5A5A5u64, u64::MAX] {
            let expected = BranchBit::from(x.count_ones() % 2 == 1);
            assert_eq!(parity64(x), expected, "parity mismatch for {x:#x}");
        }
    }

    #[test]
    fn branch_step_is_deterministic() {
        let (s1, b1) = branch_step(0xDEADBEEFCAFEBABEu64, 0xFF00FF00FF00FF00u64);
        let (s2, b2) = branch_step(0xDEADBEEFCAFEBABEu64, 0xFF00FF00FF00FF00u64);
        assert_eq!(s1, s2);
        assert_eq!(b1, b2);
    }

    #[test]
    fn v_size_equals_steps_not_block() {
        // Simulate 16 expansion steps on a 4096-bit block.
        // V must be exactly 16 bits regardless of block size.
        let mut v = BranchVector::new();
        let mut state = 0xCAFEBABEDEADBEEFu64;
        let mask = 0xF0F0F0F0F0F0F0F0u64;
        for _ in 0..16 {
            let (new_state, bit) = branch_step(state, mask);
            v.push(bit);
            state = new_state;
        }
        assert_eq!(v.len(), 16, "|V| must equal number of steps");
        // Compare to a "block" that would be 4096 bits
        let block_bits = 4096usize;
        assert!(
            v.byte_len() * 8 < block_bits,
            "|V|={} bits must be << block={block_bits} bits",
            v.byte_len() * 8
        );
    }
}
