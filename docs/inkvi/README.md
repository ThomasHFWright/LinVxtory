# inkVI renderer on LinVxtory

These are the earlier inkVI tests. For the current build see the
[7900XTX renderer](../7900/README.md). The historical build instructions
below apply to [main at 7de8ad3](https://github.com/ThomasHFWright/LinVxtory/tree/7de8ad35ba99d0bd4e84fda0b7bd72ac5f8a08fe).

The source stack is [OctoAlgo’s inkVI at 7031c82](https://github.com/OctoAlgo/inkVI/tree/7031c8259f4c8a906d06c1e3f4726d455126bfca).
It applies 39 ordered patches to [fxceless at ceb9e1f7a](https://github.com/fxceless/victory-fork/commit/ceb9e1f7a0180e61961bfde85d69d24e2dd84b39)
and includes each original contributor’s provenance and license.
[OctoAlgo’s report and patch breakdown](https://github.com/ThomasHFWright/LinVxtory/pull/1#issuecomment-7198399027)

LinVxtory adds the Series S compositor and upscale fallback, fresh CBV
tables after a command-list wait, eight-part Series S batching, and
7900XTX’s placed-view reuse and shader preparation improvements.
[Series S and 7900XTX port credits](../pr995-candidate/README.md#series-s-and-7900xtx-changes)

## Build

Configure Rust 1.98.1 and the Windows x64 MSVC cross toolchain
(clang-cl, llvm-lib, lld-link and the MSVC and Windows SDK libraries).
[Cross-toolchain setup and provenance](../pr995-candidate/README.md#reconstruct-and-build)

From the LinVxtory root, run `sh build.sh "../linvxtory-renderer"`. The
wrapper checks all 39 patch hashes and the pinned upstream tree, then
verifies 219 final build inputs before invoking Cargo. The release
executable is in `target/x86_64-pc-windows-msvc/release/` under that
external checkout. The build fetches a pinned helper shader from the
public base fork. The checkout supplies the same BCPack table exception
documented upstream. [Local asset policy](../asset-policy.md)

## Launch

Use the paths from your own runtime and game installation. Set
`VICTORY_DXC`, `VICTORY_DXIL`, `WINEPREFIX`, `VICTORY_CACHE_DIR` and
`VICTORY_DATA_DIR` for this renderer. Keep the title files read-only and
use a separate cache and save directory for trials.
[Linux runtime setup](../linux-runtime.md)

```sh
src=$(realpath "../linvxtory-renderer")
set -a
. "$src/.git/inkvi-kit/launch/speed.env"
set +a
export WINEDLLOVERRIDES='d3d12=n;d3d12core=n;dxgi=n'
export VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains
export VICTORY_NO_WATCHDOG=1 VICTORY_LOG=info
"$PROTON_WINE" "$src/target/x86_64-pc-windows-msvc/release/victory.exe" \
  run "$TITLE_EXE" --execute --console series-s
```

The preset waits up to five seconds for each pipeline's first use. On a
cold cache that can spend several minutes on the first world frame. Press
the start button once if you want to see the prologue close-ups. Use an
external time limit for watchdog-disabled tests.
[Upstream settings and known issues](https://github.com/OctoAlgo/inkVI/blob/7031c8259f4c8a906d06c1e3f4726d455126bfca/docs/known-issues.md)

The release uses its built-in shader loop limits. In diagnostics builds,
a global limit of 32 can cut the strand simulation short. Upstream tested
`VICTORY_SHADER_LOOP_LIMIT_CS=128` to keep the strand simulation intact
with the lower global limit. [Compute loop fix](https://github.com/fxceless/victory-fork/pull/683)

## RTX 5090 observations

The exact Series X reference build showed intact facial geometry and
visible hair in the early prologue close-up. In the cell the ponytail
and strands rendered beside the window mesh. Some cutscene background
surfaces were missing. The first cold load took several minutes; the
run was closed after 572 seconds once the cell had been observed.

The combined Series S build displayed facial skin in its first close-up and
retained the ponytail and hair strands in the same cell. The wall and window
mesh remained visible. One NPC's skin temporarily rendered black during the
cutscene. OctoAlgo reported the streaming issue and identified the later
texture recheck change. [OctoAlgo's report](https://github.com/ThomasHFWright/LinVxtory/pull/1#issuecomment-7198399027) · [Later fork change](https://github.com/fxceless/victory-fork/commit/e4245f3c1b0490250d9cf0946a2113000be98687)

| Build | Mean FPS | Median FPS | Sample range | Median indirect drops |
| --- | ---: | ---: | ---: | ---: |
| inkVI reference, Series X | 7.42 | 7.4 | 7.1–7.7 | 0 |
| LinVxtory with inkVI, Series S | 9.03 | 9.8 | 1.6–10.0 | 0 |

Each row uses ten host-log samples from seconds 510–565 of one cold
replay on the same RTX 5090 and GE-Proton 11-14 setup. The profiles and
overlays differ. The combined run was closed after 640 seconds. Both
runs built all four previously failing compute pipelines. No NVIDIA Xid
or kernel out-of-memory kill was observed. Controller interaction and
later chapters still need verification on this build.

| Locked source | Identity |
| --- | --- |
| inkVI reference tree | `3535e80cb6d1f7b4adff282678fe41d77f99a22c` |
| Combined renderer tree used for the FPS table | `463835438f93e46dc7b905720b8434bf41355464` |
| Ray-tracing preview tree | `c9e8e9e1eed6865010ff237a530cc1dcb33b11d1` |
| Memory synchronization test tree | `917cc4867c53f9634fffaab3e15b25737812aa9d` |
| LinVxtory overlay SHA-256 | `f54471336f5f72d8bfeb5e4f9868b79cd866e86a6d8efba334d8c7338dd1ed6d` |


## Ray-tracing preview

This branch adds DexNom's guest acceleration-structure path and the
7900XTX Wave32 adaptations. The guarded prefix-scan proof comes from
7900XTX #142, based on fxceless #497.
[DexNom renderer change](https://github.com/DexNom/VIctory/commit/bc3cd4b045ff63c52972d29c1bef7e897b94abc7) · [7900XTX Wave32 tails](https://github.com/avatarmanz/VIctory-7900XTX/pull/66) · [7900XTX guarded scan](https://github.com/avatarmanz/VIctory-7900XTX/pull/142) · [fxceless uniformity proof](https://github.com/fxceless/victory-fork/pull/497)

The same release build can enable the preview. After sourcing
`speed.env`, set these for a separate offline replay.

```sh
export VICTORY_BVH_BUILDER=1 VICTORY_BVH_HOLD=1 VICTORY_AMBIENT=0
export VICTORY_GPU_UPLOAD_COPIES=0 VICTORY_CARD_WINDOWS_MB=6144
export VICTORY_MERGE_RUNS=4096 VICTORY_MERGE_GAP=0
```

The RTX 5090 and GE-Proton 11-14 replay created all 53 builder pipelines
and reached the Series S cell with the movement and camera prompts.
The preview ran with the ambient substitute disabled. A second replay
held compute traversal while keeping the
builder and other settings active. The cell's left wall and shelving
received visible indirect light with traversal enabled.

The cell's median was **5.1 FPS** with traversal and **7.3 FPS** with
traversal held. Each median uses nine host samples from seconds 185–235
of a warmed replay. The traversal run ended manually after 5 minutes
43 seconds. Window speckling and slow streaming remain, and later
areas are unverified. Keep a separate shader cache and save directory
for the preview.


## Memory synchronization test

On RTX 5090 and GE-Proton 11-14, lifting the exact merge limit from
512 to 4,096 reduced waits for CPU and GPU writes to overlapping ranges
from 13 to 6 ms per frame. Median FPS in the fixed cell window went from
4.4 to 4.7 in one pair of five-minute runs. The zero-byte merge gap
preserves exact writes.

The compositor resolves both input texture states at submission and
uses descriptor slots owned by each present list. Three GPU readback
tests passed, including two queued frames with different overlays.
Asynchronous overlays remain experimental and off by default. They
removed the synchronous present waits in a separate comparison, and
median cell FPS fell from 6.35 to 5.55 as later waits grew. A
follow-up with GPU-side constant-view and title buffer copies enabled
reached 4.8 FPS. The recommended settings above use synchronous overlay
presentation and the standard buffer-copy path.
