# Series S 4K DLSS on Linux

This is the renderer built by the root [build script](../../build.sh). It uses
avatarmanz’s VIctory-7900XTX source at 014d52d from the
[public source mirror](https://github.com/TheInfamous404/VIctory-7900XTX/commit/014d52dedf837d1ac660a0bd256c4704be9eeb40),
with LinVxtory's Linux and face/hair work, CPU submission changes, bounded
readback capture, and direct 4K DLSS. The patches are
GPL-3.0-or-later like the renderer. The kit provides source, settings, and
input hashes. Use your own game files and build the runtime locally.

## Default Ray Reconstruction preset

The Series S profile enables DLSS Ray Reconstruction automatically on
qualifying frames in Ultra Performance mode with **1280×720** input and
**3840×2160** output.
The game’s native global illumination (GI) remains active. The renderer
supplies the scene, depth, motion, material and reflection guides and
preserves the original wavy glass. The title applies its later
postprocessing and composites the UI at presentation.

For eligible frames, the renderer commits RR before the native filter
cutoff and replaces three opaque reflection passes. It retains the
game’s alpha reflection history to reduce lighting flashes. When the
guides or glass data fail to qualify in time, the renderer keeps stock Super
Resolution and the native filters for that frame. Startup can use
Super Resolution until the required frame data qualifies.
[Cell lighting results](ray-reconstruction.md#cell-lighting-stability) ·
[Startup investigation](ray-reconstruction.md#continuous-startup-investigation)

To retain opaque reflection history as well, export
`VICTORY_RR_RETAIN_NATIVE_OPAQUE=1` before launching the title with
the default RR preset. The three native opaque filters feed the single
4K RR evaluation. Use the regular automatic RR build and keep the
preset’s native alpha and GI settings. [Matched native-history comparison](ray-reconstruction.md#remaining-reflection-pulses)

## Historical 4K Super Resolution control

The historical profile feeds **1280×720** into DLSS Super Resolution
in Ultra Performance mode and presents a **3840×2160** swap chain. The
title generates jitter using the 72-phase count reported by the upscaler.

The offline AOC replay on CachyOS and RTX 5090 completed its scheduled
301-second run. It logged **at least 1,024 DLSS evaluations**, all 53
ray-tracing builder pipelines and ambient substitute off. In the cell
from elapsed seconds 240–295, 585 title Present intervals yielded
**10.77 median FPS**, a 92.89 ms median frame interval and a **119.01 ms
95th-percentile** interval. Face and hair rendered in the intro, and the
ponytail, wire window and lighting remained visible in the cell.

This result belongs to the historical Super Resolution renderer. The
separate same-build SR/RR comparison used the historical hybrid-GI route.
[Historical hybrid-GI timing](ray-reconstruction.md#capture-free-timing-with-the-stock-fallback-guard)

## Offline shader lookup

The current build uses the validated byte range of each embedded DXBC
container to match offline shader keys to runtime keys. Existing shader
lists based on the old carrier-tail hashes may need regeneration. The
same source change applies to both Super Resolution and Ray Reconstruction.
On a synthetic 64 MiB carrier with 256 valid containers, four alternating
Windows CLI runs under Wine reduced median `shader-info --verbose` time
from 0.775 s to 0.051 s and preserved all 256 offsets. A separate fixture
confirmed that identical containers in different carriers share a key.
[Upstream fix](https://github.com/trustno1-coder/VIctory-NVIDIA/commit/b1369afb40017099471223ff61327c4550669a61)

Use `sh build.sh --sr --control <renderer-directory>` to reproduce the
original Super Resolution source lock. Use `--rr --control` to reproduce
the Ray Reconstruction source lock before the newer performance patches.

## Descriptor updates

Partial imported copies now mark overlapping view records for descriptor
sync. An address-only write can therefore refresh a view even when its tag
sits outside the copied bytes. All eight Windows-target descriptor tests
passed under Wine. An uncontended copy-hook benchmark added about 0.04 ms
per 3,800 mixed copies and did not measure downstream sync or game FPS.
[Implementation and test](partial-descriptor-copies.patch.txt)

## Build and launch

Set up Rust 1.98.1 and the Windows x64 MSVC cross toolchain, with Cargo's
locked dependencies available offline.
[Cross-toolchain setup](../pr995-candidate/README.md#reconstruct-and-build)

Get NVIDIA DLSS SDK **310.9.16** and point `VICTORY_DLSS_SDK` at the
directory containing `lib/Windows_x86_64/nvsdk_ngx_d.lib`. If the SDK
archive places the library under `lib/Windows_x86_64/x64/`, stage a
local copy one directory up.
[NVIDIA DLSS SDK release](https://github.com/NVIDIA/DLSS/releases/tag/v310.9.16)

From the LinVxtory checkout, build the renderer.

```sh
export VICTORY_DLSS_SDK=/path/to/DLSS-SDK
sh build.sh "../linvxtory-renderer"
```

Use a separate external renderer directory for each build mode.

| Build options | Renderer |
| --- | --- |
| No option or `--rr` | Current Ray Reconstruction renderer |
| `--sr` | Current 4K Super Resolution renderer |
| `--sr --control` | Exact historical 4K Super Resolution renderer |
| `--rr --control` | Historical Ray Reconstruction renderer before the newer performance patches |

The script checks the base tree and all 263 historical build inputs,
then verifies subsequent overlay trees before compiling the locked
DLSS profile.
[RR overlay locks](rr-build-trees.txt) · [Performance overlay locks](perf-build-trees.txt)

The test used CachyOS, RTX 5090 and NVIDIA 620.73.04, GE-Proton 11-14, and the
[vkd3d-proton source revisions](vkd3d-source-lock.json) with the
[custom-heap and null-CBV patch](vkd3d-proton-linux.patch.txt). Use those pins
and that patch at the source checkout and patch steps in the
[Linux runtime instructions](../linux-runtime.md#build). The runtime binaries
are built and installed locally. See also the
[indexed wineserver setup](../runtime/indexed-server/README.md).

Set `PROTON_WINE` to your Proton Wine executable and `TITLE_EXE` to your
title executable. Export `WINEPREFIX` as a Linux path. Export
`VICTORY_DXC`, `VICTORY_DXIL`, `VICTORY_CACHE_DIR` and
`VICTORY_DATA_DIR` as paths visible to the Windows process. Use a separate
shader cache and save directory for the new renderer.
The replay used a read-only game mount, an isolated offline runtime, and a
fixed stop time. The settings disable the renderer watchdog, so use a time
limit for unattended tests.

Prepare the dedicated Wine prefix with the Microsoft x64 Visual C++ v14
runtime. Set `NGX_RUNTIME` to a local directory containing the signed
`nvngx_dlss.dll` and `nvngx_dlssd.dll` from NVIDIA DLSS SDK 310.9.16,
`_nvngx.dll` and `nvngx.dll` from the installed NVIDIA driver's Wine
components, and the x64 `nvapi64.dll` from Proton's DXVK-NVAPI.
`nvngx_dlssd.dll` enables Ray Reconstruction. Keep `nvngx_dlss.dll` for
Super Resolution, including fallback when RR or its DLL is unavailable.
The build kit contains none of these files.
[Microsoft Visual C++ runtime](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist) · [DXVK-NVAPI](https://github.com/jp7677/dxvk-nvapi)

```sh
src=$(realpath "../linvxtory-renderer")
bin="$src/target/x86_64-pc-windows-msvc/diag"
system32="$WINEPREFIX/drive_c/windows/system32"
mkdir -p "$system32"
for dll in _nvngx.dll nvngx.dll nvapi64.dll; do
    install -m 0644 "$NGX_RUNTIME/$dll" "$system32/$dll"
done
install -m 0644 "$NGX_RUNTIME/nvngx_dlss.dll" "$bin/nvngx_dlss.dll"
if [ -f "$NGX_RUNTIME/nvngx_dlssd.dll" ]; then
    install -m 0644 "$NGX_RUNTIME/nvngx_dlssd.dll" "$bin/nvngx_dlssd.dll"
fi
export WINEDLLOVERRIDES='d3d12=n;d3d12core=n;dxgi=n;nvngx_dlss,nvngx_dlssd,_nvngx,nvngx,nvapi64,concrt140,msvcp140,vcruntime140,vcruntime140_1=n'
"$PROTON_WINE" wineboot -u
"$PROTON_WINE" reg add 'HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore' \
  /v FullPath /t REG_SZ /d 'C:\Windows\System32' /f
set -a
. ./docs/7900/rt-speed.env.sh
set +a
export VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains
"$PROTON_WINE" "$bin/victory.exe" \
  run "$TITLE_EXE" --execute --console series-s
```

This launches the default 720p-to-4K Ray Reconstruction preset with
ray tracing and native GI active. Ordinary play needs no benchmark or
frame capture trigger.
For an explicit Super Resolution launch, start in a fresh shell,
`export VICTORY_RR_AUTO=0` before sourcing `rt-speed.env.sh`, and
point `src` at the desired renderer directory in the command above. The
same setting selects SR when comparing both modes with one RR executable.

The preset waits up to five seconds on a pipeline's first use. A cold
run can spend several minutes compiling.
[Diagnostic capture and historical hybrid-GI timing](dive-testing.md#keep-stock-super-resolution-when-the-rr-cutoff-is-missed)

## Findings

Each earlier 1080p comparison uses the same executable within the pair, warm cache seed,
runtime, save, and offline 1080p preset. FPS is the median of individual
title Present intervals from elapsed seconds 240–295. The second capture
pair ran capture-on first and capture-off second. The table lists the settings
as off/on or small/large for comparison.

| Comparison | Median FPS | Median copyback per frame |
| --- | ---: | ---: |
| Producer capture off → on, earlier renderer | 7.94 → 8.28 | 19.05 → 18.50 ms |
| Producer capture off → on, published 1080p renderer | 8.14 → 8.89 | 19.35 → 16.70 ms |
| 8 → 16 MiB per capture slot, separate experiment | 9.03 → 8.34 | 17.85 → 18.30 ms |

Both arms of the published 1080p comparison ran for about 356 seconds and created
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

### Earlier 1080p DLSS trial on Proton

A private trial adapted the DLSS replacement to the Series S upscaler call.
Both replays used the same executable, cache seed, driver, 720p render at
1080p output, 53 ray-tracing builder pipelines, and ambient substitute off.
DLSS recorded at least 512 replacements with no stock fallback. The face
rendered in the intro and the cell window and ponytail were visible in the
sampled frames.

The +240 to +295 second window measured **7.28 FPS with FidelityFX and
7.39 FPS with DLSS**, over 379 and 388 Present intervals respectively.
The +260 to +295 second window measured **7.37 and 7.28 FPS**. In the
later +300 to +350 second window, 95th-percentile frame time increased
from **335 to 744 ms**. This single pair shows that Super Resolution can
run on this Linux setup. A repeatable 1080p speedup remains unverified.
[Donor DLSS experiment](https://github.com/avatarmanz/VIctory-7900XTX/pull/203)

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
The DLSS bridge builds on the
[7900XTX DLSS experiment](https://github.com/avatarmanz/VIctory-7900XTX/pull/203).
See the [project credits](../../README.md#credits-and-development-history) for earlier work.
