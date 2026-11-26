# Upstream integration — 2026-11-25

The published Linux build at `cbe8b3b` includes Marvisak/VIctory main
`13f91caee25f2fdf2c062be5f7a27f058e51cbca`, 402 commits beyond the shared ancestor `ffe310e`. The previous Linux
source is retained locally at `backup/pre-marvisak-20261125` (`fd467c9`).
The previous tested executable and runtime remain preserved.

This is a merge of upstream main, not the unmerged PR queue. Original history
and authorship are retained. Upstream Windows prologue reports do not establish
Linux gameplay on this integration.

## Application and container integration

Both selective `translate --warm` and upstream `translate --cache` remain.
They trim embedded raw sources to the declared container length before caching;
truncated and undersized containers are rejected. Pipeline prebuilding requires
a Windows host or Wine/Proton. Diagnostic output directories retain repository
asset-policy checks.

The new shader-pack reader uses the existing zlib-rs implementation. It requires
complete streams and uses consumed-byte counts to locate subsequent streams.
Tests cover synthetic pack archives and malformed/truncated record streams.

Windows crash handling is compiled only on Windows. Tests that prepare their
own executable as a PE fixture are Windows-only; they execute under Proton.
Portable container and policy tests still execute natively. Cross-built loader
tests can take the separately built synthetic fixture through
`VICTORY_TEST_HLE_PROBE`; ordinary Windows tests still build it with Cargo.

## Renderer integration

Upstream dense change trackers, saved pages, aliases and kernel-write guards
replace the previous tracking implementation. The Linux remap lock remains;
absent pages are rechecked after ordinary commits. Texture staging allocation
failures remain eligible for retry without another write from the title.

Private write-watched pools also query the kernel before trusting a clean
tracker. Native decommit/recommit can remove a software guard without a runtime
notification. Queries publish changes to all overlapping trackers and rearm
stale guards; mapped views retain the dense-bit fast path. The cost of these
pool queries is not yet measured. Bytes destroyed by an unnotified decommit
cannot be recovered for a previously armed snapshot.

The inherited `ReadFile` guard still ends when a pending overlapped read
returns, rather than at its eventual completion. The synchronous paths are
covered; pending reads into tracked buffers remain a limitation.

Shader-cache directories use an immutable translator-source identity. Each
entry still includes translation options and host capabilities, so opening
the cache before device discovery does not misfile later translations.

Upstream replaces the optional relay/card/in-place execution and demand-residency
paths. `VICTORY_TEXTURE_NO_WAIT` is no longer consumed. Historical timing results
for that option are not measurements of the new implementation.

## Validation so far

- Native game: 16 passed.
- Native formats: 42 passed, 5 ignored.
- Native core / loader / PE / pipeline: 13 / 2 / 32 / 7 passed.
- Windows loader / PE under pinned Proton: 36 / 36 passed.
- Windows loader integration: 4 passed (crash logging, synthetic loading/binding,
  and title-DLL error reporting).
- Native asset-policy suite: 6 passed.
- Windows core / formats / game / pipeline: 13 / 42 / 16 / 7 passed;
  formats has 5 ignored tests.

The second full Proton run completed with shader 356 passed / 3 failed / 3
ignored, GPU 295 / 8 / 5, and HLE 151 / 2 / 0. Subsequent focused testing fixed
both shader stride-fixture failures (all 8 stride tests now pass). The remaining
shader stub-pipeline, GPU event/pipeline-library/profiler, and HLE host-network
and device-query failures remain documented compatibility limitations or open
investigations. This is not a fully green integration.

The optimized application builds. Initial title trials crashed during loading
in Vulkan graphics-pipeline creation. An isolated pixel shader produced invalid
SPIR-V: integer atomic OR had a float result type. The merge had lost the full
resource annotation in a handle-cache key; the correction now covers the new
entry-block and control-flow-join caches too. The synthetic regression exercises
all three cache routes. The original failing shader pair now passes DXIL and
SPIR-V validation and creates a host pipeline on the test GPU.

Device initialization also again publishes the host wave width before shader
translation and cache lookup. A focused device-initialization regression passes.
Together, the annotation, stride and initialization checks are 10 passed, no
failures. The title retry with both fixes creates the previously crashing
pipeline and reaches a world scene with its HUD visible after roughly four
minutes. Rendering remains dark, incomplete and slow; controllable gameplay
is unverified. No NVIDIA Xid was observed at that checkpoint. Final full-suite
reruns, Clippy and merge completion remain pending.

## Selected rendering-fork ports

Four changes from fxceless/victory-fork through `c36971b` are being adapted to
this merge. Each experimental path remains opt-in:

| Control | Behavior | Scope |
|---|---|---|
| `VICTORY_COPYBACK_SIMD=1` | SSE2 three-way buffer merge | GPU changes win; otherwise preserve current CPU bytes, including unaligned tails |
| `VICTORY_INDIRECT_GATHER=1` | Clamp an ordered prefix of indirect draws in one prelude pass | Stop batching at a fallback or possible argument write; retain the shared work budget |
| `VICTORY_SHADER_OPT=127` | Batch raw scalar/global loads up to four dwords | Default remains 63; retain descriptor/page boundaries and scalar offset-wrap fallback |
| `VICTORY_TEXTURE_BACK=1` | Return selected shader-written textures to title memory | Off by default; preserve allocation/remap tickets, all mip/array footprints and GPU completion ordering |

The SIMD adaptation and wide loads follow fxceless `c36971b`; indirect gathering
follows `465b94f`. Texture readback follows cxy0072019-sketch's
[PR999](https://github.com/fxceless/victory-fork/pull/999), merged as `f5c03f3`,
including the allocation-lifetime corrections in `04df435`. The older fork's
asynchronous submission machinery is not needed here: execution waits for the
GPU, completes every readback callback, then writes title completion markers.

The four native CPU merge regressions pass. A synthetic optimized CPU benchmark
of the same merge implementations measures SSE2 at 1.94–1.97 microseconds for
64 KiB and 128–166 microseconds for 4 MiB, versus scalar 16.4–21.8 and
1,246–1,333 microseconds respectively (100 iterations, dense and sparse GPU
writes, identical-output assertions). These cached CPU measurements are not
frame-rate improvements. The optimized application and strict Windows all-target/all-feature Clippy for
GPU and shader crates pass. The native supported workspace has 502 passes and
8 ignored tests; three HLE cases require Windows path/API behavior and are
explicitly excluded after reproducing their native failures. The full Proton
GPU suite has 319 passes, 8 existing failures and 5 ignored tests; shaders have
364 passes, the existing stub-pipeline failure and 3 ignored tests. All new
regressions pass, including 10 texture-readback cases and 7 indirect-gather cases.
No new test is ignored to obtain these results.

The texture port withholds a readback when buffer writes overlap its destination,
including known physical aliases. The check and copy share the mapping gate;
remapping invalidates affected tickets before old alias links disappear. Synthetic
shared-section and real GPU tests cover this ordering. Overlapping buffer/texture
writes remain conservative until their original write order can be preserved.

The combined offline Linux trial ran for 601.77 seconds, ending with harness
code 124 at its ten-minute limit. It did not exit itself; no unhandled exception,
device loss, NVIDIA Xid or OOM kill was observed. The opening cinematic rendered
world geometry, a vehicle and characters, with substantial missing textures and
models. At least 32 gathered batches and 128 texture readbacks executed. The
last 32 logged frames had a median of 3,971 ms, ranging from 1,392 to 10,800 ms.
Controllable gameplay is unverified. Cold shader compilation, different scenes
and substantial swapping prevent a fair frame-rate comparison. Host preparation,
especially textures and heaps, remains the largest measured cost.

This bounded run did not reproduce the earlier title assertion at image offset
`0x5149aec`; it does not establish that the assertion is fixed. The previous
merged run hit it after 581.39 seconds. All four paths remain experimental and
opt-in. The source and matching executable are available in the [cbe8b3b prerelease](https://github.com/ThomasHFWright/LinVictory/releases/tag/linux-edge-cbe8b3b).

With the same executable and `VICTORY_PIPELINE_WAIT_MS=0`, an offline RTX 5090
trial ended after 590.71 seconds on a title breakpoint at image offset
`0x63966f` (exit 3). The opening cinematic rendered. Input probes did not
establish control. In the three-minute window starting three minutes after
the same first world compute bind, the earlier run presented 63 frames
(0.36 FPS) and this run presented 67 (0.38 FPS). Cache warmth, the visible
shot, and background observation differed. Keep the pipeline wait setting
experimental.

## Paired-wave and upload trial

The compute translator incorporates changes from PiloteCode's
[paired waves](https://github.com/fxceless/victory-fork/pull/854) and
[second shader round](https://github.com/fxceless/victory-fork/pull/993).
It can represent a 64-lane compute wave on the 32-lane host. On GE-Proton
11-14 and an RTX 5090, all four formerly refused compute shaders and four
tiled cascade-shadow shaders validated and created pipelines. The title also
created all four formerly refused pipelines during the run. Correct shadow
appearance in gameplay remains unverified.

An opt-in port of independent upload fills from tunczzz's
[frame-rate work](https://github.com/fxceless/victory-fork/pull/995) runs with
`VICTORY_WIDE_FILLS=1`. Partial private-memory reads now check only the
write-watch pages they cover. The native shader suite passed 253 tests with
two ignored. The focused Proton run passed 52 tests covering paired waves,
fills, indirect execution, watches, texture readback and completion ordering.

With zero pipeline wait, the trial stopped itself after 619.71 seconds at
title image offset `0x5149aec`. The title reported frame 2250, prestreaming,
1,561 streaming requests and 45,000 of 45,000 instance slots occupied.
No OOM kill occurred. The scripted gameplay probe was scheduled later.
The three-minute window beginning three minutes after the first world compute
bind presented 74 frames (0.42 FPS), versus 67 (0.38 FPS) on the preceding
zero-wait build. These runs differ in shader preparation and visible shots.

### Indexed wineserver trial on the RTX 5090

Both runs used the paired-wave executable, zero pipeline wait, wide fills and
the same shader-cache directory. From 180 to 330 seconds after the first bind of
compute `02384a60fda6f2d0`, stock GE-Proton 11-14 advanced 59 frames in 146.3
seconds (0.40 FPS). A private GE11-14 wineserver with indexed view lookup
advanced 148 in 149.2 seconds (0.99 FPS). Rates use the first and last logged
present in the window. The camera sequence advances with the title, so the
visible workload can differ. Both runs reused translated shaders. vkd3d
reported that it could not open its disk archive in the configured directory,
which the launcher had omitted to create. Both also logged that the application
could not create its D3D12 pipeline library.

The indexed run closed itself after 393.28 seconds at `0x5149aec`. Its own
report listed frame 1488, 1,586 streaming requests, 22,478 of 22,500 building
slots and all 45,000 instance slots occupied. Both servers reached this
breakpoint before the scheduled control test. The indexed server is a separate
runtime experiment. The application release contains the same executable used
for both runs.

The [source-only GE11-14 build kit](https://github.com/ThomasHFWright/LinVictory/releases/download/linux-edge-10532b2/ge1114-indexed-source-kit.tar.gz)
contains source pins, build instructions and the parent/child mapping test.
The three server variants produced identical results for all 94,376 records.
The kit downloads the upstream patch at build time. The executable release and
installed GE runtime can be used independently of the kit.

A second indexed run traced five instance-culling compute shaders. All five
recorded nonzero dispatches and had callbacks after GPU completion. All the
early drops in the retained trace preceded the respective pipeline's creation.
The title still stopped at frame 1510, with 45,000 of 45,000 instance slots
and 22,495 of 22,500 building slots occupied. The trace leaves the shaders'
output values and the title's consumption of those outputs unresolved.

### Empty pipeline library on Proton

The Windows Rust binding supplies a non-null pointer for an empty slice.
vkd3d-proton rejects that pointer at zero length. Creating an empty library
now passes a null pointer and a zero length. Existing nonempty libraries
continue to use the slice binding. On the RTX 5090, all 10 library tests,
8 pipeline-host tests and 4 bindless tests passed. This includes four
previously failing tests for creation, concurrent loading, mesh pipelines and
depth bounds. Frame-rate impact has not been established.

### First locally generated story save

The unmodified 10532b2 executable ran for 1,801.67 seconds in Series S mode
on indexed GE11-14. The harness stopped it at the time limit. The title wrote
a new story slot about eight minutes in. The developer build used stock pool
sizes and a private copy of the original settings. The picture was largely
black or white with the title's own upscaler. A separate GPU integration test
using the official PC upscaler passed for an FP16 output. Player control and
the result of loading the new save on Series X require separate verification.
