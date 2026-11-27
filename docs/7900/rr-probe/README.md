# D3D12 Ray Reconstruction probe

The standalone probe uses NVIDIA NGX feature 13 (Ray Reconstruction) on a
hardware NVIDIA DXGI adapter. It queries the SDK's optimal input size for
3840×2160 Ultra Performance and requires 1280×720. It creates the feature
and evaluates two synthetic frames on one D3D12 direct queue. The first
evaluation requests a history reset. The second reuses the feature without
a reset.

The probe uploads distinct noisy HDR inputs and initializes linear depth,
diffuse and specular albedo, packed normals and roughness, and separate
primary and reflection motion fields for a stationary scene. It fills the
RGBA16F output with a sentinel before each evaluation. A passing run
requires successful NGX results, completed GPU fences and no device loss.
Both GPU readbacks must contain finite, spatially varying RGB data, with
every sentinel replaced, and differ from a sampled bilinear red-channel
reference. The outputs must have different RGB checksums.

The synthetic result establishes execution for this configuration. Image
quality, temporal stability, and renderer pass placement require title
captures.

## Build

Use NVIDIA DLSS SDK [310.9.16](https://github.com/NVIDIA/DLSS/releases/tag/v310.9.16),
commit `9981c6350dc58c0ce21f024fd65e31e851c31689`. The probe builds
against its D3D12 headers and `lib/Windows_x86_64/x64/nvsdk_ngx_d.lib`.
The cross-build uses clang-cl, lld-link, and the xwin MSVC 14.44.17.14 and
Windows SDK 10.0.26100 layout.

From the repository root, build into an external directory.

```sh
bash docs/7900/rr-probe/build.sh "$DLSS_SDK" "$XWIN_SDK" "$PROBE_DIR"
```

## Run

Place the SDK's `lib/Windows_x86_64/rel/nvngx_dlssd.dll` and
`lib/Windows_x86_64/rel/nvngx_dlss.dll` beside `rr-probe.exe`. The tested
NGX loader discovered both feature DLLs. The probe creates and evaluates
only Ray Reconstruction. On Proton, use a dedicated Wine prefix with the native
NVIDIA driver NGX DLLs, working DXVK-NVAPI, the Microsoft x64 Visual C++ v14
runtime, and the tested native D3D12/DXGI overrides. Keep the offline
runtime setup and any runtime binaries outside the checkout. The
[Super Resolution runtime setup](../README.md#build-and-launch) documents
the shared dependencies. Include `nvngx_dlssd=n` in `WINEDLLOVERRIDES`.

Run from a writable private working directory for NGX logs. The probe
uses its executable directory as the feature DLL search path and needs no
title files or arguments.

```sh
cd "$PROBE_DIR"
timeout -k 5s 180s "$PROTON_WINE" ./rr-probe.exe
```

A successful run prints `PASS D3D12 Ray Reconstruction created, evaluated,
fenced, and verified on both frames`.

## Hardware result

On 2026-11-27 the probe exited successfully in an offline run on an RTX 5090
(PCI `10de:2b85`), NVIDIA 620.73.04, GE-Proton 11-14 and vkd3d-proton
3.2.0 build `e08a456a483cd8f`. NGX reported Ray Reconstruction available
and returned `0x00000001` for creation and both evaluations. The signed
310.9.16 Ray Reconstruction DLL logged RR2 resource allocation with the
1280×720 input, 3840×2160 output, and Ultra Performance mode.

| Stage | Completed fence | RGB checksum | Red samples differing from bilinear |
| --- | ---: | --- | ---: |
| Feature creation | 1 | | |
| Evaluate with reset | 2 | `f51a616a70b47013` | 9,145 of 28,928 |
| Evaluate with history | 3 | `6a52a1780a14f1df` | 9,792 of 28,928 |

Both readbacks contained zero sentinel or nonfinite RGB components. All
8,294,400 pixels in each frame had a nonzero red value. The D3D12 device
remained healthy at all three fences. The checksums identify this run.

The tested executable SHA-256 was
`af5b16357a50e3e5febd724b2e5d4869c038ee5c6993f01d9e8d9cc678cc8a35`.
The tested `nvngx_dlssd.dll` SHA-256 was
`b49435ae346f227feafe7bbe937d88277ea0ee7323975ec06488f377fdb46cee`.

### Compare the input and GPU output visually

In a fresh private working directory, run the same fixture with
`--preview`. The fixture checks the two evaluations and GPU readbacks
before exporting the final frame.

```sh
"$PROTON_WINE" "$PROBE_DIR/rr-probe.exe" --preview
```

`noisy-input-720p.ppm` contains the synthetic input after the same FP16
quantization used by the GPU upload. `ray-reconstruction-4k.ppm` comes from
the fenced output readback. Both exports use the same linear-to-sRGB
conversion and preserve their native resolutions. The export refuses to
overwrite existing files.

On the RTX 5090 run, a flat background tile provided a simple grain check.
Within input coordinates x=50…79, y=36…55, the blue channel's population
standard deviation was 8.95 in the input and 0.90 at the corresponding 4K
pixel centers (3x+1, 3y+1), in 8-bit sRGB values. This describes the 600
samples in that synthetic tile. Gameplay quality requires a separate
in-game comparison.

### Verify a transparency layer inside one RR evaluation

Run the same executable with `--transparency-test`. The four synthetic
arms use one feature-13 evaluation apiece and reset the feature history.
The scene has a known straight reference and a second reference displaced
by 23 input pixels horizontally and 13 vertically inside a pane. The
fixture verifies the FP16 base and premultiplied overlay reconstruct the
intended input color at all 921,600 input pixels before running NGX.

| Arm | Mean absolute error vs. straight | Mean absolute error vs. displaced |
| --- | ---: | ---: |
| Omit layer | 0.002062 | 0.197035 |
| Bind displaced pane | 0.197585 | 0.000062 |
| Bind straight pane at the same coverage | 0.000062 | 0.197583 |
| Omit layer again | 0.002744 | 0.196741 |

The RTX 5090 readbacks used 29,808 linear RGB samples inside the pane
for each arm. The guided output follows the pattern supplied in the
overlay. This provides a possible single-evaluation way to keep the
game’s native refraction. It still requires the original refracted
colors and a verified mask at the point the game hands the frame to RR.
