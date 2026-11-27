# LinVxtory

LinVxtory develops VIctory for **NVIDIA GPUs on Linux through Wine and
Proton**. It provides a source-locked renderer build, runtime setup and
repeatable gameplay checks.

## Default 4K preset

The Series S preset renders at **1280×720** and presents at **3840×2160**
using NVIDIA DLSS Super Resolution in Ultra Performance mode. It runs
with ray tracing enabled and the ambient substitute disabled. The
renderer composites the UI at presentation.

The documented setup is **CachyOS, RTX 5090, NVIDIA driver 620.73.04,
GE-Proton 11-14 and a locally built, pinned vkd3d-proton**. One offline
cell replay measured **10.77 median FPS** and **119.01 ms p95 frame time** across 585
title Present intervals in its 55-second timing window. The room lighting,
wire window, character face and ponytail were visible in that checkpoint. Later
scenes and long-session stability remain open.
[Preset and replay conditions](docs/7900/README.md#current-4k-baseline) ·
[Runtime versions](docs/7900/vkd3d-source-lock.json)

## Build and run

Set up Rust 1.98.1, the Windows x64 MSVC cross toolchain, the locked Cargo
dependencies and NVIDIA DLSS SDK 310.9.16. Build from this checkout.

```sh
export VICTORY_DLSS_SDK=/path/to/DLSS-SDK
sh build.sh ../linvxtory-renderer
```

Use a fresh output directory when upgrading. The build script verifies
the pinned renderer tree and build-input hashes. The launch guide covers
the Proton prefix, local runtime libraries and Series S launch flags.
Supply your own game files and keep the prefix, saves and caches separate
from the source checkout.
[Toolchain and launch guide](docs/7900/README.md#build-and-launch) ·
[Build-input hashes](docs/7900/build-inputs.sha256.txt) ·
[Asset policy](docs/asset-policy.md)

## Ray Reconstruction research

The separate research branch tracks opt-in DLSS Ray Reconstruction
experiments, including a D3D12 hardware probe and in-game guide
validation. A verified gameplay presentation with Ray Reconstruction
remains an open milestone.
[Research branch notes](https://github.com/TomTomAIbot/LinVxtory/blob/dlss-ray-reconstruction/docs/7900/ray-reconstruction.md) ·
[Reproduce a 4K cell test with inkVI-dive](https://github.com/TomTomAIbot/LinVxtory/blob/dlss-ray-reconstruction/docs/7900/dive-testing.md)

## Credits and development history

Marvisak and the VIctory contributors built the loader, runtime, graphics
backend and shader translator. LinVxtory builds on renderer work from
fxceless and the victory-fork contributors, Slaktarn700’s Proton work,
OctoAlgo and the inkVI contributors’ face and hair fixes, and ongoing
work from the Wine and Proton communities.
[VIctory](https://github.com/Marvisak/VIctory) ·
[victory-fork](https://github.com/fxceless/victory-fork) ·
[Slaktarn700’s Proton fork](https://github.com/Slaktarn700/VIctory) ·
[inkVI](https://github.com/OctoAlgo/inkVI)

The development archive preserves earlier measurements, exact source
locks, patch provenance and detailed contributor credits.
[Linux development history](docs/linux-improvements.md) ·
[Validation archive](docs/pr995-candidate/validation.md) ·
[Source lock](docs/pr995-candidate/source-lock.json) ·
[Detailed contributor credits](https://github.com/TomTomAIbot/LinVxtory/blob/338d80e1fe7fee997c7545688afe8d9113e4ae74/README.md#attribution) ·
[Runtime credits and source lock](docs/runtime/indexed-server/README.md)

The code is licensed under GPL-3.0-or-later. The asset policy documents
upstream’s separate BCPack table exception.
[License](LICENSE) · [BCPack exception](docs/asset-policy.md#the-one-exception)
