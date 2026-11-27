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
