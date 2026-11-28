# Ray Reconstruction on the Series S 4K renderer

The control is LinVxtory `338d80e`, renderer `5b10585` (tree
`3d033cbd633757d2fd15f97a4323d566e8a043ba`). Its 1280×720 → 3840×2160
Super Resolution replay measured 10.77 median FPS and 119.01 ms p95 over
585 title Present intervals in the cell at +240 to +295 s. See the
[control build and results](README.md#historical-4k-super-resolution-control).

The default 4K preset requests one NGX Ray Reconstruction evaluation
per qualifying frame from startup. It retains native global illumination
and alpha reflection history, preserves the original wavy glass, and
replaces three native opaque reflection passes on eligible frames. The
title applies its later postprocessing and composites the UI. A frame
that misses the filter cutoff keeps stock Super Resolution and the
native filters. The first intro upscales observed in the hardware
replay still used this fallback.
[Build and launch](README.md#build-and-launch) · [Native GI and glass](#native-gi-with-original-glass)

The earlier five-filter experiment replaced the probe shader with a
hybrid that retained diffuse and caustic accumulation. Its stock-vs-RR
timing pair measured 8.54 vs 8.75 median FPS and 271.46 vs 202.39 ms
p95. Those results apply to the hybrid-GI experiment.
[Hybrid-GI filter evidence](#diffuse-history-replacement) · [Historical same-build timing](#capture-free-timing-with-the-stock-fallback-guard)

## Cell lighting stability

The 720p → 4K Series S cell replay on RTX 5090, GE-Proton 11-14 and
NGX 310.9.16 captured the same four patches at the RR input, directly
after the RR evaluation, and at DXGI Present. The largest underside
flash was already +5.31 EV in the supplied scene color, measured +5.33
EV directly after RR, and produced a 113.34-level step in encoded luma
on the displayed tile. Both frames presented optical RR with continuous
history and pre-exposure 1.0. The optical layer had zero coverage in
the measured patches.

The comparison uses adjacent frames from +230 to +410 seconds, after
the cell loaded. Counts include every exact Present pair in the window,
including provider changes.

| Reflection histories retained before RR | Exact pairs | Under-bed steps over 5 / 10 / 25 luma levels | Peak absolute step |
| --- | ---: | ---: | ---: |
| Neither opaque nor alpha | 894 | 34 / 14 / 3 | 113.34 |
| Native opaque and alpha | 947 | 6 / 0 / 0 | 7.57 |
| Native alpha only, the new default | 853 | 2 / 0 / 0 | 5.63 |

All three runs used the same diagnostic executable and retained native
GI and the original wavy glass. For uninterrupted RR without a history
reset, the underside exceeded five luma levels in 27 of 706 pairs with
the cut and 1 of 701 with native alpha alone. Native alpha alone still
replaced the three opaque passes. The fixture verified 32 full 4K images
against their matching native-alpha tiles. The +370 s full-image request
was interrupted, while exact tile capture continued to the end of the
window. These are instrumented quality results. [Fence-matched replay results](https://github.com/TomTomAIbot/LinVxtory/pull/14#issuecomment-7236347897)

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
without game files. The hardware replays below verify the separate
in-game route.

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
sh build.sh --sr --control "$RESEARCH_SRC"
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
sh build.sh --sr --control "$TRACE_SRC"
cd "$TRACE_SRC"
test "$(git write-tree)" = 3d033cbd633757d2fd15f97a4323d566e8a043ba
git apply --index "$kit/rr-graph.patch.txt"
test "$(git write-tree)" = 15258f1aa5fa8f547be63ad68b246bdbe37da942
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
```

Use the [4K launch settings](README.md#build-and-launch) with
`VICTORY_RR_AUTO=0` before loading the preset. Choose a frame in the cell. The frame number varies with loading. Set
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
the people outside easier to distinguish. RR also flattened the intentionally wavy glass. This is a visual
regression. Preserving the native refraction is an acceptance criterion. The ponytail and diamond wire remained visible. Lucia faced away
from the camera. Face detail and ghosting during turns still require
their own views.

These are pre-postprocessing provider outputs. The title continued to
present SR. Image capture and the extra diagnostic evaluation exclude this
run from a single-provider performance comparison. The separate inkVI-dive
bookmark returned within 0.003 game-world units, with identical heading
and clock. The game's camera stayed independent of the saved position.
[Reproduce the EXR pair](dive-testing.md#capture-native-4k-sr-and-rr-from-one-game-frame)

### Preserving the wavy window

In a stationary cell replay, two independent RR histories received the
same 720p game frame, the same DOF guide and a reset. One also received
the original HDR color captured before transparency. The paired SR
references were byte-identical. At Present 1766, submission 8717, the RR
output hashes were `7dd81c301c703ef0` without the guide and
`c8fe8ece5df38d79` with it. The AOC showed SR throughout this experiment.

The additional guide restored some visible structure along the roof and
table. In two fixed roof patches, mid-scale luminance contrast relative
to SR went from 64% to 71% and from 62% to 70%. Neither RR image
preserved the reference ripple. These ratios describe image contrast in
one reset frame. They cannot identify native displacement or establish
temporal stability.

To reproduce the comparison, add the [glass comparison overlay](rr-glass-compare.patch.txt)
to the [HDR pair tree](dive-testing.md#capture-native-4k-sr-and-rr-from-one-game-frame)
`5025e20d5658f9a3691999bd85fa726dcc5c4b26`. The resulting tree is
`30256e4ebd5b64af8f6fcadaffb0a0fce44b31e1`. Use the same diagnostics
build, keep the native filters on, and set `VICTORY_RR_GLASS_COMPARE=1`,
`VICTORY_RR_BACKGROUND_FRAME=1`, and `VICTORY_RR_SHADOW_LIMIT=1` in the
pair setup. Compare the original and `-before-transparency` EXR
directories after matching their Present, camera, submission, reset,
exposure and SR image bytes.

#### Native 4K frames with two opaque filters bypassed

An exact match to the closed future FidelityFX recording let the renderer
select RR before the native opaque filters submitted. The renderer kept
queue order and committed the filter bypass after it had recorded and
closed the replacement. It ran once per frame in the native FFX slot,
with zero Super Resolution evaluations in those slots.

| Native 4K capture | Opaque source submit | RR fence | RR history |
| ---: | ---: | ---: | --- |
| 1933 | 12205 | 12210 | Reset |
| 1934 | 12222 | 12227 | Reused |
| 1935 | 12239 | 12244 | Reused |

The opaque bilateral and history-update shaders each reported 32 compute
invocations in their corresponding source submits. The calibrated
vkd3d-proton indirect-argument clear accounts for all 32, leaving zero
native workgroups for both shaders. The opaque temporal shader still
recorded 88,576–89,856 invocations and the alpha temporal shader recorded
326,656–327,936 in its matching submits. The AOC captures retained the
rear ponytail and window mesh. The original glass waves were reduced in
this build, which did not supply the optical layer.

The renderer kept stock SR available. After the RR burst, source submit
12256 restored both opaque filters at 51,936 invocations each. This build
continued resetting native history on the alternating packed backing
after fallback. The run measured dispatches and captured frames. It
provides no matched median FPS or p95 comparison. [Replay instructions](dive-testing.md#verify-the-two-opaque-filter-bypasses)

#### In-game optical overlay comparison

A second stationary cell replay used the SDK’s transparency layer,
which upscales its input without denoising it. The renderer measured
where the original transmission draws changed native HDR. Inside that
footprint, it passed the original final HDR samples as the premultiplied
layer with full coverage and used the native pre-transmission HDR as
their base. Outside the footprint, the base was the original final HDR.
The two inputs recompose the original RGB at each 720p sample. The
capture measures the transmission footprint before heat haze and DOF.

At Present 1756, submission 9849, the footprint covered 195,736 of
921,600 input pixels with zero rejected samples. Independent RR
evaluations used the same frame, DOF guide and reset, and their stock SR
reference EXRs were byte-identical. Both 4K RR outputs had 8,294,400
finite, written, nonzero pixels. The title presented SR.

| Mid-scale log-luminance contrast relative to SR | Standard RR | Optical overlay |
| --- | ---: | ---: |
| Tower and silhouette | 79% | 105% |
| Right roof | 73% | 104% |
| Courtyard table slats | 55% | 103% |
| Left roof | 69% | 123% |

The tower, right roof and table recovered much of the intended wavy
detail. The left roof overshot SR contrast. These contrast ratios use
the difference between Gaussian blurs of three and nine 4K pixels.
Using the nearest-scaled native footprint, log-luminance mean absolute
error relative to SR fell 14.2% in the interior more than nine pixels
from the boundary. It rose 4.2% in the nine-pixel band on either side
of the boundary, where a few bright flecks appeared near the character’s
silhouette. Farther outside, the error was about 0.054 in both cases.
These are selected regions of one stationary reset frame before title
postprocessing. The following display test checks the stationary result
with history. Movement and timing require separate comparisons.

Add the optional [optical overlay patch](rr-glass-optical.patch.txt) to
the glass comparison tree `30256e4ebd5b64af8f6fcadaffb0a0fce44b31e1`
and verify the resulting tree before rebuilding with the same 4K
diagnostics settings.

```sh
git apply --index "$kit/rr-glass-optical.patch.txt"
test "$(git write-tree)" = f72e9620745e6b93c08671667e6d9c99c8026298
```

Use the private [HDR pair setup](dive-testing.md#capture-native-4k-sr-and-rr-from-one-game-frame)
and leave the native filters enabled. Clear the optional
`VICTORY_RR_GLASS_COMPARE`, `VICTORY_OPAQUE_CURRENT`,
`VICTORY_OPAQUE_CUT` and `VICTORY_ALPHA_CURRENT` switches. Set
`VICTORY_RR_GLASS_OVERLAY_TEST=1`, `VICTORY_RR_TRANSMISSION_FRAME=1`,
`VICTORY_RR_BACKGROUND_FRAME=1`, `VICTORY_RR_SHADOW_LIMIT=1` and
`VICTORY_DOF_GUIDE=1`. To save per-draw masks, set
`VICTORY_RR_TRANSMISSION_MASK_DIR` to an existing private directory.
Arm the trigger facing a stationary pane. Compare the ordinary and
`-native-transmission-overlay` output directories only when their
receipts agree on the Present, camera, submission, reset, exposure and
SR reference bytes. The optical receipt reports
`observed_pre_haze_transmission_pixels`. Require positive coverage and
zero rejected samples in the matching renderer log. Keep all captures
and generated files local.

#### The optical layer on the AOC display

A separate stationary replay used the native transmission input in the
selected FFX slot for Presents 1888–1890. Feature 13 retired at fences
9573, 9589 and 9605, with no SR evaluation in any of those slots. The
first frame reset history and the next two reused it. All three captured
title outputs were 3840×2160. This test kept the original native filters
active.

In the displayed output at 1889, the tower, left roof, right roof and
table retained 98%, 108%, 96% and 110% of the preceding SR capture's
mid-scale contrast. The left roof and table show coarser bright ripples.
The rear ponytail and wire mesh remain visible. This compares nearby
stationary frames after title postprocessing. The same-frame comparison
above isolates the effect of the optical layer, while movement and face
coverage require a separate sequence.

## Native output and fallback

The bounded display experiment selected RR in the actual 4K FFX slot
for Presents 2453 and 2454. Both used output serial 32396. The title
queue retired fences 19816 and 19833, with a history reset on the first
frame and reused history on the second. Each selected slot submitted
one RR evaluation and zero SR evaluations. The real AOC replay captured
4K frames 2453–2460. The ponytail and room lighting were visible. The
character faced away in the two RR captures.

Presents 2455–2457 selected SR when the current opaque producer could
not be matched. Both providers used the native denoisers throughout
this test. Preserving pane edges in motion and removing redundant native
work are still open. A 55-second RR performance comparison awaits those
changes. [Reproduce the native output test](dive-testing.md#test-the-native-4k-upscaler-slot-on-the-aoc)

A follow-up tested whether the renderer could commit the later RR slot
before the earlier opaque filters were submitted. Two more 4K RR slots
retired with zero SR GPU evaluations, at Presents 2385 and 2388. The
early decision succeeded on neither frame. The matching opaque source
submissions 19583 and 19631 recorded 15,776 and 11,168 compute
invocations respectively for **each** of the bilateral and history-update
filters. The original denoisers were still running in both frames.

The first longer camera replay selected SR in all 373 FFX slots. The
continuous RR window armed the source and located exact future FFX
recordings, but the pre-transparency snapshot still required the
five-frame diagnostic trigger. The renderer safely kept SR and the
original filters when those input anchors were missing. The test
captured the camera sequence and recorded no presented RR for analysis.

After moving native background recording into the continuous RR window,
the next replay submitted 276 optical RR slots and 86 SR slots. The
earlier atomic decision adopted zero filter bypasses. The downstream
tracker could not match these provider submissions to individual DXGI
Presents, and the camera did not reach the intended turn. The run used
capture and input and supplies no comparable timing result.

The native glass producer was independently identified in a stock 4K
cell replay. At Present 1937, submission 11993, the active transmission
draw passed 186,626 raster samples and changed 186,624 pixels of the
1280×720 HDR buffer. Its footprint excluded the character where she
occluded the pane and included the visible wire. Its bounding rectangle
would have included 86,584 additional pixels. The in-game optical
overlay comparison above uses the per-draw footprint at this point in
the pipeline.

The [November 27 Victory-fork replacement](https://github.com/fxceless/victory-fork/commit/5dbf5ce226690bb0f1b2949dc8ef822617ae81ea)
introduces a different NVIDIA renderer baseline. Its [DLSS implementation
notes](https://github.com/fxceless/victory-fork/blob/5dbf5ce226690bb0f1b2949dc8ef822617ae81ea/docs/dlss.md)
describe Streamline Super Resolution and Frame Generation. The 4K control
here remains pinned to the tested renderer.

For command-driven 4K frame captures and shareable Present receipts, see the [inkVI-dive test instructions](dive-testing.md).

### Optical input and opaque bypass on the same frames

With the experimental alpha and GI variants off, the continuous replay
retired 140 feature-13 slots with the protected glass input and the
opaque post-filter bypass selected, including the guide warmup. Each
of those slots submitted one RR evaluation and zero SR evaluations.
This run traced the selection and GPU fence. The earlier bounded test
directly measured the native dispatch counts for the two post-filters.

The 4K capture with request ID 1946 shows the face in profile, the
ponytail and ripples in the portion of the pane still in view. Captures
1954, 1956, 1968 and 1998 show the turn uncovering the bunk, far wall,
door and bookshelf. These request IDs match fenced RR slots. The resource trace still
lost attribution before final DXGI presentation, so the captures alone
cannot prove frame-by-frame provider or absence of temporal ghosting.

Stock SR returned after the RR window. In that transition, fence 16736
retired both native opaque post-filters with 104,352 compute invocations
each. Sequence 16753 accepted the refreshed packed history from that
same backing. The replay used capture and scripted camera motion, so
its frame times are unsuitable for comparison against the SR control.
[Combined reproduction](dive-testing.md#reproduce-optical-rr-with-the-opaque-post-filters-bypassed)

When the alpha replacement was also enabled, a separate bounded replay
found the exact alpha writer and its same-frame consumer in the closed
queue cohort. It declined the replacement on four frames because the
title's constant buffer changed between translation and the cutoff.
The late optical evaluation still ran for request 1881 with the native
filters enabled. The proposed alpha dispatch uses the renderer's
existing deferred constant-buffer fill, shared with the original
shader. Its GPU dispatch and timing are still under test.

### Presented RR with the opaque and alpha reflection filters bypassed

The next build bound both alpha PSOs to the same native deferred
constant buffer. Four consecutive 4K frames on the AOC used one RR
evaluation with the original glass data and zero SR evaluations in the
selected slots. The receipt follows the exact HDR output through native
compositing and dithering to an accepted DXGI Present.

| Present | Opaque source fence | Alpha shader fence | RR fence | Present fence |
| ---: | ---: | ---: | ---: | ---: |
| 1861 | 11214 | 11218 | 11219 | 11221 |
| 1862 | 11231 | 11235 | 11236 | 11238 |
| 1863 | 11248 | 11252 | 11253 | 11255 |
| 1864 | 11265 | 11269 | 11270 | 11272 |

All four original shaders reported 32 compute invocations apiece in
each paired frame. The calibrated indirect-argument clear accounted
for those counts, so the stock opaque temporal, bilateral,
history-update and alpha temporal shaders executed zero workgroups.
Current-frame packing still ran. Its counters read 91,936–95,008 for
opaque and 326,176–327,712 for alpha, including the indirect overhead.
The first RR frame reset history, and the next three reused it.

At Present 1865 the required alpha background witness was unavailable
and the title presented stock SR. Both original opaque post-filters
ran during recovery, and their exact packed history became reusable.
GI was stock for this test. The fixed-count replay used GPU queries
and capture, so the timing comparison needs separate matched runs.

### Presented RR with reflection and diffuse-probe replacement

The stationary replay with all three current-frame packers enabled
confirmed four consecutive accepted 4K RR Presents. Each used the
original glass input and one feature-13 evaluation with zero SR
evaluations in those slots.

| Present | Opaque and GI GPU fence | Alpha GPU fence | RR fence | Present fence |
| ---: | ---: | ---: | ---: | ---: |
| 1867 | 10005 | 10009 | 10010 | 10012 |
| 1868 | 10022 | 10026 | 10027 | 10029 |
| 1869 | 10039 | 10043 | 10044 | 10046 |
| 1870 | 10056 | 10060 | 10061 | 10063 |

All five original shader bodies had zero workgroups in each fenced
frame. The counters read 32 for each original opaque temporal,
bilateral, history-update, alpha temporal and diffuse-probe
temporal/caustic shader, the pinned indirect-clear cost. The current
opaque packer recorded 89,376–92,704 invocations, current alpha
327,200–328,480, and current probe 921,632. These packers retain the
data consumed by the title's lighting. Stock SR presented at 1871.
The two original opaque history backings again became reusable after
their native writers retired.

In the captured combined frame 1870, the tower, roof and table ripple
patches retained 90–99% of the mid-scale contrast of the preceding SR
frame 1861. The nearby fallback frame retained 90–105%, so nearby
frames vary even under SR. The ponytail shape was intact and the wire
mesh was softer with RR. These stationary captures show the combined
effect and cannot isolate the GI change.

The dedicated native transmission chain still produces the intended
refraction. Its result travels in RR's optical layer. Native directional
shadow visibility, local and hair shadow reconstruction, and coarse
alpha probe filtering also remain active. They generate packed
visibility and lighting inputs consumed before the FFX handoff.
Motion and material guide generation, current-frame light integration,
depth of field and final compositing still run.

The 720p trace also found a previous-scene contribution in the
half-resolution `c89f7bd38af4f529` resolve. One output feeds heat haze
and the other feeds the blur used by depth of field. Both paths reach
the FFX color. The two-current-output specialization is CPU-verified
and has no live activation in this measured build.
[Source and reproduction](dive-testing.md#verify-reflection-and-diffuse-probe-replacement-together)

### First matched capture-free timing

Both arms used the same build, RTX 5090, GE-Proton 11-14, isolated
save and cache seeds, restored cell view and windowed 1280×720 to
3840×2160 profile. The 55-second windows began after loading and a
three-second warmup. Shots and input stayed outside the timed windows.

| Arm | Median FPS | p95 Present interval | Accepted FPS over 55 s | Exact RR / SR / unknown |
| --- | ---: | ---: | ---: | ---: |
| Stock SR | 8.51 | 231.05 ms | 7.65 | 0 / 421 / 0 |
| RR enabled with fallback | 9.30 | 136.69 ms | 8.91 | 367 / 123 / 0 |

All 367 RR Presents used the optical layer and the opaque, alpha and
probe replacements. Accepted throughput rose 16.4% in this pair.
The arm switched between RR and SR 98 times. In this window 67 stock
frames lacked the current qualified camera, and 56 more followed a
frame that could therefore supply no previous camera. A separate
diagnostic sampled the first failure at the FidelityFX call. Each
sample had matching VelocityFill data and an empty independent
reflection projection bucket. Lighting instability was observed
during the opt-in run. The recorded timing
describes this mixed configuration, including its fallbacks. Stable
continuous RR and the visual impact of switching still require proof.

### Stationary lighting comparison

Two capture-only replays used the same executable and independently
confirmed the same camera, position, and title clock before arming.
Only the GI replacement switch differed. Both retained RR, the optical
layer, and the opaque and alpha replacement paths where their frame
prerequisites passed.

The sampled 90th percentile of the largest indoor patch change between
consecutive confirmed RR Presents was 0.865 EV with GI replacement
enabled and 0.032 EV with it disabled. For consecutive frames that
changed provider it was 0.890 and 0.127 EV, respectively. In the longer
RR history sequences the median change at the bunk fell from 9.59% to
1.66%. The window edges were stable to within three pixels, and the
open sky stayed steady. Exact SR fallback still showed a rarer wall
change with GI replacement disabled.

The stationary result identifies GI replacement as a major contributor
to the lighting instability in this cell. The separate camera admission
failures still cause provider switching. The captures measure visible
stability, and the capture-free timing above measures the earlier mixed
configuration.

On a separate bounded capture, fenced source taps matched three
consecutive full-route RR Presents (2155–2157). At four sampled sites
the packed GI output was byte-identical to the current RGB9E5 input
on each frame. Over those three frames the left and far wall probe
luma fell 37.5% and 23.5%, and the corresponding raw composed 720p
HDR luma fell 6.2% and 2.9% before NGX ran. The supplied title
pre-exposure stayed at 1.0. This locates part of the change upstream
of RR and motivates retaining the accumulation needed to form stable
diffuse lighting before compositing.

### Native camera continuity check

The deferred camera check keeps a fresh matching VelocityFill sample
provisional until the earlier queued opaque producer independently
confirms its projection and depth/motion identity. It publishes the
camera for the next frame only when the exact FFX submission signals.

A separate 55-second stationary hardware audit presented 454 RR frames,
all with the optical layer and the opaque and alpha replacements.
There were zero SR fallbacks and zero unknown Presents. The trace
matched all 59 deferred camera confirmations to their actual opaque
source, FFX submission, and accepted RR Present. This camera audit
used native GI to isolate continuity. Lighting and timing for the
combined build require their own replays.

### Diffuse-history replacement

The new paired GI shader keeps the primary diffuse and caustic
accumulation that feeds title lighting. It removes six previous-frame
reflection gathers, preserves the original store sites, and bypasses
the original full GI shader on qualifying RR frames.

In the bounded stationary hardware replay, four consecutive accepted
RR Presents (2013–2016) used the optical layer and all paired
replacements. The original GI kernel returned the calibrated 32 raw
invocations per frame. The paired diffuse-history kernel returned
921,632. The original opaque temporal, bilateral, history-update
and alpha temporal bodies were also at the calibrated floor.

At the sampled left wall, far wall and bunk, the median adjacent
packed GI luma change was 0.91%, 0.66% and 0.92%. The earlier
current-only replay measured 20.76%, 12.54% and 5.98% in those patches.
The hybrid source input still varied by up to 21.94% across the
indoor sites, while the packed result stayed within 1.86%. These
short bounded samples show that the retained accumulation works.
The longer visual check and capture-free matched timing are separate.
[Reproduction](dive-testing.md#check-the-deferred-camera-and-diffuse-history-gi-replacement)

The longer stationary capture sampled 47 consecutive RR pairs. The
90th-percentile peak indoor patch change was 0.088 EV, down from 0.865 EV
in the current-only replay. With native GI it was 0.032 EV. The sampled
stone and sky medians stayed steady on consecutive RR frames. A local
dark bunk patch still changed by 0.259 EV. The tower, roof and table
ripple patches retained 95–98% of the preceding SR contrast. The
ponytail outline remained intact, with softer fine hair and wire detail.

The 55-second visual capture contained 282 RR and 11 SR Presents.
281 of the RR Presents used all paired replacements. These capture
counts establish route and image evidence, and the timing run uses the
same build without captures.

### Capture-free timing after the lighting and camera changes

A same-build pair used `df0553c`, the same isolated Series S cell pose,
and the same 720p-to-4K AOC window. Both 55-second measurement windows
started after loading and warmup. Capture, per-pass queries and camera
diagnostics were disabled throughout the measurement.

| Arm | Median FPS | p95 Present interval | Accepted FPS over 55 s | Exact RR / SR / unknown |
| --- | ---: | ---: | ---: | ---: |
| Stock SR | 9.01 | 184.08 ms | 8.38 | 0 / 461 / 0 |
| Diffuse-history RR | 9.11 | 118.60 ms | 9.07 | 499 / 0 / 0 |

Every accepted RR Present included the original glass input and 498 of
499 used all verified replacements. Frame 2332 used late RR with the
native filters retained because an optical producer was pending at the
earlier cutoff. This one-pair result measured 8.2% higher accepted
throughput and 35.6% lower p95. The follow-up below changes that late
selection to stock SR.

### Stock fallback at the native cutoff

The follow-up build requires a committed early replacement for atomic RR.
On the same RTX 5090 replay, Present 1952 had no eligible previous camera
at the native filter cutoff. The later FidelityFX slot kept the original
SR command. Its signal and accepted 4K Present identify stock SR with
all replacement flags clear. The same signal certified the camera for
the next frame, and the replay presented full-route RR after the warmup.
[Diagnostic reproduction](dive-testing.md#keep-stock-super-resolution-when-the-rr-cutoff-is-missed)

In the first camera-turn run of this build, 227 of 229 accepted Presents
used the full RR route and the other two used exact stock SR. The native
Presents show confirmed fallback at 2004 and 2071. The 47 verified RR
images show the ponytail and newly revealed lighting on the wall and
shelf. Nearby frames around the shelf showed no large duplicate edges.
The two replays reached different character and door poses, so this
sample cannot estimate a matched disocclusion error. Pipeline first-use
work overlapped the capture window. Timing comes from a separate run.

The delayed replay put the stationary window capture ahead of the first
movement. In the same-view comparison, stock Present 2295 and full RR
Present 2103 retain the tower and roof distortion. At those sites RR
retained 96–97% of SR’s mid-scale display contrast. The wire mesh was
softer. Exact native output lineage identifies all eight stationary RR
images. Across the full delayed camera replay, 206 Presents used the
complete RR route, 11 used stock SR and zero were unknown.

During the first turn, full RR Presents 2254–2256 show the eye, nose,
lips and ear in profile, with the ponytail and flyaways intact. A
locally aligned door and wall crop showed no obvious duplicate edge or
persistent silhouette trail in the sampled RR frames. The character
angles differed from the stock run, and full-frame registration failed
across the turn. The review uses locally aligned crops and adjacent
frames from the same replay.

### Native GI with original glass

The native-GI validation build `a53eaa18` presented the 4K cell frame
1914 on the physical AOC. Its fenced FidelityFX slot used one RR
evaluation and zero SR evaluations. The Present receipt identifies the
original optical input and confirms the four reflection replacements
with native GI selected. The pane retained the visible wavy distortion
and wire mesh. This is frame-level image and route evidence.

### Continuous startup investigation

A normal launch can request RR without a benchmark trigger. The physical
AOC replay confirmed full-route 4K RR over more than four minutes. The
first intro FidelityFX slot still used SR, before the later accepted
scene path had enough guide and queue evidence.

A closed HDR and alpha bracket can contain no captured pane draw. In
continuous mode, the proof now accepts that empty pane set when all
lists passed capture validation. A native pane still requires its
original depth/HDR serials and draw ordering. The alpha replacement
still needs its exact writer and reader. With no pane, the already
composed title color goes directly into RR.

An earlier hybrid-GI diagnostic of this change executed feature 13 on
intro FFX frame 1369 with a history reset, zero SR GPU evaluations and all five
targeted replacements. The first FFX frame, 1366, still executed SR.
An animated letterbox limited accepted Present attribution for the early
RR frames, so this is a GPU-slot result.

A separate exact input/output tap on a stationary frame pair measured
the under-bunk metal dropping by 3.15 EV in the color entering RR and
2.07 EV in its linear output. Both frames used RR history, fixed
pre-exposure and an empty optical layer at that patch. The image
variation begins upstream of RR on that pair.

### Default preset replay

The exact default renderer tree `d6adab8` and native-GI preset ran on
CachyOS, GE-Proton 11-14 and an RTX 5090 with the signed DLSS 310.9.16
runtime. The replay used read-only input and a separate offline prefix,
save and cache. It ran on the physical AOC with 1280×720 input and
3840×2160 output, without a timed RR trigger.

The first 720p-to-4K upscale used SR. The first verified full 4K RR
Present arrived 63.4 seconds later during the transition into the cell.
The quiet profile gives an upper bound for RR activation because the
earliest intro operations lack full-display attribution. A separate
startup diagnostic with the same lighting reached its first fenced RR
evaluation on the fourth upscale, 0.81 seconds after the first
upscale submitted to the GPU.

Over seconds 240–295 in the loaded cell, 451 of 453 verified Presents
used RR with native GI and the original optical input. The other two
used stock SR. The capture-free interval measured **8.37 median FPS**
and **133.31 ms p95 title Present interval** across both paths. The
449 intervals between consecutive verified RR Presents measured
**8.36 median FPS** and **131.21 ms p95**. This is one default-preset
measurement with no paired performance control.

The wavy pane and hair remained visible. Separate exact taps at the
reported through-glass tree and dark post found temporal variation
before RR. Pane coverage stayed steady in the sampled tree tile. RR
reduced the variation, with residual fine shimmer visible through the
glass. Native sampling and subpixel jitter are still combined in those
measurements.

### Capture-free timing with the stock fallback guard

The historical hybrid-GI experiment used the same executable for both
arms on the physical AOC with the same Series S checkpoint, stationary
cell pose, save/cache seed and 720p-to-4K
profile. Each run excluded loading and the three-second guide warmup
from its 55-second measurement. Capture, motion and pass diagnostics
stayed outside the measurement. Before each measured arm in the timing
pair, the GPU reported P0 at 2,977 MHz and about 152 W.

| Arm | Median FPS | p95 Present interval | Accepted FPS over 55 s | Exact full RR / SR / unknown |
| --- | ---: | ---: | ---: | ---: |
| Stock SR repeat | 8.54 | 271.46 ms | 7.02 | 0 / 386 / 0 |
| RR enabled | 8.75 | 202.39 ms | 7.93 | 434 / 2 / 0 |

All 434 RR Presents used all five filter replacements and the original
optical layer. At 2651 the exact closed packet was unavailable, and at
2652 the previous camera was unavailable. Both frames presented stock
SR. This pair measured 2.4% higher median FPS and a 25.4% lower p95 in
the opt-in arm. The counts include its two fallback frames.

The first SR-only control on the same pin measured 4.57 median FPS and
361.15 ms p95. The RR launch already presented stock frames faster
before the trigger was armed. The size and cause of this run-to-run
variation remain unresolved. The table uses the repeated SR control
and describes this tested cell and pair.
