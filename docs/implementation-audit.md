# Implementation Audit

**Date:** 2026-06-13
**Scope:** second remediation pass for `docs/review-claude.md`

## Real encode strategies

`encode_block` now evaluates four real candidates:

1. compact spectral;
2. operator parity trajectory;
3. direct parity trajectory;
4. alphabet diagnostic;

Raw is retained when every encoded candidate is larger.

## Review remediation

| Review finding | Current implementation | Regression test |
|---|---|---|
| Spectral existed only in the decoder | `try_encode_spectral_block` is called by `encode_block`; the real single-layer archive serializes and decodes that mode | `real_encode_path_emits_spectral_for_a_known_walsh_signal`, `public_archive_path_serializes_and_decodes_a_real_spectral_block` |
| Spectral predictor was a keyed PRNG | Removed `generate_operator_base_words` and the XorShift predictor; spectral output is synthesized from recorded Walsh coordinates, signs, and quantized amplitudes | `spectral_program_expands_a_walsh_basis_without_prng_state`, `spectral_magic_constant_expands_the_recorded_walsh_coordinates` |
| `K` was a 40-50 byte collection of segments | `MagicKey` is exactly one `u64`; the complete spectral program is bit-packed into that value | `k_is_exactly_one_64_bit_magic_constant`, `magic_constant_roundtrips_the_complete_spectral_program` |
| Operation codes were copied into the archive | Operation schedules are expanded locally from the shared base and the 64-bit root; no operation vector is serialized | `k_root_generates_the_same_file_base_for_both_sides`, `generated_base_schedule_is_executable_and_reversible` |
| Operator transformation was accepted without proving improvement | Operator mode compares the shortest original and transformed parity trajectories and is rejected unless `U_K` strictly reduces `V` | `operator_is_never_accepted_without_strictly_shorter_parity_trajectory` |
| Codec's `U_K` was a hand-written `reverse_bits` facade | Word codec calls the exact binary butterfly kernel `apply_binary_u_k`; the kernel is an involution | `binary_u_k_is_an_exact_involution_for_word_codec`, `operator_k_is_one_constant_and_u_k_is_an_involution` |
| Fixed 10% block threshold discarded cumulative gains | Block policy accepts any strict gain after complete `K + V + overhead` accounting; the recursive container still rejects non-shrinking layers | `hard_constraint_accepts_any_strict_gain_after_full_cost_accounting`, `recursive_layers_are_used_only_when_the_complete_archive_shrinks` |
| Synthetic quality tests hid the real result | Codec-generated `>=10x` fixtures were removed; metrics come from public CLI runs on files under `tests/` | benchmark commands below |

## Compact spectral layout

The 64-bit spectral `K` stores:

```text
3 × 13-bit Walsh indices
3 × 5-bit relative amplitudes
3 sign bits
2-bit peak count
4-bit block log2
1 tie bit
```

The decoder expands these fields into a weighted inverse-Walsh approximation.
`V` stores canonical, increasing bit-exception addresses as delta ULEB128 values.
The mode is rejected when those addresses do not produce a strict total gain.

## Current measurements

```text
tests/noise/*: 29,722 -> 26,158 bytes, 1.1362x, roundtrip=true
tests/test1.txt: 24,465 -> 10,963 bytes, 2.2316x, roundtrip=true
tests/test2.txt: 22,673 -> 10,178 bytes, 2.2276x, roundtrip=true
```

The base64-noise result is still produced by the alphabet strategy. Spectral is
now a real encoder and wins on Walsh-structured blocks, but a three-peak
64-bit program does not yet describe the external noise corpus with a short
enough exception stream. The `>=10x` objective remains open.
