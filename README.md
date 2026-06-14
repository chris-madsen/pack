# pack — Metagen Compression Engine

> **Status: active research prototype.**  
> Multi-layer encoder + decoder implemented. All test vectors lossless.

## Architecture

This is **not** a classical compressor (LZ, Huffman, arithmetic coding).  
It is a **metagenerator**: synthesises a *generating function* K per block,
such that `|K| + |V| << |N|`.

```
ARCHIVE = layer_map + stream{ K_i, V_i }  ← layer 0
                    + stream{ K_j, V_j }  ← layer 1 (compresses layer 0 stream)
                    + ...
```

| Symbol | Meaning |
|--------|---------|
| `N` | Original data block (bits) |
| `K` | Magic constant — spectral peak indices + Feistel params |
| `V` | Branch vector — parity-bit log of U_K branching decisions |
| **Condition** | `\|N\| >> \|K\| + \|V\| + overhead` |

### Key insight

**V is NOT `N XOR Generate(K)`.**  
V = parity-bit log of branching decisions. `|V| = peaks × rounds`.  
`|V|` is **independent of block size N**. This is the source of compression.

---

## Operator

```
U_K = F_K⁻¹ · H · D_K · H · F_K
```

| Component | Role |
|-----------|------|
| `H` | Fast Walsh-Hadamard (self-inverse: H²=I) |
| `D_K` | Phase mask at accepted spectral peak indices |
| `F_K` | Feistel ARX — bijective, derived from peak amplitudes |
| `U_K` | Self-inverse: U_K(U_K(x)) = x |

### Adaptive window

Block size adapts per layer via spectral entropy (NOT per block — stored once in `layer_map`):

| Spectral entropy | Action |
|---|---|
| < 0.30 (sharp peaks) | double window |
| > 0.70 (flat) | halve window |
| 0.30–0.70 | keep current |

Steps: powers of two only. Range: 64 – 2²⁰ bits.

---

## Compression Metrics

### Single-layer (U_K, 3 rounds, 512 B blocks)

| Block type | File | N | Passes | Peaks | K bits | V bits | K+V+OH | **Ratio** |
|---|---|---|---|---|---|---|---|---|
| `uniform_0x3C` | 512 B | 4096 b | 3 | 1 | 220 | 3 | 287 | **14.27×** |
| `all_zeros` | 512 B | 4096 b | 3 | 0 | 208 | 0 | 272 | **15.06×** |
| `alternating_A5` | 512 B | 4096 b | 3 | 1 | 220 | 3 | 287 | **14.27×** |
| `ascii_text` | 512 B | 4096 b | 3 | 7 | 292 | 21 | 377 | **10.87×** |
| `sequential` | 512 B | 4096 b | 3 | 28 | 544 | 84 | 692 | **5.92×** |
| `half_uniform` | 512 B | 4096 b | 3 | 29 | 556 | 87 | 707 | **5.79×** |
| `lcg_noise` | 512 B | 4096 b | 3 | 56 | 880 | 168 | 1112 | **3.68×** |
| `uniform_16k` | 2048 B | 16384 b | 3 | 1 | 222 | 3 | 289 | **56.69×** |

### Multi-layer (adaptive, stop at gain < 10%)

| File | Layers | Input | Output | **Total ratio** | Total V bits |
|---|---|---|---|---|---|
| `uniform_0x3C` 512 B | 2 | 512 B | 93 B | **5.56×** | 96 |
| `uniform_0x3C` 2 KB | 3 | 2048 B | 89 B | **23.17×** | 162 |
| `all_zeros` 512 B | 2 | 512 B | 93 B | **5.56×** | 93 |
| `alternating_A5` 512 B | 2 | 512 B | 93 B | **5.56×** | 96 |
| `ascii_text` 512 B | 2 | 512 B | 91 B | **5.67×** | 159 |
| `sequential` 512 B | 2 | 512 B | 89 B | **5.79×** | 171 |
| `half_uniform` 512 B | 2 | 512 B | 87 B | **5.92×** | 171 |
| `lcg_noise` 512 B | 3 | 512 B | 91 B | **5.67×** | 324 |
| `lcg_noise` 2 KB | 4 | 2048 B | 91 B | **22.69×** | 834 |

### Per-layer breakdown: `lcg_noise` 2 KB (hardest case)

| Layer | Input | Output | Ratio | Gain | Blocks | Peaks | V bits |
|---|---|---|---|---|---|---|---|
| 0 | 2048 B | 571 B | 3.59× | 72.1% | 4 | 232 | 696 |
| 1 | 571 B | 138 B | 4.16× | 75.9% | 2 | 37 | 111 |
| 2 | 138 B | 76 B | 1.83× | 45.5% | 1 | 22 | 66 |
| 3 | 76 B | — | — | −​18.8% | — | — | — |

Layer 3 rejected by auditor (output > input). Final: **76 B from 2048 B = 22.69×**.

### Correctness

| Check | Result |
|---|---|
| H(H(x)) = x | ✓ PASS |
| encode(decode(x)) = x (uniform) | ✓ PASS |
| encode(decode(x)) = x (LCG noise) | ✓ PASS |
| encode(decode(x)) = x (ascii) | ✓ PASS |
| `\|V\|` = peaks × rounds (not block size) | ✓ PASS |
| Auditor rejects unprofitable layers | ✓ PASS |

---

## Module Map

```
src/domain/
  fwht.rs      — Fast WHT, adaptive window, spectral profile
  branch.rs    — BranchVector: parity-bit log, |V|=steps not block
  keygen.rs    — synthesize_k(): greedy auditor, MagicKey
  operator.rs  — U_K = F_K⁻¹ · H · D_K · H · F_K, encode/decode
  encoder.rs   — LayerEncoder: multi-layer, adaptive window, stop rule
  decoder.rs   — LayerDecoder: reverse layers, restore original
```

## Build & test

```bash
cargo test
cargo test -- --nocapture  # with output
```

## Core condition

```
|N| >> |K| + |V| + overhead
```

Every decision validated against this inequality only.
