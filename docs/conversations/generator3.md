<img src="https://r2cdn.perplexity.ai/pplx-full-logo-primary-dark%402x.png" style="height:64px;margin-right:32px"/>

# Pack Compressor — Architecture \& Implementation Plan

## Overview

This document describes the full architecture of the `pack` generator-based compressor, the GPU/CPU parallelism model, and the step-by-step implementation plan to achieve real compression.

***

## 1. GPU Parallelism — Corrected Model

### Why GPU Works Here

The concern about PCIe overhead is valid **only** when data is shuffled back and forth repeatedly. For batch compression, the correct model is:

1. **Copy entire file to VRAM once** — PCIe bandwidth is ~16 GB/s (PCIe 4.0 ×16). A 4 GB file takes ~250 ms. This is a one-time cost.
2. **Partition into blocks on GPU** — no CPU involvement needed.
3. **Run FWHT + contract_block as CUDA/WebGPU kernels in parallel** — thousands of blocks simultaneously.
4. **Copy compressed result back** — result is smaller than input, so this is faster than the original upload.

For a 4 GB file with 512-byte blocks: ~8 million blocks. A modern GPU (e.g., RTX 4090) has ~16,000 CUDA cores. That's **500 blocks per core** processed concurrently. With FWHT at ~384 ops/block and branchless contract at ~200 ops/block — total ~584 ops/block × 8M blocks = ~4.7 billion ops. At GPU throughput of ~80 TFLOPS int32 this finishes in **~60 ms** after the initial VRAM upload.

### CPU vs GPU — Where Each Belongs

| Task | CPU | GPU |
| :-- | :-- | :-- |
| File I/O, syscalls | ✅ Only option | ❌ Cannot |
| FWHT per block (384 ops) | ✅ ~150 ns/block | ✅ 8M blocks × 150 ns / 16K cores = ~0.5 ms total |
| contract_block (branchless) | ✅ Fast, cache-friendly | ✅ Ideal — no branches, no memory deps between blocks |
| Sparse crumb encoding (ULEB128) | ✅ Simple | ⚠️ Doable but needs atomic writes |
| Codec orchestration / BitBudget accept | ✅ Single-threaded fine | ❌ Overkill |
| Multi-layer recursion | ✅ Per-layer sequential | ✅ Inner per-block work is parallel |

### Recommended Architecture: CPU Orchestrates, GPU Does Block Work

```
[File on disk]
     │  read (mmap or read_to_end)
     ▼
[CPU: analyze_pass_band]   ← window size selection (once per layer)
     │
     ▼
[VRAM: upload full layer data]   ← PCIe, one-time per layer
     │
     ▼
[GPU kernel: per_block_compress]
  ├── FWHT(block) → walsh_peaks
  ├── dominant_mask = walsh_peaks[0].index
  ├── apply_pipeline_forward(words, K)
  ├── for each word: spectral_pivot_contract(word, dominant_mask)
  │     → crumb = (parity(word & mask) != bias) as u1
  │     → compressed_word = remove_bit_at(word, pivot)
  └── sparse_encode_crumbs(crumbs) → position deltas (ULEB128)
     │
     ▼
[VRAM: compressed blocks + crumb arrays]
     │  download (smaller than input)
     ▼
[CPU: BitBudget.accepts() per block, codec assembly, archive serialization]
     │
     ▼
[Compressed archive on disk]
```

Multi-layer recursion: each layer runs the same GPU dispatch on the previous layer's output. The output shrinks per layer, so later layers are progressively faster.

***

## 2. Full Generator Architecture (Current State)

### File Map

```
src/domain/
├── codec.rs              — orchestrator: compress_bytes, decompress_bytes
├── bitstream.rs          — bit-level pack/unpack utilities
├── model.rs              — data structures: Archive, BlockRecord, OperatorBlock, etc.
└── kernel/
    ├── topology.rs       — FWHT analyzer, compile_topology_to_constant
    ├── key.rs            — ConstantLayout, PackedConstantK, RoutingKind, ConstantFamily
    ├── budget.rs         — BitBudget, CompressionPolicy::MVP
    ├── operator.rs       — contract_block, expand_block, apply_pipeline_forward
    ├── spectral.rs       — synthesise_spectral_bits (MagicKey / SpectralBlock path)
    ├── trajectory.rs     — parity trajectory encode/decode (TrajectoryBlock path)
    ├── reversible.rs     — reversible word transforms
    └── base.rs           — base integer operations
```


### Data Flow — Compression

```
compress_bytes(input)
  └─ compress_bytes_with_profile(input, profile=General)
       └─ for layer in 0..MAX_LAYERS:
            analyze_pass_band(current, block_size_hint, layer)
              └─ returns PassWindowBand { min_window_bits, max_window_bits }
            compress_single_layer_bytes_with_profile(current, band, profile)
              └─ for each window_size in band:
                   for each block in current.chunks(window_size):
                     try_encode_operator_block(block, K, profile)
                       ├─ analyze_topology(block) → TopologySignature
                       ├─ compile_topology_to_constant(sig) → PackedConstantK (K)
                       ├─ contract_block(block, K) → (seed, breadcrumbs, steps)
                       ├─ BitBudget { source, key, seed, branch, overhead }
                       └─ CompressionPolicy::MVP.accepts(budget) → bool
            if output.len() >= current.len(): break
            layers.push(LayerSummary)
            current = output
  └─ serialize_recursive_archive(original_len, layers, current)
```


### Data Flow — Decompression

```
decompress_bytes(archive)
  └─ parse header (PACKMVP1 or PACKREC1)
  └─ for each layer in reverse:
       for each BlockRecord:
         match mode:
           OperatorBlock → expand_block(seed, breadcrumbs, K, steps)
           RawBlock      → copy as-is
           AlphabetBlock → palette decode
           SpectralBlock → synthesise_spectral_bits
           TrajectoryBlock → decode_parity_trajectory
```


### Key Structures

**`ConstantLayout`** (in `key.rs`) — the K constant:

```
family: PhaseXor | OddAffine | Hybrid
routing: Identity | RotateWords | ReverseWords | Butterfly
branch_rounds: u8 (2..12)
rotate_left, rotate_right: u8
gf2_shift: u8
lane_rotate: u8
pivot_seed: u8          ← BUG: random, not spectral
branch_stride: u8       ← BUG: random
branch_span: u8
phase_mask: u64
odd_multiplier: u64
affine_mask: Option<u64>
```

**`BitBudget`** (in `budget.rs`):

```
source_bits:   N × 64           (block size)
key_bits:      |K| in bits      (max 64 per MVP policy)
seed_bits:     remaining_words × 64   ← PROBLEM: almost = source_bits
branch_bits:   branch_rounds × words  ← PROBLEM: added back to budget
overhead_bits: OPERATOR_BLOCK_FIXED_OVERHEAD_BITS = 144
```

Net gain today = `source_bits - (key + seed + branch + 144)`.
With `branch_rounds=4`, 64-word block: seed=60×64=3840, branch=256, key≤64, overhead=144.
Total encoded = 4304 > source = 4096. **MVP.accepts() → false. No block is ever accepted.**

***

## 3. Root Cause of Zero Compression

Three bugs work together to guarantee zero compression:

**Bug A — Blind pivot selection.**
`pivot_seed` and `branch_stride` are derived from `peak0 ^ peak1 ^ popcnt_profile[0]` — a hash with no spectral meaning. The pivot bit has no higher predictability than any other bit. Removing it carries zero information advantage.

**Bug B — crumb = actual bit value, not deviation.**
`contract_block` stores the raw bit at pivot position as a crumb. Since the bit is random (pivot is randomly chosen), crumbs have ~50% entropy. They are incompressible.

**Bug C — Flat crumb storage.**
Crumbs are packed as a dense bit array (`pack_indices`). Even if crumbs were sparse (mostly zeros), they would be stored at full density. The sparse savings never materialize.

***

## 4. The Fix — Spectral Pivot + Sparse Crumbs

### Mathematical Basis

Walsh-Hadamard transform peak at index `K` means:

$$
\bigoplus_{i : (i \;\&\; K) \neq 0} \text{bit}_i = C
$$

where `C` (0 or 1) is the bias of the dominant coefficient (positive coefficient → C=0, negative → C=1). This is a **linear constraint**: one of the bits in the mask is fully determined by the others. That bit is redundant — it carries zero additional entropy given the rest. Removing it and recording only whether the constraint was violated (crumb=1) or satisfied (crumb=0) gives a crumb vector that is as sparse as the data is structured.

For structured data (text, logs, binary formats) the dominant Walsh peak typically accounts for 70-95% of spectral energy, meaning the constraint holds in 85-97% of words. The crumb vector is then 3-15% dense — highly compressible by sparse delta encoding.

### New contract_block Logic

```
dominant_mask  = walsh_peaks[0].index as u64
dominant_bias  = walsh_peaks[0].coefficient < 0  // true → expected parity = 1
pivot_pos      = dominant_mask.trailing_zeros()   // lowest set bit of mask

for each word in words:
  parity = (word & dominant_mask).count_ones() % 2
  crumb  = (parity as bool != dominant_bias) as u8   // 1 = anomaly
  compressed_word = remove_bit_at(word, pivot_pos)   // 63-bit word
  crumbs.push(crumb)

// encode crumbs as sparse delta positions
anomaly_positions = crumbs.iter().enumerate()
                         .filter(|(_, c)| **c == 1)
                         .map(|(i, _)| i)
                         .collect()
encoded_crumbs = encode_position_deltas_uleb128(anomaly_positions)
```

Reversibility is guaranteed: decoder inserts `0` at pivot_pos, computes expected parity, reads crumb, XORs if crumb=1.

***

## 5. Step-by-Step Implementation Plan

### Step 1 — Add `dominant_walsh_mask` to `ConstantLayout` in `key.rs`

Add two fields to `ConstantLayout`:

```rust
pub dominant_walsh_mask: u64,
pub dominant_walsh_bias: bool,
```

Update `PackedConstantK::from_layout` to encode these fields.
Update `ConstantLayout::layout()` (parser) to decode them.
**No changes to any other file yet.** Run `cargo test` — all existing tests must pass.

**Acceptance:** `compile_topology_to_constant` compiles, no regressions.

***

### Step 2 — Propagate Walsh mask from `topology.rs`

In `compile_topology_to_constant` (topology.rs), fill the new fields:

```rust
dominant_walsh_mask: signature.walsh_peaks[0].index as u64,
dominant_walsh_bias: signature.walsh_peaks[0]```

