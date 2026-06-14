//! Fast Walsh-Hadamard Transform — adaptive window, powers of two.
//!
//! Key properties:
//!   - H(H(x)) == x  (self-inverse)
//!   - Block size is NOT fixed: anything from 64 to 2^20 bits (must be power of 2)
//!   - Window sizing rule (called once per layer, stored in layer_map):
//!       spectral entropy low  (sharp peaks)  -> double window
//!       spectral entropy high (flat spectrum) -> halve window

use std::fmt;

/// Result of spectral analysis on one block.
#[derive(Debug, Clone)]
pub struct SpectralProfile {
    /// Block size in bits (always a power of two)
    pub window_bits: usize,
    /// Peaks sorted by |amplitude| descending: (index, amplitude)
    pub peaks: Vec<(usize, i64)>,
    /// Spectral entropy estimate (0.0 = single peak, 1.0 = perfectly flat)
    pub entropy: f64,
}

impl fmt::Display for SpectralProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SpectralProfile {{ window={} bits, top_peak={:?}, entropy={:.3} }}",
            self.window_bits,
            self.peaks.first(),
            self.entropy
        )
    }
}

/// In-place Fast Walsh-Hadamard Transform over a mutable i64 slice.
/// slice.len() must be a power of two.
pub fn fwht_inplace(data: &mut [i64]) {
    let n = data.len();
    debug_assert!(n.is_power_of_two(), "fwht requires power-of-two length");
    let mut step = 1usize;
    while step < n {
        let mut i = 0;
        while i < n {
            for j in i..i + step {
                let u = data[j];
                let v = data[j + step];
                data[j] = u + v;
                data[j + step] = u - v;
            }
            i += step * 2;
        }
        step *= 2;
    }
}

/// Convert a byte slice into a signed +1/-1 integer sequence (bit-level).
/// Bit value 1 → +1, bit value 0 → -1.
fn bytes_to_bipolar(bytes: &[u8], n_bits: usize) -> Vec<i64> {
    let mut out = Vec::with_capacity(n_bits);
    'outer: for byte in bytes {
        for b in 0..8 {
            if out.len() >= n_bits {
                break 'outer;
            }
            out.push(if (byte >> (7 - b)) & 1 == 1 { 1i64 } else { -1i64 });
        }
    }
    // Pad to n_bits if input was short
    while out.len() < n_bits {
        out.push(-1i64);
    }
    out
}

/// Analyse one block: compute FWHT, extract peaks.
/// `window_bits` must be a power of two and <= 8 * data.len().
pub fn analyse_block(data: &[u8], window_bits: usize) -> SpectralProfile {
    debug_assert!(
        window_bits.is_power_of_two(),
        "window_bits must be power of two"
    );
    let n = window_bits; // number of WHT coefficients
    let mut spectrum = bytes_to_bipolar(data, n);
    fwht_inplace(&mut spectrum);

    // Normalise so max amplitude is comparable across window sizes
    let norm = n as f64;

    // Collect (index, amplitude) sorted by |amplitude| desc
    let mut indexed: Vec<(usize, i64)> = spectrum.iter().cloned().enumerate().collect();
    indexed.sort_unstable_by_key(|&(_, a)| -a.unsigned_abs() as i64);

    // Spectral entropy: p_i = amp_i^2 / sum(amp^2)
    let sum_sq: f64 = indexed.iter().map(|&(_, a)| (a as f64 / norm).powi(2)).sum();
    let entropy = if sum_sq > 0.0 {
        let probs: Vec<f64> = indexed
            .iter()
            .map(|&(_, a)| {
                let p = (a as f64 / norm).powi(2) / sum_sq;
                p
            })
            .collect();
        let log_n = (n as f64).ln();
        let h: f64 = probs
            .iter()
            .filter(|&&p| p > 0.0)
            .map(|&p| -p * p.ln())
            .sum::<f64>();
        (h / log_n).clamp(0.0, 1.0)
    } else {
        1.0
    };

    SpectralProfile {
        window_bits,
        peaks: indexed,
        entropy,
    }
}

/// Adaptive window sizing decision for the *next* block.
/// Returns new window_bits (clamped to [min_bits, max_bits]).
/// Rule: sharp spectrum (low entropy) → double; flat → halve.
pub fn adapt_window(
    current: usize,
    entropy: f64,
    sharp_threshold: f64, // e.g. 0.3 — below this = double
    flat_threshold: f64,  // e.g. 0.7 — above this = halve
    min_bits: usize,
    max_bits: usize,
) -> usize {
    if entropy < sharp_threshold && current * 2 <= max_bits {
        current * 2
    } else if entropy > flat_threshold && current / 2 >= min_bits {
        current / 2
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fwht_self_inverse() {
        for &n in &[64usize, 128, 256, 1024, 4096] {
            let orig: Vec<i64> = (0..n).map(|i| (i as i64 % 7) - 3).collect();
            let mut data = orig.clone();
            fwht_inplace(&mut data);
            fwht_inplace(&mut data);
            // After two passes: data[i] == orig[i] * n (standard unnormalised WHT)
            for (i, (&got, &src)) in data.iter().zip(orig.iter()).enumerate() {
                assert_eq!(
                    got,
                    src * n as i64,
                    "self-inverse failed at index {i} for n={n}"
                );
            }
        }
    }

    #[test]
    fn uniform_block_has_single_dominant_peak() {
        let data = vec![0x3Cu8; 512]; // 512 bytes = 4096 bits
        let profile = analyse_block(&data, 4096);
        let top_amp = profile.peaks[0].1.unsigned_abs();
        let second_amp = profile.peaks[1].1.unsigned_abs();
        assert!(
            top_amp > second_amp * 10,
            "uniform block must have dominant single peak, got top={top_amp} second={second_amp}"
        );
        assert!(profile.entropy < 0.1, "uniform block must have very low entropy");
    }

    #[test]
    fn random_block_has_high_entropy() {
        // LCG noise
        let mut s = 0xDEADBEEFu64;
        let data: Vec<u8> = (0..512)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                (s >> 56) as u8
            })
            .collect();
        let profile = analyse_block(&data, 4096);
        assert!(
            profile.entropy > 0.5,
            "random block must have high spectral entropy, got {:.3}",
            profile.entropy
        );
    }

    #[test]
    fn window_doubles_on_sharp_spectrum() {
        let new_w = adapt_window(4096, 0.1, 0.3, 0.7, 64, 1 << 20);
        assert_eq!(new_w, 8192);
    }

    #[test]
    fn window_halves_on_flat_spectrum() {
        let new_w = adapt_window(4096, 0.9, 0.3, 0.7, 64, 1 << 20);
        assert_eq!(new_w, 2048);
    }
}
