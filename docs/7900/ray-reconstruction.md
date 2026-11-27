# Ray Reconstruction on the Series S 4K renderer

The control is LinVxtory `338d80e`, renderer `5b10585` (tree
`3d033cbd633757d2fd15f97a4323d566e8a043ba`). Its 1280×720 → 3840×2160
Super Resolution replay measured 10.77 median FPS and 119.01 ms p95 over
585 title Present intervals in the cell at +240 to +295 s. See the
[control build and results](README.md).

The intended RR path uses one NGX Ray Reconstruction evaluation to produce the
4K scene from noisy 720p lighting. The title can then composite the UI.
SR remains the control and the fallback. The branch is experimental.
The repaired cell test completed one offscreen Ray Reconstruction
evaluation on title inputs at 4K. The displayed game still used SR.

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

The original three-frame test also created SR with its own live parameter
map and handle, completed fence 4 and released successfully. The current
test adds independently initialized absent, zero, signed-difference, and
repeat-absent DOF guide arms. It checks that omission clears the binding
and that all 19 RR submissions complete before the SR coexistence check
at fence 20. With model preset F on SDK 310.9.16, the omitted and zero
controls matched at the sampled blur edge. The signed guide changed the
output by up to 18 binary16 representable steps at those coordinates. The renderer
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
test "$(git write-tree)" = ea647718da0ebb9a65b1086766d85a820b35bb08
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

### Guide bindings in that frame

The opaque reflection prepass (`7168dd781ca7efb9`) read the following
resources after the tiled weathering/decal pass. Offsets are dwords from
the bound descriptor table for this shader. The typed view matters when
decoding color and when selecting the depth plane.

| Candidate | Table slot + offset | Backing and typed SRV |
| --- | --- | --- |
| Base color and material selector | 0 + 568 | RGBA8 typeless, sRGB view, 1280×720 |
| Packed normal and roughness | 0 + 920 | RGBA8 UNORM, 1280×720 |
| Encoded specular material | 0 + 664 | RGBA8 UNORM, 1280×720 |
| Opaque motion | 0 + 528 | RG16F, 1280×720 |
| Opaque depth | 0 + 752 | R32G8X24 typeless, R32_FLOAT_X8X24_TYPELESS view, 1280×720 |

Opaque reflection distance uses a **640×360 RG16F** texture. The prepass
binds it at UAV 1 + 120, the ray tracer (`6d7fe7745ab9c6d8`) at UAV
1 + 144, and screen-space resolve at SRV 1 + 64. A second, separate
allocation serves the alpha path. Both the primary depth and motion
resources receive later updates before the final upscale.

The opaque temporal and bilateral chain updates a packed R32_UINT
resource. An alias copy supplies an RGB9E5 view to standard tiled PBR
at SRV 0 + 456. A separate alpha temporal resolve supplies a packed
product to a later alpha draw. Both feed the backing that eventually
reaches the upscaler. A global dispatch skip would leave missing lighting
inputs as well as stale history.

The SDK also supports retaining aligned pre-upscale depth of field by
supplying a signed, single-channel FP16 luminance difference between the
blurred and pre-blur colors. The 720p graph identifies a pre-blur source
and the immediate post-blur SR input. This path still requires checked
same-frame captures and validation on the active model.
[NVIDIA RR guide, §3.4.13](https://github.com/NVIDIA/DLSS/blob/v310.9.16/doc/DLSS-RR%20Integration%20Guide.pdf)

### Values sampled on the GPU

A separate SR replay copied native texels at selected points on the title's
command lists and read them after completion. In the opaque 640×360
reflection allocation, X contained 162 positive values, 29 values of −1
and 65 zeros across 256 sampled locations. The largest value was 854.5.
Y contained 69 positive infinities and one negative infinity. The three
grids taken after the prepass, after ray tracing and before screen-space
resolve were byte-identical at those coordinates. This verifies the sampled
values persisted through that interval. It does not establish that the
tracing dispatch wrote those pixels on this frame.

The title maps each sparse sample to one of four primary pixels using
frame-dependent parity. The measured extents were 640×360 and 1280×720.
Two groups recorded near one presentation boundary used distinct parity
seeds and camera projections. The current camera-relative view-projection
bytes agreed between a geometry witness and the reflection producer in the
first group. Joining guides solely by the latest Present number would mix
different camera samples. The sparse distance and its preserved Y channel
do not supply dense reflection motion.

Across these replays **the visible title uses 4K Super Resolution and its
native denoisers**. All RR evaluations reported so far used the standalone
or renderer hardware fixtures. A valid gameplay comparison requires both a
confirmed in-game feature-13 evaluation and proof of the specific stock
filters it replaced.

## Second SR control

The published control executable completed a second offline, windowed AOC
cell replay on the same driver, warm-cache seed and save. Over elapsed
seconds 240–295, 427 title Present intervals gave **8.64 median FPS** and
**171.37 ms p95**, versus 10.77 FPS and 119.01 ms in the original control.
Timing excluded loading and screenshot collection in that window. This
is run-to-run variation within SR; there is no matched RR measurement yet.

The post-timing sequence shows the player's face in profile, the ponytail,
wire window, bedding and room lighting during the turn. The short
high-cadence crop during the turn coincided with a game stall and contained
no new title Presents. That crop cannot establish frame-to-frame ghosting.

## Camera and depth-of-field measurements

The Series S velocity-fill pass supplies a rotation, a right-handed
projection, an inverse rotation and a world-space camera position. In one
cell sample, its camera-relative view-projection matched the projection
used by the reflection tracer to within 6.3×10⁻⁸ in the coefficient
comparison. The signed projection shift was (−0.28414344, +0.01111841)
render pixels, matching the same camera sample's upscale jitter to within
6×10⁻⁹ pixels. An adjacent geometry recording carried another frame-begin
identifier. These checks qualify one observed camera and projection.
The [upstream velocity-buffer investigation](https://github.com/Marvisak/VIctory/pull/2751)
provided an independent starting point for the camera layout.

GPU readbacks of the 720p pre-DOF and post-DOF HDR from the same command
list and backing produced finite signed luminance differences at all 256
sampled positions. Sixteen changed. The range was −0.01285 to +0.00550.
This identifies an aligned source for the SDK's single-channel blur guide.
The signed-difference guide is separate from the noisy lighting RR needs.

To reproduce the bounded point samples, apply the [guide sampling
overlay](rr-guide-samples.patch.txt) to its own fresh reconstructed 4K
control tree and build the diagnostics profile as for the graph trace.
The resulting tree must be `b591699968e6d683e3533e0ca0332838dd7554f4`.
Set `VICTORY_CAPTURE` to a private Windows directory and
`VICTORY_RR_SAMPLE_FRAME` to the chosen recording interval. For example,
this selector samples the opaque prepass's material SRV and the
ray-tracing dispatch's distance UAV:

```sh
export VICTORY_RR_SAMPLE_TABLES='7168dd781ca7efb9,srv,0,664;6d7fe7745ab9c6d8,uav,1,144'
```

Use the actual camera, command list, backing serial and occurrence in the
results. The same shader can process a separate transparent surface, and
recording for a title frame can span two Present intervals.

The next instrumented game run produced the signed blur guide at all
921,600 render pixels. The fenced FP16 readback contained 299,219 zeros,
407,333 positive values, 215,048 negative values and zero nonfinite
values. Its largest absolute value was 7,316 in native HDR luminance
units. The effect of that range on an RR gameplay frame remains untested.

The frame-1780 window capture showed the opening cutscene. Retired D3D12
pipeline-statistics queries counted 122,624 compute invocations for the
opaque temporal denoiser and 517,632 for one alpha temporal dispatch.
These measurements confirm actual GPU execution of both stock filters in
the SR control. A gameplay bypass must demonstrate the corresponding
work was removed and the 4K presented frame came from RR. A fixed frame
number alone does not reproduce the scene across runs.

Apply the [stage-one instrumentation](rr-stage1.patch.txt) immediately
after the renderer backend overlay to reproduce the guide and execution
counters. Starting from the control tree above, applying those two patches
in order produces tree `f1d5002bb0f51a99c7a052800365f8ba4628a4b9`.
Build the Windows diagnostics profile with the pinned SDK, then set
`VICTORY_DOF_GUIDE=1`, `VICTORY_DOF_CHECK_FRAME=1779` and
`VICTORY_PASS_STATS_FRAME=1780`. The DOF count describes the stored guide.
The conversion maps nonfinite input differences to zero and clamps to
representable FP16. Use a separate capture to identify the scene.

The cell graph also contains filters that supply other essential inputs.
The local-light denoiser updates a packed shadow visibility atlas and tile
metadata used by light pruning. The diffuse screen-probe filters supply
the lighting gather and later reflection and caustic work. The alpha probe
feeds particle shading and transmission. Skipping those passes without
valid replacement outputs changes scene lighting. A separate 4K dithering
draw follows the current NGX handoff. Its contribution to visible grain
has yet to be measured.

## Reflection bypass test in the playable cell

After movement and camera prompts appeared in the AOC window, a five-frame
capture compared the native filters with the renderer's bypass flag. The
flag was set, and the opaque and alpha resolves received private
current-frame history controls. Nevertheless, fence-retired GPU counters
measured **49,537, 49,537, 48,897, 49,281 and 49,985** compute invocations
in each of opaque bilateral and history update. A separate hardware
predication fixture had skipped its dispatch successfully. The title test
failed the bypass criterion. The player was still seeing 4K SR.

The real sparse reflection classifier found 146–150 freshly traced hits
and 795–832 screen-space hits per 640×360 input on three cell frames. It
checked the same opaque depth serial, camera and native write sequence.
Most positions had insufficient evidence of a fresh hit. Matching actual
guide coverage and confirming a presented feature-13 frame are still open.

## Offscreen title evaluation at Present 1918

A subsequent cell replay on the RTX 5090 under GE-Proton 11-14 matched the
opaque prepass to its own material record and paired the current and
previous SR cameras in host submission order. The native reflection pass
ran in submission 11770. The dense guide and offscreen RR handoff used
the captured inputs for Present 1918, with the input snapshots retired
under submission 11774.

| Check | Result |
| --- | --- |
| Dense reflection motion at 1280×720 | 3,226 reconstructed candidates, 3,221 changed from primary motion, zero unexpected changes to primary-motion fallback, zero nonfinite outputs |
| Guide coverage | 72,998 unsupported positions, 818,575 rough surfaces, 26,780 without a compatible hit and 21 explicit reflection misses. Zero pixels were marked as fully qualified reflection motion. |
| Offscreen RR at 3840×2160 | Feature 13 evaluated and retired. All 8,294,400 pixels were finite, written and nonzero. The readback hash was `b6cb0167feaa47ec` (FNV-1a 64). |
| Display | The AOC continued to show the SR title output. The RR image was read back privately and did not feed presentation. |

The command-count cut brought both targeted opaque post-filters to 32
query invocations in each of four captured pairs. The calibrated
vkd3d-proton indirect-argument pass accounts for all 32, leaving zero
native workgroups for bilateral filtering and history update. The opaque
temporal resolve and alpha temporal shader still ran. For the linked
submissions, they recorded 87,040 and 327,168 invocations respectively.
The feature-13 test used captured title HDR, diagnostic opaque material
guides and dense candidate reflection motion. It omitted the DOF guide.
Complete material ownership after alpha composition and a noisy lighting
handoff with all replaced denoisers bypassed remain open. This replay
does not establish presented RR image quality, a matched performance
result or a verified named-bookmark return.
[Counter calibration and report fields](dive-testing.md#first-live-repeat)

### Reproduce the repaired cell test

The [experimental in-title build recipe](dive-testing.md#reproduce-the-experimental-in-title-rr-test)
applies the [cell join overlay](rr-cell-join.patch.txt) after the three
original patches. The overlay includes the material and camera joins and
the capture receipt changes. It takes the original experimental renderer
tree `dbe076f814f66e1ddad92c7e2e924e0f0eca92bf` to
`471f1db55f6437dcc62b5b3ffdb0c79dc542c28c`. Use the documented five-Present
trigger when the cell is visible.

The reconstructed renderer matches the graphics and capture code in the
live build, `db305be`. That build also contained an optional bookmark
command channel. Its position request timed out after the selected
frame, and the replay performed no bookmark action. The published
overlay omits that channel and cross-builds with the diagnostics profile.
Record your executable hash and the actual Present selected by the
trigger. The live executable SHA-256 was
`ea4fa1521ee13016ffb7e5bd66fa6aceccedc43aa0342939f852a30392630db5`.
Check for `RR dense reflection-motion candidate completed` and
`RR title shadow output retired` naming the same title frame. The case
above reports only one completed offscreen evaluation.

## Same-frame 4K visual comparison

A second real cell replay produced three consecutive, fenced 4K RR
outputs with the Preset F depth-of-field guide bound. The five-Present
trigger requested four pairs. Three had the input receipts needed to
complete the comparison. Each matched SR image came from the same FFX
handoff and shared the same experimental lighting and filter cuts.

| Present | RR history | RR output FNV-1a 64 |
| --- | --- | --- |
| 2250 | Reset | `e738878095dd661b` |
| 2251 | Reused | `8c3caf32c3356a34` |
| 2252 | Reused | `408a4510e16d6162` |

Each RR output had 8,294,400 finite, written, nonzero pixels. Present 2252
referenced the preceding camera at submission 17590. Its SR output hash
was `d2ff0108d37ca6bd` and both EXRs recorded pre-exposure 1. Viewed with
one exposure and tone curve, RR reduced speckle on the window and made
the people outside easier to distinguish. The window also lost some fine
texture. The ponytail and diamond wire remained visible. Lucia faced away
from the camera. Face detail and ghosting during turns still require
their own views.

These are pre-postprocessing provider outputs. The title continued to
present SR. Image capture and the extra diagnostic evaluation exclude this
run from a single-provider performance comparison. The separate inkVI-dive
bookmark returned within 0.003 game-world units, with identical heading
and clock. The game's camera stayed independent of the saved position.
[Reproduce the EXR pair](dive-testing.md#capture-native-4k-sr-and-rr-from-one-game-frame)

The [November 27 Victory-fork replacement](https://github.com/fxceless/victory-fork/commit/5dbf5ce226690bb0f1b2949dc8ef822617ae81ea)
introduces a different NVIDIA renderer baseline. Its [DLSS implementation
notes](https://github.com/fxceless/victory-fork/blob/5dbf5ce226690bb0f1b2949dc8ef822617ae81ea/docs/dlss.md)
describe Streamline Super Resolution and Frame Generation. The 4K control
here remains pinned to the tested renderer.

For command-driven 4K frame captures and shareable Present receipts, see the [inkVI-dive test instructions](dive-testing.md).
