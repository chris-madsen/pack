# Implementation Audit

**Date:** 2026-06-13
**Scope:** remediation of `docs/review-claude.md`

## Implemented execution path

The operator path is now:

```text
block
  -> topology analysis over the complete block
  -> compact K (<= 512 bits)
  -> exact self-inverse word-level U_K
  -> shared terminal + parity trajectory V
  -> strict K + V + overhead budget
```

`K` contains five canonical segments:

1. `RevMix`
2. `PhaseMask`
3. `WalshConfig`
4. `Program`
5. `AuxConst`

The decoder reads and uses every segment. `Program` contains operation codes
from the versioned shared base. Directly irreversible instructions are rejected.

## Review remediation

| Review finding | Remediation | Regression tests |
|---|---|---|
| `OperationCode` and generated base were decorative | Generated schedules execute through `GeneratedBase::apply_forward/apply_inverse`; codec parses and executes `Program` | `generated_base_schedule_is_executable_and_reversible`, `changing_generated_schedule_changes_execution`, `every_operator_key_segment_changes_executable_output` |
| Magic key did not describe an algorithm | Added canonical `Program` segment; program length equals `K.header.rounds`; all instructions are validated | `hybrid_k_roundtrips_canonically_and_is_versioned`, `operator_program_rejects_non_reversible_direct_operations` |
| Operator parameters were hard-coded | `build_operator_codec_key` is now exactly `analyze_topology -> compile_topology_to_key` | `changing_block_topology_changes_compiled_k`, `every_topology_profile_materially_changes_compiled_k` |
| Seed used only the first eight bytes | `AuxConst` uses a fingerprint folded over every block chunk | `block_fingerprint_depends_on_the_entire_block`, `operator_key_depends_on_the_entire_block_not_only_its_prefix` |
| XorShift predictor ignored Walsh data | Removed the standalone XorShift predictor and duplicate spectral encoder; predictor generation executes `Program`, Feistel, and `U_K` | `every_operator_key_segment_changes_executable_output`, `spectral_block_roundtrips_through_real_archive` |
| Operator accepted only exact synthetic matches except one bit | Operator applies exact `U_K`, records a shared terminal as overhead, and stores only parity branch bits in `V` | `real_operator_word_u_k_is_an_involution`, `operator_v_is_a_parity_trajectory_inside_the_u_k_path`, `operator_archive_rejects_noncanonical_parity_stream` |
| Zero-valued minimal spectral fallback | Removed | covered by the absence of a fallback path and topology-key tests |
| Popcount data was serialized and ignored | Popcount contributes to program synthesis and multiplier selection | `every_topology_profile_materially_changes_compiled_k` |
| Spectrum/derivative hashes were used as opaque parameters | Removed those fields; semantic features select masks, shifts, multipliers, and the program | `topology_compiler_is_deterministic_and_uses_all_required_profiles` |
| Feistel was dead code | Codec predictor generation calls `feistel_forward` in both state preparation and output generation | `every_operator_key_segment_changes_executable_output`, Feistel unit tests |
| Synthetic `>=10x` tests hid poor external results | Removed all codec-generated quality fixtures; quality is measured through CLI benchmarks | `docs/test-contract-matrix.md` |

## Current measured quality

Release benchmark command:

```bash
target/release/pack benchmark tests/noise
```

Results for each of 15 files:

```text
29,722 bytes -> 26,158 bytes
packed/original = 0.8801
compression factor = 1.1362x
roundtrip = true
```

Additional external files:

```text
test1.txt: 24,465 -> 10,963 bytes, 2.2316x, roundtrip=true
test2.txt: 22,673 -> 10,178 bytes, 2.2276x, roundtrip=true
```

The target `>=10x` result is **not achieved**. The current noise result is still
produced by the alphabet strategy; the executable operator path is now real but
does not yet synthesize a sufficiently short `V` for this corpus.

## Remaining research gap

The code no longer fakes operator execution or substitutes an XOR residual for
parity breadcrumbs. The metagenerator still needs a stronger constructive rule
that maps arbitrary external blocks to an executable program whose transformed
words share a short trajectory. Adding format-specific predictors or generating
test data from the decoder is not an acceptable solution.
