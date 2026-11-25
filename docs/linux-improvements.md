# Linux improvements — 2026-11-25

The normal-visuals checkpoint is preserved as `linux-world-visuals-2026-11-25`.
This integration prioritizes CPU performance, progression and model data.
Original commit authorship and cherry-pick provenance are retained.

## Selected changes

| Source | Imported behavior | Local integration |
|---|---|---|
| [PR989](https://github.com/fxceless/victory-fork/pull/989), `e72eb03` | Bound texture detiling to the upload allocation, which may omit final-row padding | BC7 mip/array, volume and zero-fill canary regressions |
| [PR1032](https://github.com/fxceless/victory-fork/pull/1032), `d919676` | Fault-guarded CPU write-back before kernel fallback | Default off; handle source/destination faults, retry the complete range, test holes and remapped prefixes |
| [PR927](https://github.com/fxceless/victory-fork/pull/927), through `b9c3781` | Preserve typed 16-bit values through UINT companion buffer views | Retain residency-aware table keys; prepare interior UINT slots; validate cache identity and bounds |
| [PR959](https://github.com/fxceless/victory-fork/pull/959), audio subset only | Bounded decoding, ring-index normalization, loop-start skips, unaligned output handling, cached transform rotations and byte gathering | Keep the stricter continuation parser from [PR627](https://github.com/fxceless/victory-fork/pull/627), `b786897`; add boundary regressions |

The eight audio source commits are `fcbb3cbc`, `a84e3289`, `1ff9486a`,
`be48d796`, `1765093b`, `9f974a2d`, `f333ac72` and `c24e4014`.
The other GPU, watchdog and page-pool changes bundled in PR959 were not imported.
A generic failure-only allocation diagnostic records native errors without
changing the caller's last-error value.

## Validation

Environment: CachyOS, Ryzen 7 9800X3D, RTX 5090, NVIDIA 620.73.04, GE-Proton 11-14,
Rust 1.98.1 and the [pinned external runtime](linux-runtime.md). All runs are
offline with isolated writable data and read-only title input.

- Native shaders: **185 passed, 2 ignored**.
- Windows shaders: **234 passed, 1 known stub-pipeline failure, 3 ignored**.
- GPU: **211 passed, 4 known failures, 8 ignored**. The failures remain the
  driver-event subscription and three pipeline-library expectations.
- Native formats: **35 passed, 5 ignored**, including malformed XMA input,
  packet traversal, scalar/byte gathering and bitwise transform equivalence.
- Full HLE: **104 passed, 1 known failure** (`WSAConnectByList` is absent in Wine).
  Audio/APU boundaries, ring wrapping, loop skips and allocation-error preservation pass.
- Diagnostic application build, all-target/all-feature Clippy and asset policy pass.

The GPU suite includes exhaustive half-pattern readback: **65,536/65,536**
patterns preserved by integer views, versus **1/65,536** through the float
conversion control. See [half-view integration](pr927-integration.md) for
binding details and limits. Mixed or dynamically aliased descriptor accesses
use the existing float path; exact half-bit preservation is limited to proven
half-only selections. GPU tests precede the audio import; formats/HLE were
validated afterward. Both complete shader suites were rerun after the final
access-analysis corrections. Four new regressions fail when the old behavior
is restored; the pure-path and dead-rewrite controls still pass.

## Guarded-copy measurement

The ignored test `host::guarded_write_tests::benchmark::guarded_vs_kernel`
compares the production copy helper with guarded writes versus
`NtWriteVirtualMemory`. It uses fresh fixed mappings, identical addresses and
64 KiB copies, 12 ABBA cycles, 24 samples per path, with resets, readback and
canary checks outside timing. The Windows debug-profile test executable ran
under the same Proton version as the title.

| Synthetic working set | Guarded median per copy | Kernel median per copy | Ratio |
|---|---:|---:|---:|
| Repeated 64 KiB | 553 ns | 14,464 ns | 26.1x |
| Rotating 16 MiB | 2,258 ns | 22,972 ns | 10.2x |

These are cached CPU-copy measurements on VirtualAlloc memory, not FPS gains,
section-view throughput, or proof of atomicity during remapping. The helper's
inherited per-fragment delivery-status limitation remains. Title observations
and their scene/cache caveats are recorded in [Linux validation](linux.md).

## Experimental controls

Guarded writes require **both** `VICTORY_COPY_PLAIN=1` and
`VICTORY_WRITE_PLAIN=1`; writes remain disabled by default. A failed attempt
falls back to the existing kernel path. Concurrent remapping can still change
which allocation receives bytes; fault recovery does not make a copy atomic.

`VICTORY_TEXTURE_NO_WAIT=1` is a separate existing option. It avoids waiting for
remapping by reading into scratch; failed reads preserve the previous staging
contents. Successful reads are not guaranteed to represent one mapping
instant. It stays opt-in.

For timing, unset `VICTORY_D3D12_DRED` and `VICTORY_GPU_TIMING`; setting them to
`0` still enables them because their switches test presence. The validation runs
retain normal shaders and default loop limits. No assertion or shader is skipped.

The audio changes repair concrete bugs, but both combined title trials still
hit the repeated fixed-pool assertion. They have not resolved progression here. An inherited multi-frame audio
skip can still cause a zero-filled output block. A later checkpoint alone can
change whether the assertion occurs, so preserve starting saves/settings when
comparing builds. No game data or captured benchmark inputs are distributed here.
