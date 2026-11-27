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
