pub fn ceil_div_u64(value: u64, divisor: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    ((value - 1) / divisor) + 1
}

pub fn bit_width_for_cardinality(cardinality: usize) -> u8 {
    match cardinality {
        0 | 1 => 0,
        n => (usize::BITS - (n - 1).leading_zeros()) as u8,
    }
}

pub fn pack_indices(indices: &[u8], bit_width: u8) -> Vec<u8> {
    let total_bits = indices.len() as u64 * bit_width as u64;
    let mut out = vec![0_u8; ceil_div_u64(total_bits, 8) as usize];
    let mut bit_offset = 0_usize;

    for &value in indices {
        for bit_index in 0..bit_width {
            let bit = (value >> bit_index) & 1;
            let absolute_bit = bit_offset + bit_index as usize;
            if bit == 1 {
                let byte_index = absolute_bit / 8;
                let bit_in_byte = absolute_bit % 8;
                out[byte_index] |= 1 << bit_in_byte;
            }
        }
        bit_offset += bit_width as usize;
    }

    out
}

pub fn unpack_indices(packed: &[u8], bit_width: u8, count: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(count);
    let mut bit_offset = 0_usize;

    for _ in 0..count {
        let mut value = 0_u8;
        for bit_index in 0..bit_width {
            let absolute_bit = bit_offset + bit_index as usize;
            let byte_index = absolute_bit / 8;
            let bit_in_byte = absolute_bit % 8;
            let byte = packed
                .get(byte_index)
                .ok_or_else(|| "packed breadcrumb stream is truncated".to_string())?;
            let bit = (byte >> bit_in_byte) & 1;
            value |= bit << bit_index;
        }
        out.push(value);
        bit_offset += bit_width as usize;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{bit_width_for_cardinality, pack_indices, unpack_indices};

    #[test]
    fn roundtrip_pack_unpack_indices() {
        let input = vec![0_u8, 1, 2, 3, 0, 2, 1, 3, 1, 0];
        let packed = pack_indices(&input, 2);
        let unpacked = unpack_indices(&packed, 2, input.len()).unwrap();
        assert_eq!(input, unpacked);
    }

    #[test]
    fn computes_bit_width() {
        assert_eq!(bit_width_for_cardinality(1), 0);
        assert_eq!(bit_width_for_cardinality(2), 1);
        assert_eq!(bit_width_for_cardinality(3), 2);
        assert_eq!(bit_width_for_cardinality(10), 4);
        assert_eq!(bit_width_for_cardinality(17), 5);
    }

    #[test]
    fn singleton_alphabet_requires_no_breadcrumb_bits() {
        let input = vec![0_u8; 4096];
        let packed = pack_indices(&input, 0);
        let unpacked = unpack_indices(&packed, 0, input.len()).unwrap();
        assert!(packed.is_empty());
        assert_eq!(input, unpacked);
    }
}
