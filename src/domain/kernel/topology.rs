// FILE: src/domain/kernel/topology.rs

use crate::domain::kernel::key::{GenerativeBlueprint, MagicKey};
use crate::domain::kernel::spectral::{normalized_fwht, strongest_walsh_peaks};

pub fn compile_topology_to_blueprint(bytes: &[u8]) -> Result<MagicKey, String> {
    let bits: Vec<f32> = bytes.iter().flat_map(|b| (0..8).map(move |i| if (b >> i) & 1 == 1 { 1.0 } else { -1.0 })).collect();
    
    let mut signal = bits;
    normalized_fwht(&mut signal)?;
    let peaks = strongest_walsh_peaks(&signal, 4);

    let p0 = peaks.get(0).map(|p| p.index).unwrap_or(0);
    let p1 = peaks.get(1).map(|p| p.index).unwrap_or(0);

    let blueprint = GenerativeBlueprint {
        hadamard_log_n: (bytes.len() * 8).ilog2() as u8,
        hadamard_parity: (p0 & 0xFFFF) as u16,
        channel_m0: ((p0 >> 16) & 0x0F) as u8,
        channel_m1: (p1 & 0x0F) as u8,
        channel_m2: ((p1 >> 4) & 0x0F) as u8,
        channel_m3: ((p0 ^ p1) & 0x0F) as u8,
        clean_mask: ((p0 ^ p1) & 0x0F) as u8,
        reserved: 0,
    };

    MagicKey::from_blueprint(&blueprint)
}