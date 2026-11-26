# inkVI renderer on LinVxtory

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
verifies 218 final build inputs before invoking Cargo. The release
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
| Combined renderer tree | `463835438f93e46dc7b905720b8434bf41355464` |
| LinVxtory overlay SHA-256 | `5a8213841840ac4e3e9301b219bd24f8976eca3dac9322ac5b513254c3364ac8` |
