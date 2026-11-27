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

## Reproduce the experimental in-title RR test

Start from another fresh build of the 4K control. Apply the following
overlays in order. The last overlay includes its own inkVI-dive capture
adapter. The resulting diagnostics renderer still presents SR and can run a
separate feature-13 evaluation on captured title inputs.

```sh
kit=$(realpath docs/7900)
sh build.sh "$RR_TEST_SRC"
cd "$RR_TEST_SRC"
test "$(git write-tree)" = 3d033cbd633757d2fd15f97a4323d566e8a043ba
for patch in rr-backend rr-stage1 rr-cell-test; do
  git apply --index "$kit/$patch.patch.txt" || exit 1
done
test "$(git write-tree)" = dbe076f814f66e1ddad92c7e2e924e0f0eca92bf
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
