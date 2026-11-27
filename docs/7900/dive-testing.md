# Repeating a 4K cell capture with inkVI-dive

Use this when several testers need a short sequence from the same view.
LinVxtory's Series S capture adapter accepts OctoAlgo’s `dcmd.sh cap N`
protocol on the windowed 720p → 4K DLSS build. You can request another
sequence from the running title as soon as the first sequence has completed.
GPU capture can change frame times, so use separate capture and timing
windows. [OctoAlgo’s pinned inkVI-dive source](https://github.com/OctoAlgo/inkVI-dive/tree/479a9c20ed5bc4843bdf8c6ad1a199f939f991cb)

## Build the capture-enabled SR control

Start with the [4K toolchain and runtime](README.md#build-and-launch).
Set `DIVE_SRC` to a fresh external renderer checkout location and run these
commands from LinVxtory. The optional overlay adapts the upstream capture
work by OctoAlgo under GPL-3.0.

```sh
kit=$(realpath docs/7900)
sh build.sh "$DIVE_SRC"
cd "$DIVE_SRC"
test "$(git write-tree)" = 3d033cbd633757d2fd15f97a4323d566e8a043ba
git apply --index "$kit/dive-capture.patch.txt"
test "$(git write-tree)" = deab75f463d424fbb05ed8248d8bafbe28f92444
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
```

Run the published 4K profile with the same read-only title, isolated
saves, warm-cache seed, and a real window. Give each process its own
command and capture directories. Set the following Windows paths in the
title process environment, substituting paths inside your offline mount.

```sh
export VICTORY_CAPTURE_CONTROL='Z:\path\to\run\channel'
export VICTORY_CAPTURE='Z:\path\to\run\captures'
export VICTORY_CAPTURE_FULL=1 VICTORY_CAPTURE_REQUEST_ONLY=1
export VICTORY_FRAME_LOG=1
```

## Request and share a test case

Confirm the character, wire window, and movement/camera prompts are
visible. From the host, request two contiguous title Presents.
The native capture supports bursts of one to eight frames.

```sh
sh "$DIVE_TOOLS/dcmd.sh" ./run/channel 'cap 2'
```

The reply acknowledges the scheduled Present IDs. To wait for complete
files, use the helper in the [capture receipt PR](https://github.com/OctoAlgo/inkVI-dive/pull/1).
Set `DIVE_TOOLS` to the helper checkout's `tools` directory. Each invocation
requests a new burst.

```sh
python3 "$DIVE_TOOLS/capture-receipt.py" ./run/channel ./run/captures \
  --count 2 --timeout 45 > run/capture.json
```

Share the renderer tree and executable hash, title build and profile,
input/output sizes, GPU/driver/Proton versions, cache condition, and the
receipt's Present IDs, dimensions and hashes. Keep game data, saves,
runtime binaries, screenshots and raw logs local. A file hash identifies a
particular readback. Similar views from different machines can have
different hashes.

For a performance comparison, confirm the same playable view before and
after a continuous 55-second segment, with capture and scripted camera
movement outside the segment. Report median FPS, p95 Present interval
and sample count. An RR result also needs confirmation that the presented
output came from feature 13, which title denoisers actually bypassed,
and whether SR fallback occurred during the interval. A feature creation
or an offscreen evaluation alone only verifies that stage.

Enable `VICTORY_FRAME_LOG=1` for timing arms. Supply UTC boundaries for
the visually verified playable segment to the [Present timing tool](present-window.py.txt).

```sh
python3 docs/7900/present-window.py.txt title.log "$PLAY_START_UTC" "$PLAY_END_UTC"
```

The tool selects intervals by their end timestamp, including both
boundaries. FPS is the reciprocal of the median frame interval, and p95
uses the nearest rank. Keep captures outside the measured window. Include
the run's selected provider and fallback counts alongside the JSON.

## Reproduce the experimental in-title RR test

Start from another fresh build of the 4K control. Apply the following
overlays in order. They include the inkVI-dive capture adapter and the
material and camera repair used in the successful offscreen test. The resulting diagnostics renderer still presents SR and can run a
separate feature-13 evaluation on captured title inputs.

```sh
kit=$(realpath docs/7900)
sh build.sh "$RR_TEST_SRC"
cd "$RR_TEST_SRC"
test "$(git write-tree)" = 3d033cbd633757d2fd15f97a4323d566e8a043ba
for patch in rr-backend rr-stage1 rr-cell-test rr-cell-join; do
  git apply --index "$kit/$patch.patch.txt" || exit 1
done
test "$(git write-tree)" = 471f1db55f6437dcc62b5b3ffdb0c79dc542c28c
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
```

Include the SDK 310.9.16 release `nvngx_dlssd.dll` in the private runtime
beside the SR DLL. Keep ambient substitution off and use the published
4K profile. In addition to the capture switches above, set:

```sh
export VICTORY_RR_PREPARE=1 VICTORY_RR_CAMERA=1
export VICTORY_OPAQUE_CURRENT=1 VICTORY_OPAQUE_CUT=1 VICTORY_ALPHA_CURRENT=1
export VICTORY_RR_REFLECTION_DENSE=1 VICTORY_RR_MATERIAL_GUIDES=1
export VICTORY_PASS_STATS_FRAME=1 VICTORY_RR_MATERIAL_FRAME=1
export VICTORY_RR_REFLECTION_CAPTURE=1 VICTORY_RR_REFLECTION_CAPTURE_FRAME=1
export VICTORY_RR_ORDER_FRAME=1 VICTORY_RR_BACKGROUND_FRAME=1 VICTORY_RR_SHADOW_FRAME=1
export VICTORY_RR_STAGE_TRIGGER='Z:\path\to\run\captures\arm-stage'
```

Start with a fresh run directory. Once gameplay is visible, create
`arm-stage` in the capture directory from the host. This selects the next
five Present buckets after the trigger is observed. Request your first
capture burst in that window. Keep each burst’s JSON and record the
trigger’s `first_present` from the title log.

| Report field | Evidence in the same run |
| --- | --- |
| In-process RR creation | `Ray Reconstruction feature preflight retired` with feature 13, completed fence, 1280×720 input and 3840×2160 output. |
| Opaque filter cut | Retired statistics for shaders `dce085c07b9414b6` and `6fad2980c2774c90` at the selected recording frames. Include actual invocation counts for both and confirm the temporal resolve and lighting continued. A missing counter is inconclusive. |
| Coverage | Retired material status and dense reflection-motion counts. Include unclassified or unsupported positions. |
| Actual offscreen evaluation | `RR title shadow output retired`, feature 13, all 8,294,400 output pixels finite and written, and the reported output hash. This build presents SR; the receipt identifies the separate GPU test output. |

Mark missing evidence as missing. Timing this instrumented cut experiment
does not measure the speed or image quality of a presented RR replacement.

## First live repeat

On the RTX 5090 with GE-Proton 11-14, the first command returned eight
3840×2160 PNGs, Presents 2458–2465, in 3.974 seconds. The second command
returned three, Presents 2636–2638, in 1.631 seconds. Both came from the
same AOC cell session. All eleven SHA-256 hashes were independently
checked against the files. The first burst overlapped the GPU measurement
window beginning at Present 2457. These times include command handling,
capture, disk writes and receipt generation. They exclude boot. A later
check found redundant X11 caption writes during these runs. The times
record observed command latency and are unsuitable for performance comparisons. [Upstream test result](https://github.com/OctoAlgo/inkVI-dive/pull/1#issuecomment-7222712410)

This run retired the in-game feature-13 creation fence at the requested
resolutions. The revised indirect-command limit brought each targeted
post-filter query to **32 compute invocations** in the sampled cell.
Both shaders use 64-thread groups. Four paired submissions (18202,
18218, 18234 and 18250) each recorded exactly 32 for each filter.
A hardware calibration using the renderer’s UAV-to-indirect-argument
barrier measured two count-limited dispatches with 64 counter invocations
and zero writes to a target UAV. The corresponding ungated single dispatch
measured 768 invocations and 768 writes. With the two dispatches enabled,
the counter read 1,600 for 1,536 writes. The translator runs a 32-thread
shader to clear indirect arguments for each zero-count call. This accounts
for the full 32 in each title query. Neither native filter executed a
workgroup in the four captured pairs.
[Indirect count translation](https://github.com/HansKristian-Work/vkd3d-proton/blob/71cae091b06571e51446cf2a937f0b04c237c04d/libs/vkd3d/command.c#L18923-L18937)
· [32-thread argument shader](https://github.com/HansKristian-Work/vkd3d-proton/blob/71cae091b06571e51446cf2a937f0b04c237c04d/libs/vkd3d/shaders/cs_execute_indirect_multi_dispatch.comp#L3-L53)

The title produced GPU snapshots of the camera, HDR, depth and
motion, but the retained material sample belonged to a different prepass
recording. The diagnostic withheld its RR evaluation and the cell
continued to present 4K SR.

The next replay reproduced the same-frame material and camera join and
evaluated RR offscreen at Present 1918. The AOC showed the title’s SR
output. [4K output and guide coverage](ray-reconstruction.md#offscreen-title-evaluation-at-present-1918)

## Capture native 4K SR and RR from one game frame

After applying the four cell-test patches, add the optional [HDR pair
overlay](rr-cell-pair.patch.txt) and rebuild with the same diagnostics
flags. This adds a local EXR writer and a bounded evaluation sequence.

```sh
git apply --index "$kit/rr-cell-pair.patch.txt"
test "$(git write-tree)" = 5025e20d5658f9a3691999bd85fa726dcc5c4b26
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
```

Set `VICTORY_RR_PAIR_DIR` to a fresh, writable Windows path in the
run’s private capture directory. Set `VICTORY_RR_SHADOW_LIMIT=4`
and `VICTORY_DOF_GUIDE=1` in addition to the cell-test flags above.
Arm the five-Present trigger after the playable scene appears. Allow
about 127 MiB for each two-image pair.

Each completed `frame-<present>-submit-<submission>` directory contains
`rr.exr`, `sr.exr` and `receipt.txt`. Both images preserve the native
3840×2160 RGBA16F upscaler output before downstream title post-processing
and UI. Both use the same experimental current-frame lighting and
post-filter cuts. The reference is the SR provider in this experiment.
Use the published control separately to compare against its native lighting.

Use the same exposure and view transform on both images. Record the
Present, preceding camera receipt, reset flag, pre-exposure and hashes.
A continuous history sequence starts with a reset and only reuses history
when the current view references the preceding accepted camera. The
title continues to present SR during this comparison. Keep the EXRs and
game images local.

## Test the native 4K upscaler slot on the AOC

From the LinVxtory research branch, `sh build.sh --rr ../linvxtory-rr`
reconstructs and builds the native output test in its own directory.
The default `sh build.sh ../linvxtory-sr` builds the published SR control.
Each mode checks the resulting renderer tree before compiling.

For a manual build on the source tree from the HDR pair build, apply the optional [native
display overlay](rr-native-display.patch.txt). The diagnostics build can
select a single 4K provider on the title queue before the output's later
consumers. It requires a precreated RR feature, a matching opaque
producer, camera and DOF guide, frozen FFX inputs and a compatible 4K
output UAV. The native lighting filters run for this experiment. An
unselected or rejected RR list leaves the closed SR list available.

```sh
git apply --index "$kit/rr-native-display.patch.txt"
test "$(git write-tree)" = fdf5181bb2e3676e33e4e79f331cd953347d5fb7
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
```

Use the same private directories, 4K profile and capture controls. In
a fresh launch environment configure the following switches. The
`VICTORY_RR_STAGE_TRIGGER` path must be absent when the run begins.

```sh
unset VICTORY_OPAQUE_CURRENT VICTORY_OPAQUE_CUT VICTORY_ALPHA_CURRENT
unset VICTORY_RR_SHADOW_FRAME VICTORY_RR_PAIR_DIR VICTORY_RR_BACKGROUND_FRAME
export VICTORY_RR_PREPARE=1 VICTORY_RR_CAMERA=1 VICTORY_DOF_GUIDE=1
export VICTORY_FFX_SR_SECTION=1 VICTORY_RR_NATIVE_DISPLAY=1 VICTORY_RR_DISPLAY_FRAME=1
export VICTORY_RR_REFLECTION_CAPTURE=1 VICTORY_RR_REFLECTION_CAPTURE_FRAME=1
export VICTORY_RR_REFLECTION_DENSE=1 VICTORY_RR_MATERIAL_GUIDES=1
export VICTORY_RR_MATERIAL_FRAME=1 VICTORY_RR_ORDER_FRAME=1 VICTORY_PASS_STATS_FRAME=1
export VICTORY_RR_STAGE_TRIGGER='Z:\path\to\run\captures\arm-stage'
```

Confirm the game is visible and the stock SR section has submitted before
arming the trigger. Save an inkVI capture burst around the five selected
Presents and a second burst after the window. Check the reported feature,
number of submitted SR evaluations in the chosen slot, output serial,
submission and fence. A candidate that declines reports the reason and
keeps SR. The captured on-screen view, adopted FFX output and retired
queue receipt answer different parts of the presentation check.

## Verify the two opaque filter bypasses

Build the 4K RR renderer in a separate directory as above. Apply the
optional [opaque cut test overlay](rr-opaque-cut.patch.txt) to its
diagnostics source and rebuild.

```sh
test "$(git write-tree)" = fdf5181bb2e3676e33e4e79f331cd953347d5fb7
git apply --index "$kit/rr-opaque-cut.patch.txt"
test "$(git write-tree)" = 422ada24652eb1444303ac1d8c91157d66a6a61e
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
export VICTORY_RR_EARLY_SOURCE=1 VICTORY_RR_ATOMIC_CUT=1
```

Use the same trigger, offline replay and capture settings. Keep
`VICTORY_OPAQUE_CURRENT`, `VICTORY_OPAQUE_CUT` and
`VICTORY_ALPHA_CURRENT` unset. Those switches select independent
diagnostics. For each chosen frame, match the adopted cutoff's
`sequence` to `source_submit` on the selected FFX record, and match
the selected FFX fence to its retirement. Look for both shader hashes
`dce085c07b9414b6` and `6fad2980c2774c90` in retired pass statistics
for that exact source. With the pinned vkd3d-proton build, each query
reports 32 compute invocations for the indirect clear alone when the
target shader executes zero groups. The opaque and alpha temporal
shaders still run in this test. After the burst, verify stock SR and
both native post-filters resume. Use a separate replay for timing.

## Reproduce optical RR with the opaque post-filters bypassed

Start from the opaque-cut tree above in a separate checkout. Apply the
[combined optical overlay](rr-optical-cut.patch.txt) and rebuild. The public
build supplies the `cap N` channel used by the inkVI-dive receipt helper.

```sh
test "$(git write-tree)" = 422ada24652eb1444303ac1d8c91157d66a6a61e
git apply --index "$kit/rr-optical-cut.patch.txt"
test "$(git write-tree)" = 775e0c7d5be42de4df8d63944f0f062cb9e9bf24
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
```

Use the same offline title, pinned SDK and windowed 4K profile. In a fresh
launch environment, use the capture paths and provider setup above. Replace
the five-Present trigger with the continuous window and enable the optical
layer and opaque current-frame path. Leave the experimental alpha and GI
variants off for this replay.

```sh
unset VICTORY_RR_STAGE_TRIGGER VICTORY_RR_DISPLAY_FRAME VICTORY_PASS_STATS_FRAME
unset VICTORY_RR_EXPERIMENTAL_ALPHA_KERNEL VICTORY_RR_PROBE_CURRENT
export VICTORY_RR_EARLY_SOURCE=1 VICTORY_RR_ATOMIC_CUT=1
export VICTORY_RR_REFLECTION_CURRENT_KERNELS=1 VICTORY_RR_NATIVE_TRANSMISSION_DISPLAY=1
export VICTORY_RR_PRESENT_LINEAGE=1 VICTORY_RR_PRESENT_LINEAGE_DIAGNOSTICS=1
export VICTORY_RR_PERF_TRIGGER='Z:\path\to\run\rr-optical-trigger'
```

Leave the trigger file absent at startup. Request two stock frames and wait
for their receipts when the cell is playable. From the host, create
`rr-optical-trigger` while facing the pane. After the three-second warmup,
request a new `cap 8` burst. The window stays active for 55 seconds after
warmup. A replay with capture or scripted motion only supplies quality and
provider evidence.

Match the cutoff's `sequence` and `begun` to the chosen slot's
`source_submit` and `begun`. Require feature 13, zero SR GPU evaluations,
`opaque_postfilters_cut=true`, and `native_transmission_overlay=true`.
Match the chosen submission, output serial and fence to its retirement.
Check exact DXGI attribution separately before assigning a provider to a
captured frame. Keep the PNGs and title logs local.
[Combined hardware results](ray-reconstruction.md#optical-input-and-opaque-bypass-on-the-same-frames)

## Verify reflection and diffuse-probe replacement together

From tree `775e0c7` above, add the [alpha and GI overlay](rr-reflection-gi.patch.txt).
This includes the exact native Present tracking used in the hardware test.

```sh
test "$(git write-tree)" = 775e0c7d5be42de4df8d63944f0f062cb9e9bf24
git apply --index "$kit/rr-reflection-gi.patch.txt"
test "$(git write-tree)" = 8cabc54072e7ff76e4487e33021b3f778c878bd7
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
export VICTORY_RR_EXPERIMENTAL_ALPHA_KERNEL=1 VICTORY_RR_PROBE_CURRENT=1
```

Use the optical + opaque settings above and add the two switches. For
the bounded dispatch proof, unset `VICTORY_RR_PERF_TRIGGER` and restore
the five-Present stage trigger. Set `VICTORY_RR_DISPLAY_FRAME=1`,
`VICTORY_RR_BACKGROUND_FRAME=1`, `VICTORY_RR_MATERIAL_FRAME=1`,
`VICTORY_RR_REFLECTION_CAPTURE_FRAME=1`, `VICTORY_RR_ORDER_FRAME=1`,
`VICTORY_PASS_STATS_FRAME=1` and `VICTORY_PASS_GPU_TIME=1`. Arm in the
playable cell and capture around the trigger as above.

Join the exact source and alpha list epochs to retired shader queries
and the FFX fence. In addition to the opaque counters, verify original
and current variants for alpha temporal (`34992f2b65a789a5`) and probe
temporal/caustics (`34fb51b8a0312e82`). The calibrated raw count of 32
means only the indirect clear ran. Require positive replacement work
and `paired_alpha_selected=true` and `paired_probe_selected=true` at
the chosen slot. Match the HDR output to accepted DXGI Present lineage
with `is_rr`, `early_cut`, `optical`, `paired_alpha` and `paired_probe`
all true.

For timing, run the **same binary** in two fresh isolated sessions with
the same stationary camera, seed and window size. Enable
`VICTORY_RR_PRESENT_LINEAGE=1` and use a fresh
`VICTORY_RR_PERF_TRIGGER` for each process. Clear the five-Present
trigger, the per-pass query and lineage diagnostic switches, and all
capture paths. Keep opt-in replacement switches off in the SR arm.
Arm each process only after the same playable view is verified and
collect the next 55 seconds after the three-second warmup. Report
median FPS, p95 Present interval, and RR, SR, unknown and replacement
route counts. A mixed window measures the enabled configuration
including its fallback costs. Report it as mixed.

## Check the deferred camera and diffuse-history GI replacement

Apply the [camera and GI overlay](rr-camera-stable-gi.patch.txt) on the
verified full-native source tree above.

```sh
test "$(git write-tree)" = 8cabc54072e7ff76e4487e33021b3f778c878bd7
git apply --index "$kit/rr-camera-stable-gi.patch.txt"
test "$(git write-tree)" = 55bb577c0a2fb158ecdc6edd38e2df1b10390b78
cargo build --locked --offline --profile diag --features dlss \
  -p victory --target x86_64-pc-windows-msvc
export VICTORY_RR_DEFER_CAMERA_PROJECTION=1
export VICTORY_RR_PROBE_CURRENT=1 VICTORY_RR_PROBE_DIFFUSE_HISTORY=1
```

Use the earlier five-Present request-only capture procedure. Join each
accepted RR Present to its original native counters and the retired
`probe_diffuse_history_kernel` query. The original probe kernel must
show the calibrated floor. The other replacement and optical receipts
still apply. `VICTORY_RR_LIGHT_TAPS` optionally points at an existing
private Windows directory for rail-matched GI and 720p pre-NGX HDR
samples during this bounded stage. Keep all samples and captures local.

Both probe switches select the replacement that retains the diffuse
and caustic accumulation. The older current-only experiment uses
`VICTORY_RR_PROBE_CURRENT` alone. For capture-free timing clear all
capture, tap, per-pass query, camera diagnostics and stage flags.
Retain the new switches in the RR arm, then follow the same-source SR
control and matched 55-second procedure above.
