//! Metagenerator: derive a compact descriptor K from a slice of 64-bit words
//! by finding the three strongest Walsh-Hadamard Transform peaks and packing
//! their indices, amplitudes, and signs into a single u64.
//!
//! K layout (64 bits)
//! ───────────────────
//!  [12.. 0]  walsh_idx_0   13 bits  (0..8191)
//!  [25..13]  walsh_idx_1   13 bits
//!  [38..26]  walsh_idx_2   13 bits
//!  [43..39]  amp_0          5 bits  (0..31, scaled)
//!  [48..44]  amp_1          5 bits
//!  [53..49]  amp_2          5 bits
//!  [54]      sign_0         1 bit
//!  [55]      sign_1         1 bit
//!  [56]      sign_2         1 bit
//!  [58..57]  peak_count     2 bits  (1..=3 stored as 0..=2)
//!  [62..59]  block_log2     4 bits  log2(window_bits), 6..=12
//!  [63]      tie_bit        1 bit   reserved / tie-break

/// Maximum number of Walsh peaks encoded in K.
pub const MAX_PEAKS: usize = 3;

/// Packed metagenerator descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetaK {
    /// Raw packed u64.
    pub raw: u64,
}

/// One Walsh peak.
#[derive(Clone, Copy, Debug)]
pub struct WalshPeak {
    pub index: u16,
    pub amplitude: i64,
}

impl MetaK {
    /// Pack up to 3 peaks + block_log2 into K.
    pub fn pack(peaks: &[WalshPeak], block_log2: u8) -> Self {
        debug_assert!(block_log2 >= 6 && block_log2 <= 12);
        let count = peaks.len().min(MAX_PEAKS);
        let mut raw: u64 = 0;

        // Find max amplitude for scaling.
        let max_amp = peaks
            .iter()
            .take(count)
            .map(|p| p.amplitude.unsigned_abs())
            .max()
            .unwrap_or(1)
            .max(1);

        let offsets = [0u64, 13, 26];
        let amp_offsets = [39u64, 44, 49];
        let sign_offsets = [54u64, 55, 56];

        for i in 0..count {
            let p = &peaks[i];
            let idx = (p.index as u64) & 0x1FFF;
            raw |= idx << offsets[i];

            let amp_scaled = ((p.amplitude.unsigned_abs() * 31 / max_amp) as u64) & 0x1F;
            raw |= amp_scaled << amp_offsets[i];

            if p.amplitude < 0 {
                raw |= 1 << sign_offsets[i];
            }
        }

        // peak_count field: store count-1 in 2 bits
        let peak_count_field = (count.saturating_sub(1) as u64) & 0x3;
        raw |= peak_count_field << 57;

        // block_log2 in bits [62..59]
        raw |= ((block_log2 as u64) & 0xF) << 59;

        MetaK { raw }
    }

    /// Unpack K → peaks + block_log2.
    pub fn unpack(self) -> (Vec<WalshPeak>, u8) {
        let raw = self.raw;
        let peak_count = (((raw >> 57) & 0x3) as usize) + 1;
        let block_log2 = ((raw >> 59) & 0xF) as u8;

        let offsets = [0u64, 13, 26];
        let amp_offsets = [39u64, 44, 49];
        let sign_offsets = [54u64, 55, 56];

        let mut peaks = Vec::with_capacity(peak_count);
        for i in 0..peak_count {
            let index = ((raw >> offsets[i]) & 0x1FFF) as u16;
            let amp_scaled = ((raw >> amp_offsets[i]) & 0x1F) as i64;
            let sign = ((raw >> sign_offsets[i]) & 1) as i64;
            let amplitude = if sign == 1 { -amp_scaled } else { amp_scaled };
            peaks.push(WalshPeak { index, amplitude });
        }
        (peaks, block_log2)
    }

    /// Primary Walsh index — used as pivot seed.
    pub fn primary_walsh_index(&self) -> u16 {
        (self.raw & 0x1FFF) as u16
    }

    /// block_log2 field → window size in bits.
    pub fn window_bits(&self) -> usize {
        1 << (((self.raw >> 59) & 0xF) as usize)
    }
}

/// Compute Walsh-Hadamard spectrum of a popcnt profile over `words`.
/// Returns a Vec<i64> of length `words.len()` (in-place WHT).
pub fn walsh_spectrum(words: &[u64]) -> Vec<i64> {
    let n = words.len();
    // Build popcnt profile: for each word, count set bits.
    let mut f: Vec<i64> = words
        .iter()
        .map(|w| w.count_ones() as i64)
        .collect();
    // In-place Fast Walsh-Hadamard Transform.
    let mut len = 1usize;
    while len < n {
        let mut i = 0;
        while i < n {
            for j in i..i + len {
                let u = f[j];
                let v = f[j + len];
                f[j] = u + v;
                f[j + len] = u - v;
            }
            i += len * 2;
        }
        len *= 2;
    }
    f
}

/// Find top-`MAX_PEAKS` peaks by |amplitude| in the WHT spectrum.
/// Excludes index 0 (DC component) unless it's the only option.
pub fn find_peaks(spectrum: &[i64]) -> Vec<WalshPeak> {
    if spectrum.is_empty() {
        return Vec::new();
    }
    let mut indexed: Vec<(usize, i64)> = spectrum
        .iter()
        .copied()
        .enumerate()
        .filter(|(i, _)| *i != 0 || spectrum.len() == 1)
        .collect();
    indexed.sort_by_key(|(_, amp)| -amp.unsigned_abs() as i64);
    indexed
        .into_iter()
        .take(MAX_PEAKS)
        .map(|(index, amplitude)| WalshPeak {
            index: index.min(0x1FFF) as u16,
            amplitude,
        })
        .collect()
}

/// Main entry: derive MetaK from a word slice + chosen block_log2.
/// Returns None if `words` is empty or not a power-of-two length.
pub fn derive_meta_k(words: &[u64], block_log2: u8) -> Option<MetaK> {
    let n = words.len();
    if n == 0 || !n.is_power_of_two() {
        return None;
    }
    let spectrum = walsh_spectrum(words);
    let peaks = find_peaks(&spectrum);
    if peaks.is_empty() {
        return None;
    }
    Some(MetaK::pack(&peaks, block_log2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let peaks = vec![
            WalshPeak { index: 7, amplitude: 120 },
            WalshPeak { index: 3, amplitude: -80 },
            WalshPeak { index: 15, amplitude: 40 },
        ];
        let k = MetaK::pack(&peaks, 9);
        let (unpacked, log2) = k.unpack();
        assert_eq!(log2, 9);
        assert_eq!(unpacked.len(), 3);
        assert_eq!(unpacked[0].index, 7);
        assert!(unpacked[1].amplitude < 0);
    }

    #[test]
    fn uniform_words_have_single_dominant_peak() {
        let words = vec![0x3C3C3C3C3C3C3C3Cu64; 8];
        let k = derive_meta_k(&words, 9).unwrap();
        assert_eq!(k.window_bits(), 512);
    }

    #[test]
    fn window_bits_matches_log2() {
        for log2 in 6u8..=12 {
            let peaks = vec![WalshPeak { index: 1, amplitude: 1 }];
            let k = MetaK::pack(&peaks, log2);
            assert_eq!(k.window_bits(), 1 << log2);
        }
    }
}
