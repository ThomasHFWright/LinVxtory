# Linux validation

LinVictory follows current fxceless/victory-fork rendering work, pinned initially
to `0b0074e0b2765e574c622e9a3de566f20f04135b`. Older debug-rendering experiments
are archived; they are not the active source base.

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
Clippy and the Windows build pass. Gameplay impact remains to be tested.
