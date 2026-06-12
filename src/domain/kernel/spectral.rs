#[derive(Clone, Debug, PartialEq)]
pub struct SpectralPeak {
    pub index: usize,
    pub coefficient: f64,
}

pub fn normalized_fwht(values: &mut [f64]) -> Result<(), String> {
    if values.is_empty() || !values.len().is_power_of_two() {
        return Err("FWHT length must be a non-zero power of two".to_string());
    }

    let inv_sqrt_two = std::f64::consts::FRAC_1_SQRT_2;
    let mut width = 1;
    while width < values.len() {
        for start in (0..values.len()).step_by(width * 2) {
            for offset in 0..width {
                let left = values[start + offset];
                let right = values[start + offset + width];
                values[start + offset] = (left + right) * inv_sqrt_two;
                values[start + offset + width] = (left - right) * inv_sqrt_two;
            }
        }
        width *= 2;
    }
    Ok(())
}

pub fn strongest_walsh_peaks(spectrum: &[f64], count: usize) -> Vec<SpectralPeak> {
    let mut peaks = spectrum
        .iter()
        .copied()
        .enumerate()
        .map(|(index, coefficient)| SpectralPeak { index, coefficient })
        .collect::<Vec<_>>();
    peaks.sort_by(|left, right| {
        right
            .coefficient
            .abs()
            .total_cmp(&left.coefficient.abs())
            .then_with(|| left.index.cmp(&right.index))
    });
    peaks.truncate(count.min(peaks.len()));
    peaks
}

#[cfg(test)]
mod tests {
    use super::{normalized_fwht, strongest_walsh_peaks};

    fn assert_close(left: &[f64], right: &[f64]) {
        assert_eq!(left.len(), right.len());
        for (index, (a, b)) in left.iter().zip(right).enumerate() {
            assert!((a - b).abs() < 1e-9, "index {index}: {a} != {b}");
        }
    }

    #[test]
    fn fwht_is_an_involution_and_preserves_energy() {
        let original = vec![1.0, -1.0, 1.0, 1.0, -1.0, -1.0, 1.0, -1.0];
        let original_energy = original.iter().map(|value| value * value).sum::<f64>();
        let mut transformed = original.clone();
        normalized_fwht(&mut transformed).unwrap();
        let transformed_energy = transformed.iter().map(|value| value * value).sum::<f64>();
        assert!((original_energy - transformed_energy).abs() < 1e-9);
        normalized_fwht(&mut transformed).unwrap();
        assert_close(&transformed, &original);
    }

    #[test]
    fn constant_signal_has_only_dc_walsh_peak() {
        let mut values = vec![1.0; 8];
        normalized_fwht(&mut values).unwrap();
        assert!((values[0] - 8_f64.sqrt()).abs() < 1e-9);
        assert!(values[1..].iter().all(|value| value.abs() < 1e-9));
    }

    #[test]
    fn walsh_peaks_use_absolute_strength_and_canonical_ties() {
        let peaks = strongest_walsh_peaks(&[2.0, -5.0, 5.0, 1.0], 3);
        assert_eq!(
            peaks.iter().map(|peak| peak.index).collect::<Vec<_>>(),
            vec![1, 2, 0]
        );
    }

    #[test]
    fn fwht_rejects_non_power_of_two_lengths() {
        assert!(normalized_fwht(&mut [1.0, 2.0, 3.0]).is_err());
    }
}
