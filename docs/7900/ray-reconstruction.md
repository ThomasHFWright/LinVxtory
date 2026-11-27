# Ray Reconstruction on the Series S 4K renderer

The control is LinVxtory `338d80e`, renderer `5b10585` (tree
`3d033cbd633757d2fd15f97a4323d566e8a043ba`). Its 1280×720 → 3840×2160
Super Resolution replay measured 10.77 median FPS and 119.01 ms p95 over
585 title Present intervals in the cell at +240 to +295 s. See the
[control build and results](README.md).

The intended RR path uses one NGX Ray Reconstruction evaluation to produce the
4K scene from noisy 720p lighting. The title can then composite the UI.
SR remains the control and the fallback. The branch is experimental.

## Graph investigation

In the locked renderer, `fidelityfx/observe.rs` intercepts the title's
FidelityFX upscale call. At that point `fidelityfx/ngx.rs` has scene color,
depth, primary motion, jitter, frame time and scalar pre-exposure. It binds
the first three textures and a fourth, distinct RGBA16F output to NGX
SuperSampling. The optional exposure, reactive and transparency textures
are absent in the supported descriptor shape.

The GPU bridge in `crates/victory-gpu/src/external.rs` records on the title's
open translated command list. It prepares aliases, uploads and depth twins,
transitions the inputs for compute reads and the output for UAV writes, and
invalidates the cached compute bindings after external work. A title texture
address alone cannot identify the contents of a reused transient allocation.

| RR input | Evidence needed at the 720p lighting pass |
| --- | --- |
| Noisy HDR scene color | Trace the final lighting producer and its consumers across the stock spatial and temporal filters. Preserve any lighting composition that still runs. |
| Diffuse and specular albedo | Verify the active material producers, linear color and view-dependent specular reflectivity. |
| Normal and roughness | Verify the shading-normal decoder, its coordinate system and the linear roughness channel for the active material variants. |
| Reflection guide | Identify dense reflection motion, or verify world-space specular hit distance, world-space normals, depth and matching camera transforms. |
| Depth, primary motion and jitter | Match the noisy input's frame, raster coverage, units, projection and history reset. |
| 4K output | Identify the first display-resolution scene consumer and preserve subsequent post-processing and UI. |

Screen-probe temporal resolve writes indirect lighting. Screen-space radiance
caching can also feed a later frame. Dropping these dispatches by name can
leave consumers with stale values. A bypass needs the specific noisy input,
all downstream readers, and a valid alternate output on every affected frame.
Decide whether RR is available before skipping a stock pass needed by SR.
The pinned [NVIDIA RR guide](https://github.com/NVIDIA/DLSS/blob/v310.9.16/doc/DLSS-RR%20Integration%20Guide.pdf)
and [D3D12 helper](https://github.com/NVIDIA/DLSS/blob/v310.9.16/include/nvsdk_ngx_helpers_dlssd_d3d.h)
define the SDK inputs.

## D3D12 hardware result

SDK 310.9.16 created feature 13 in Ultra Performance mode on the RTX 5090
under GE-Proton 11-14. It selected exactly 1280×720 for 3840×2160 output.
Two synthetic evaluations completed, one with reset and one with history.
After the GPU fences signaled, both 4K readbacks had finite RGB at every
pixel and zero remaining sentinel components. The [probe source, build
instructions and measured outputs](rr-probe/README.md) are reproducible
without game files. In-game guide validation is still underway.

## What the title filters

Joining pipeline identifiers to the published SR replay confirmed that the
title selected opaque stochastic reflection tracing, spatial filtering,
temporal resolve, bilateral filtering and history update. The title also
selected a checkerboard shadow-mask denoiser and screen-probe filters.

An older 2560×1440 frame with detailed bindings shows why the handoff
requires changes earlier than the upscaler:

- The reflection spatial filter reads half-resolution radiance and produces
  two full-resolution textures. Temporal resolve consumes both. Later passes
  pack radiance, apply a bilateral filter and update history. Replacing that
  sequence must preserve the packed input expected by material lighting.
- The shadow chain uses packed masks and a buffer. Hair and checkerboard
  tracing, denoising, pruning and lighting share products from that chain.
  The raw checkerboard mask needs coverage validation before lighting can
  consume it directly.
- The scene-color path includes fog and depth of field. Bloom and color
  grading consume the resolved color, and a separate display-resolution
  surface is blended during presentation. The 4K replay selected the same
  depth-of-field and bloom families. Exact order at 720p needs a submitted
  frame trace.

The opaque reflection tracer stores ray distance in the X component of an
RG16F target. A downstream screen-space resolve uses it to reconstruct the
hit position along a normalized reflected ray. The title uses a negative
value for an invalid sample. RR requires a primary-pixel-aligned scalar
distance and distinguishes sky from other misses. Frame phase, coverage and
world units matter when converting the title's sparse samples.

For the standard opaque material, the producer and lighting consumer agree
on packed octahedral normals, inverse roughness in the normal target's R
channel, and an encoded Fresnel reflectance. Given an encoded channel
`p`, the lighting consumer decodes reflectance as `p² × (2 − p²)`.
RR takes view-dependent specular albedo, which also depends on roughness
and the view direction. The hair resolve can write sRGB-encoded diffuse
color and selects a specialized material class. Eye, hair and sheen
lighting use additional material terms. Binding raw G-buffer textures
would feed mismatched values to RR in these cases.

These findings combine the 4K SR runtime's selected shader identities with
static producer/consumer analysis and the older frame's resource bindings.
Addresses, dimensions and pass order from that older capture are unsuitable
as constants for the 720p renderer.

## Renderer backend and device test

The optional [renderer source overlay](rr-backend.patch.txt) adds a D3D12 RR
backend and an opt-in hardware test inside `victory-hle`. Game dispatch
continues to use the published SR path. The backend accepts a verified guide
bundle, checks its device, dimensions and formats, and records one RR
evaluation. It accepts dense reflection motion or scalar hit distance with
camera matrices. The caller owns resource transitions, backing lifetimes,
queue ordering and the history reset when switching paths.

On the RTX 5090, the renderer test evaluated three frames at 720p → 4K using
the signed 310.9.16 runtime. Each evaluation completed a GPU fence and
returned nonzero, finite sampled red values:

| Evaluation | Reflection input | Completed fence |
| --- | --- | ---: |
| First frame, reset | Reflection motion | 1 |
| Following frame, history | Reflection motion | 2 |
| Explicit reset | Hit distance, world-to-view and view-to-clip matrices | 3 |

On the same device the SR feature also created with its own live parameter
map and handle, completed fence 4 and released successfully. The renderer
test uses linear depth. The preceding standalone probe checks a full image.
Both tests use synthetic geometry.

To reproduce, start with a fresh external source directory, the existing
[4K build environment](README.md#build-and-launch), and a separate test
prefix. The renderer's build script expects
`lib/Windows_x86_64/nvsdk_ngx_d.lib` in its SDK directory. For the official
tag's nested `x64` layout, stage a local copy once:

```sh
mkdir -p "$VICTORY_DLSS_SDK/lib/Windows_x86_64"
cp "$DLSS_SDK/lib/Windows_x86_64/x64/nvsdk_ngx_d.lib" \
   "$VICTORY_DLSS_SDK/lib/Windows_x86_64/"
```

Here `DLSS_SDK` points to the official SDK and `VICTORY_DLSS_SDK` to the
private staging directory. From the LinVxtory root:

```sh
kit=$(realpath docs/7900)
sh build.sh "$RESEARCH_SRC"
cd "$RESEARCH_SRC"
test "$(git write-tree)" = 3d033cbd633757d2fd15f97a4323d566e8a043ba
git apply --index "$kit/rr-backend.patch.txt"
test "$(git write-tree)" = 80481c1fec1ebff7600a947633567c088c4486ea
cargo test --locked --offline --target x86_64-pc-windows-msvc \
  -p victory-hle --features dlss,diagnostics --lib --no-run
```

Use the test executable printed by Cargo. Stage both signed feature DLLs
beside it before NGX initializes and use the [probe runtime
setup](rr-probe/README.md#run). In the isolated runtime:

```sh
"$PROTON_WINE" "$TEST_EXE" --exact \
  modules::fidelityfx::ngx::rr::tests::renderer_rr_reads_back_reset_and_history \
  --ignored --nocapture --test-threads=1
```

Use the original external control directory for SR replays. A future
in-game caller must choose SR before it skips stock denoising and handle
a failure during NGX recording without presenting a partially written
command list.

## Submitted 720p cell frame

A guarded SR replay captured the cell at frame 1780 on the real AOC window.
The trace contains 154,683 texture-binding rows across 15,381 recorded
operations in its three-frame window, with zero truncation markers. The
separately captured SR input/output tuple matched the one in submission
9156, list 31, pass 6. Recording occurred in presentation interval 1779,
and the list was submitted in interval 1780. Correlate by submission and
backing identity across the interval boundary.

| Handoff | Backing at this SR call | Observed binding immediately upstream or downstream |
| --- | --- | --- |
| Color | 1280×720 RGBA16F | DOF composite bound it as a render target immediately before SR. The preceding heat-haze pass also bound the scene color for writing. |
| Depth | 1280×720 R32G8X24 typeless | Alpha velocity/depth resolve was its last observed writer binding. |
| Primary motion | 1280×720 RG16F | Heat haze bound it for UAV access. Its actual per-pixel contribution needs numeric validation. |
| Output | 3840×2160 RGBA16F | Display-resolution tiled hair filtering reads the reconstructed scene. |

The trace follows an alpha-reflection denoising product through a packed
radiance alias into the heat-haze and DOF color path. RR cannot receive
fresh noisy HDR by simply selecting feature 13 at the SR call. Moving the
evaluation before DOF also requires a valid 4K continuation for the later
effects and hair. The frame image shows the rear ponytail, wire window and
lit cell; it is an SR reference.

To reproduce this diagnostic, apply the [graph trace source
overlay](rr-graph.patch.txt) to a separate fresh reconstruction of the
4K control. From the kit root:

```sh
kit=$(realpath docs/7900)
sh build.sh "$TRACE_SRC"
cd "$TRACE_SRC"
test "$(git write-tree)" = 3d033cbd633757d2fd15f97a4323d566e8a043ba
git apply --index "$kit/rr-graph.patch.txt"
test "$(git write-tree)" = 15258f1aa5fa8f547be63ad68b246bdbe37da942
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
```

Use the [4K control replay settings](README.md#build-and-launch) and choose
a frame in the cell. The frame number varies with loading. Set
`VICTORY_PROVENANCE` to a writable private Windows filename,
`VICTORY_PROVENANCE_FRAMES` to the selected frame, `VICTORY_CAPTURE` to
an existing private Windows directory, and `VICTORY_CAPTURE_FRAMES` to
the same frame number. The diagnostic includes two warm-up intervals.

The TSV records declared texture accesses, including backing serial,
version, shape, typed view where available, and host queue order. RTV and
UAV bindings are potential writes. The trace does not establish per-pixel
execution, subresource coverage, all GPU-dependent bindless reads, or GPU
completion. Stop following a producer chain at a truncation or unknown
version. Run timing comparisons with tracing disabled.
