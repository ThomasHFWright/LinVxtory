# Linux validation

The runtime results below describe preserved builds before the current
[Marvisak upstream integration](upstream-integration.md). They do not validate
the merged renderer or its performance.

LinVictory follows current fxceless/victory-fork rendering work, pinned initially
to `0b0074e0b2765e574c622e9a3de566f20f04135b`. Older debug-rendering experiments
are archived; they are not the active source base.

## Performance and model-data integration — 2026-11-25

The selected staging, guarded-copy, half-data and audio changes are documented
with source pins and test results in [Linux improvements](linux-improvements.md).
Normal world rendering remains incomplete and controllable gameplay is unverified.
The earlier known-working source and binaries are preserved.

Five offline trials reached world visuals without an NVIDIA Xid,
then ended at the same title breakpoint. The following medians select telemetry
samples with at least 500 draws per frame:

| Trial | Duration | Samples | Frame time | Texture preparation | Copy-back |
|---|---:|---:|---:|---:|---:|
| Staging fix, expensive diagnostics off | 478.80 s | 55 | 2504 ms | 625 ms | 222 ms |
| Same binary, texture no-wait enabled | 458.84 s | 48 | 2374 ms | 184 ms | 309 ms |
| Guarded reads and writes also enabled | 421.64 s | 45 | 1826 ms | 275 ms | 189 ms |
| Combined half-data and audio integration (`7e26bab`) | 518.31 s | 58 | 1741 ms | 245 ms | 199 ms |
| Final shader-access corrections (`eb8d790`) | 432.27 s | 43 | 1491 ms | 225 ms | 173 ms |

These are descriptive measurements, not a controlled FPS comparison: scenes,
draw/copy volumes, cache warmup and memory pressure differ. The separate
synthetic copy benchmark measures CPU throughput only. Guarded writes were
actually used in the title; the guarded-copy trial reported no fallbacks.
Both guarded writes and texture no-wait remain opt-in.

The combined trial rendered outdoor scenery and characters with substantial
missing or corrupt geometry. It ended after 8 minutes 38 seconds at the same
breakpoint, with no recorded native allocation failure or OOM event. Its seven
starting profile files matched the saved pre-run manifest; the title advanced
its save version during the run. No controllable gameplay was demonstrated.
The combined trial includes both audio and half-data changes, so it cannot
isolate either one's effect. These results do not show that audio fixes remove
the progression blocker.

The final corrected build was rebuilt and retested under Proton before a second
combined trial. It reached textured clothing, lighting and outdoor scenery, with
missing faces and substantial geometry artifacts. It ended after 7 minutes
12 seconds at the same breakpoint, again without an NVIDIA Xid or recorded
native allocation failure. The same seven-file starting profile was copied into
a separate writable directory and remained unchanged; existing saves were
preserved. Caches were warmed, so this still is not a controlled performance
comparison. Guarded writes were active, with occasional fallbacks. No OOM or
memory-limit events were recorded; host memory pressure remained substantial.

The recurring breakpoint follows an empty fixed-size object free-list branch;
its pool identity and root cause are still unproved. The audio import is a
progression experiment, not an established remedy. No assertion is bypassed.

## Earlier visual breakthrough — 2026-11-25

Normal in-world visuals are verified: textured characters, floor geometry and
lighting. Interactive gameplay is not yet verified; some character detail is
missing and observed frame rates vary substantially (roughly 0.1–1.4 fps).

The exact code-table change from [PR1006](https://github.com/fxceless/victory-fork/pull/1006)
at `4e7af7b01356417864b935bd1fffc7d0dece266f` was the change between two trials
using the same app/runtime corrections. The preceding build hit Xid 13 at
world entry after 131 seconds. The new build ran for 583.74 seconds without
an NVIDIA Xid, then exited with a title breakpoint (`0x80000003`). A second
run of the same binary/runtime with validation disabled produced real world
scenes and no Xid for 534.58 seconds, then hit the same breakpoint. This is
progress past the earlier GPU failure, not a claim of complete stability.
The repeat recorded no memory-limit, swap-limit or OOM events; memory pressure
was substantial, but allocation failure is not established as the assertion's
cause. Removing validation did not resolve the slow rendering.

Captured production shader tables shrank from 18,368 to 632 bytes and from
16,640 to 616 bytes; the retained values exactly match their original trailing
data. The trial used normal shaders, default loop limits, the watchdog hook
and Vulkan core/synchronization validation. Existing local fixes include
per-subresource texture states, null-CBV metadata and runtime allocator/null-CBV
corrections. These were also present in the immediately preceding failed run.

Validation: native shader suite **179 passed / 2 ignored**; Windows shader suite
**225 passed / 1 known stub-pipeline failure / 3 ignored**; GPU suite after the
state/null-CBV work **193 passed / 4 known failures / 7 ignored**. Diagnostic
build and all-target/all-feature Clippy pass. Restoring the old table-start
rule makes the new regression fail. [Aggregate results posted upstream](https://github.com/fxceless/victory-fork/pull/1006#issuecomment-7168360845).

The imported optimization assumes dynamic code-relative reads target trailing
data. A synthetic embedded-data case exposes a limitation; a conservative gap
guard also prevented trimming a real shader because of an unreachable NOP.
That guard is not enabled. The current implementation retains the tested PR's
behaviour, with its helper/tests in a separate module to meet the file-size
policy. Stronger address-bound analysis is still needed to generalize it.

The sections below retain earlier checkpoints and their original limitations.

For the exact external runtime revisions, source patches and build settings, see
[the runtime recipe](linux-runtime.md).

## Compatibility changes

- [PR20](https://github.com/fxceless/victory-fork/pull/20): runtime HLSL falls
  back to DXC when Wine's compiler refuses it or drops its root signature.
- [PR454](https://github.com/Marvisak/VIctory/pull/454): opt-in, build/signature
  checked watchdog hook (`VICTORY_NO_WATCHDOG=1`); use an external time limit.
- The latest clamp variants, in-flight copy shader and indirect benchmark use
  that shared compiler path. A regression creates the copy shader's root
  signature and compute pipeline on D3D12 under Wine.
- Compiler API calls in tests are adapted; three existing Clippy findings were
  cleaned up without changing renderer behavior.

The imported commits preserve their original authors. No shader skips,
assertion bypasses or forced debug pixel shaders are added.

## Test environment and results

CachyOS, RTX 5090, NVIDIA 620.73.04, Rust 1.98.1, Windows MSVC cross-build,
GE-Proton 11-14 Wine/DXGI, Microsoft DXC/DXIL 1.9.2609.
The local D3D12 runtime uses vkd3d-proton `cae8353ed7959c4` with dxil-spirv
`e084e35c4506210b44f6b7df3bbf31a1026545b8`, which avoided three isolated compiler
recursions observed in earlier Linux trials. This does not establish correct
rendering or GPU stability.

Diagnostics build and prepare pass. Nine Windows library suites under Wine:
**554 passed, 7 failed, 12 ignored**. All four compiler-fallback regressions pass,
including the new in-flight copy shader. Integration fixtures were not run.

Failures: three pipeline-library expectations, the shader stub digest,
Windows event-log subscription, `WSAConnectByList`, and host device information.
The Windows driver watcher cannot subscribe under this Wine runtime; the local
launcher separately watches Linux kernel NVIDIA Xid messages and stops its run.
These failures remain visible; they are not counted as passes.

## Runtime trial

The first normal-rendering trial reached world submissions, then hit an NVIDIA
Xid 13/MMU fault at approximately 137 seconds. The observer stopped the run.
No scene geometry was verified; this is not working Linux gameplay.

A follow-up fixes the range guard shortcut described in
[issue 698](https://github.com/fxceless/victory-fork/issues/698#issuecomment-7159281878):
a range bypasses memory queries only when every block is known. The regression
fails with the old first-block check and passes with full coverage; all 13
write-watch, alias and mapping tests pass. The follow-up title run reached the
opening title card but hit another NVIDIA Xid 13/MMU fault at approximately
115 seconds. The observer stopped it; normal scene geometry remains unverified.
This fixes a reproduced range-check bug but does not resolve the GPU fault.
The retry reused its own warmed cache, so elapsed times are not a controlled
performance comparison.

Use separate cache/data/prefix folders,
read-only input and an offline sandbox. Set native `d3d12,d3d12core,dxgi` DLL
overrides and Windows paths for `VICTORY_DXC` / `VICTORY_DXIL`. The local baseline
turns off `VK_NV_raw_access_chains` and opts into the title-watchdog hook with an
external deadline. Other rendering switches retain upstream defaults, including
its per-stage shader loop bounds; no uniform loop-limit override is used.

Only source and aggregate results are published. Game files, generated shaders,
logs, screenshots, caches and runtime binaries remain local.

## Descriptor-kind follow-up

Heap-indexed scalar buffer loads now distinguish explicit CBVs, buffer SRVs,
and buffer UAVs. Null/texture entries return zero without creating an invalid
buffer handle; kind flags are stripped before vector stride arithmetic.
This addresses the scalar-load classification concern in issue 698.

The synthetic emitter test passes with the fix and fails when the previous
invalid-slot-as-CBV condition is restored. The host regression checks actual
mapped entries through buffer/CBV/texture/null rewrites. Affected Wine suites:
**387 passed, 5 known failures, 8 ignored** (GPU 172/4/5; shader 215/1/3).
Build and Clippy pass. The classifier-only trial still hit a context-switch
timeout after approximately 228 seconds, without verified world geometry.

A diagnostic baseline with vkd3d descriptor checks and NVIDIA checkpoints still
faulted. Breadcrumbs narrowed the likely crash region to a compute dispatch;
no descriptor-QA mismatch was reported. A separate `VICTORY_HEAP_SYNC_TABLES=1`
trial also faulted and showed no normal world geometry. Neither diagnostic
experiment establishes a gameplay fix.

The CBV size clamp from [PR24](https://github.com/fxceless/victory-fork/pull/24)
by NuraadGit is applied to both host creation paths. Two explicitly invoked
actual-path tests pass under Wine; the D3D12 info queue is unavailable there,
so this proves creation/device survival, not native debug-layer validation.
Gameplay impact is not yet established.

Copied-descriptor synchronization now invalidates cached fingerprints across API
writes and unmake, applies the captured record, and only skips a table texture
when the same descriptor is installed and was actually read. This follows the
stale-table concern in [issue 698](https://github.com/fxceless/victory-fork/issues/698#issuecomment-7159281878).
Three actual-path regressions cover both individual and page-batched copying:
all three fail on the previous implementation and pass with the fix. The full
descriptor suite passes **32 tests**, with the two CBV device tests invoked
separately. The combined trial still hit an MMU fault at world entry, approximately 136
seconds after launch. It did not establish normal gameplay.


## Bounded loop exits

For a loop with a real header exit and one follow block, reaching the iteration
limit now uses that exit instead of returning from the entire shader. This keeps
current loop-carried values and allows later group barriers to execute. Loops
without a proven exit retain the bounded fallback; this does not establish
convergence for barriers inside loops or prove the title reaches a loop cap.

Six synthetic emitter regressions cover both branch orientations, conditional
and mixed latches, nested counter resets, and fallback cases. The old code fails
the follow-barrier regression; the fix passes. The Windows cases also validate
the production serialized DXIL. The full shader suite under Wine reports
**221 passed, 1 known failure, 3 ignored**: the existing stub-digest expectation
still fails. Build, Clippy and asset policy pass. The subsequent game trial still hit an MMU fault at world entry, without
verified normal geometry. The correction has not resolved gameplay.

Runtime comparisons using the older vkd3d descriptor backend and disabling
VRAM buffer copies also hit MMU faults (approximately 141 and 131 seconds).
Neither produced verified normal world geometry. Three compute DXIL containers
from the earlier crash regions pass Microsoft's validator; this does not prove
correct SPIR-V translation or runtime behavior.


## Transfers crossing a page boundary

The [PR846](https://github.com/fxceless/victory-fork/pull/846) fix by gomeaw is
applied to wide global loads/stores and scalar loads. A transfer that crosses a
64 KiB title-memory page now resolves the second page independently, including
address carry and missing-page notification. Same-page accesses remain vectorized.

All **61 synthetic GPU cases pass on the RTX 5090 under Wine**, in three test
families. They cover separate biased views, every aligned split position,
4 GiB carry, missing-page values/faults, and store canaries. Restoring the previous
lowering while retaining the tests makes all three families fail at the first
crossing x2 case: loads read the wrong mapping, and stores change a canary.
The fixture uses gomeaw's validation cases and ryotdev's initialized/root UAV
support; hardware selection is adapted for Linux. No title data is used.
Clippy and the Windows build pass. The game trial still hit an NVIDIA Xid 13/MMU
fault after approximately 127 seconds. A separate comparison with the relay
turned off (`VICTORY_RELAY=0`, synchronous submission/copy-back) also faulted,
after approximately 136 seconds. The suspected crash region in both included
the same small texture-processing compute shader. No normal world geometry
was verified; the page-transfer correction has not resolved gameplay.

The Linux synthetic validation and its limitations are posted on
[the existing PR](https://github.com/fxceless/victory-fork/pull/846#issuecomment-7165765806).
