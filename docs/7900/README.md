# Series S ray-traced replay on RTX 5090

This is the renderer built by the root [build script](../../build.sh). It uses
[VIctory-7900XTX at 014d52d](https://github.com/avatarmanz/VIctory-7900XTX/commit/014d52dedf837d1ac660a0bd256c4704be9eeb40),
with LinVxtory's Linux and face/hair work, CPU submission changes, and bounded
readback capture. The patch is GPL-3.0-or-later like the renderer. The kit
provides source, settings, and input hashes. Use your own game files and
build the runtime locally.

## Build and launch

Set up Rust 1.98.1 and the Windows x64 MSVC cross toolchain, with Cargo's
locked dependencies available offline.
[Cross-toolchain setup](../pr995-candidate/README.md#reconstruct-and-build)

From the LinVxtory checkout:

```sh
sh build.sh "../linvxtory-rt-renderer"
```

Use a fresh external source directory when upgrading. The script reconstructs
and verifies the tested Git tree `0ab430bbecb29f450d2d9e837673ad10aa1210b4`
and checks all 259 build inputs. It builds the diagnostic profile used for
these measurements.

The test used CachyOS, RTX 5090 and NVIDIA 620.73.04, GE-Proton 11-14, and the
[vkd3d-proton source revisions](vkd3d-source-lock.json) with the
[custom-heap and null-CBV patch](vkd3d-proton-linux.patch.txt). Use those pins
and that patch at the source checkout and patch steps in the
[Linux runtime instructions](../linux-runtime.md#build). The runtime binaries
are built and installed locally. See also the
[indexed wineserver setup](../runtime/indexed-server/README.md).

Set `PROTON_WINE`, `TITLE_EXE`, `WINEPREFIX`, `VICTORY_DXC`,
`VICTORY_DXIL`, `VICTORY_CACHE_DIR`, and `VICTORY_DATA_DIR` to your own
paths. Use a separate shader cache and save directory for the new renderer.
The replay used a read-only game mount, an isolated offline runtime, and a
fixed stop time. The settings disable the renderer watchdog, so use a time
limit for unattended tests.

```sh
src=$(realpath "../linvxtory-rt-renderer")
set -a
. ./docs/7900/rt-speed.env.sh
set +a
export WINEDLLOVERRIDES='d3d12=n;d3d12core=n;dxgi=n'
export VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains
"$PROTON_WINE" "$src/target/x86_64-pc-windows-msvc/diag/victory.exe" \
  run "$TITLE_EXE" --execute --console series-s
```

This selects Series S at 1920×1080 with ray tracing active and the ambient
substitute disabled. The preset waits up to five seconds on a pipeline's
first use. A cold run can spend several minutes compiling. The FPS
measurements below use a warmed shader cache.

## Findings

Each comparison uses the same executable within the pair, warm cache seed,
runtime, save, and offline 1080p preset. FPS is the median of individual
title Present intervals from elapsed seconds 240–295. The second capture
pair ran capture-on first and capture-off second. The table lists the settings
as off/on or small/large for comparison.

| Comparison | Median FPS | Median copyback per frame |
| --- | ---: | ---: |
| Producer capture off → on, earlier renderer | 7.94 → 8.28 | 19.05 → 18.50 ms |
| Producer capture off → on, current renderer | 8.14 → 8.89 | 19.35 → 16.70 ms |
| 8 → 16 MiB per capture slot, separate experiment | 9.03 → 8.34 | 17.85 → 18.30 ms |

Both arms of the current comparison ran for about 356 seconds and created
all 53 ray-tracing builder pipelines. Capture-on recorded 477 Present
intervals, and capture-off recorded 362. Fetch wait fell from 10.30 to
8.80 ms per frame. The equivalent 4,250–4,499 draw band also improved
from 121.85 to 117.10 ms in four and five frame reports respectively.
The measured replay stays below the 20 FPS target and results vary by
scene and run.

In the cell the player's face and ponytail rendered, and the wire window
and room lighting were visible. The camera sweep started after the timed
window. This covers the fixed cell checkpoint. Later scenes and long-session
stability remain unverified on this renderer.

The selected capture path saves fully covered GPU writes during their
producing dispatch, allowing the relay to skip a separate fetch. Four
8 MiB slots retain each capture through the existing completion fence.
A missing or oversized capture falls back to ordinary readback. The
16 MiB experiment increased captured bytes and slowed the measured replay.
The preset keeps 8 MiB slots.

A separate synthetic test compared shader reads from a 50 MiB shared
allocation and VRAM on the same RTX 5090. Both paths returned the expected
bytes across the split allocation. Dense shared reads took about
0.93–0.98 ms per pass, and dense VRAM reads took about 0.009–0.024 ms.
A 50 MiB transfer to VRAM took about 0.98 ms. These results favor VRAM for
repeated dense shader reads. Sparse read and direct-write experiments
remain separate from the default memory path.

### Other performance trials

Lowering the minimum GPU buffer copy size from 1 MiB to 64 KiB reached
9.50 median FPS in an exploratory run. The repeat used a fixed 7,373 MiB
cap in both arms. The median improved from **7.62 to 9.19 FPS**, while
the same 55-second window contained **411 versus 367 Presents**. The
95th-percentile frame time rose from **188 to 361 ms**. Both runs
completed. The control was mirrored to a desktop window during part of
the timed interval using the replay's existing captures. The published
preset retains the 1 MiB minimum.

Ordered indirect batching reduced clamp passes from about **1,440 to 133
per frame**, while the replay median fell from **8.83 to 5.19 FPS**. In
a separate 3,200-draw RTX 5090 fixture, equal-sized batches took
2.06 ms with the legacy path and 2.41 ms with the ordered kernel.
Narrower enhanced-barrier scopes also gave similar or slower timings
in the fixture. The preset leaves ordered batching disabled.
[Ordered batching implementation](https://github.com/avatarmanz/VIctory-7900XTX/pull/438)

## Earlier ray-traced measurements

The highest observed median in the same +240 to +295 second cell window
was **9.36 FPS** (106.84 ms, 473 title Present intervals) in one warm
control on renderer `ba3bb41`. It used Series S 1080p, the RTX 5090,
53 ray-tracing builder pipelines, and the ambient substitute off. The
window, lit cell, facial profile and ponytail were visible. That control
had compare joining disabled. Enabling compare joining on the same
executable measured 8.95 FPS in the other arm. We keep it disabled.

Increasing the outstanding command-list limit from 256 to 512 removed
a median **77.35 ms/frame** in waits at that limit in an earlier mesh-on
comparison, with median FPS going **7.75 to 8.95**. A separate cap
comparison reduced limit waits from 36.7 ms/frame to zero while median
FPS stayed similar at 8.87 and 8.73. These comparisons used different
renderer configurations. The current preset also uses 512.

The latest three-frame diagnostic trace matched 86 submission fences.
Across 57 sampled compare lists, the median was 45 µs and the sum was
3.18 ms. A single sampled game command list took 24.15 ms.
Some timestamps missed the reporting deadline. This trace points the
next investigation toward work inside game command lists. The short
sample covers only the lists it timed.

## Source and credits

[avatarmanz and the 7900XTX contributors](https://github.com/avatarmanz/VIctory-7900XTX)
provided the newer renderer and ordered ray-tracing path, building on the
[fxceless renderer](https://github.com/fxceless/victory-fork) and
[Marvisak's VIctory](https://github.com/Marvisak/VIctory).
The Linux overlay carries forward the Wine DXC fallback, Wave32 work, and
the face/hair fixes developed with [OctoAlgo's inkVI stack](../inkvi/README.md).
The CPU work includes adaptations of
[batched descriptor retirement](https://github.com/fxceless/victory-fork/pull/1429)
and [in-place dirty-range merging](https://github.com/fxceless/victory-fork/pull/1431).
See the [project credits](../../README.md#attribution) for earlier work.
