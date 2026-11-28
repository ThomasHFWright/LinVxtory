# LinVxtory

LinVxtory develops VIctory for **NVIDIA GPUs on Linux through Wine and
Proton**. It provides a source-locked renderer build, runtime setup and
repeatable gameplay checks.

## Default 4K preset

The Series S preset enables **NVIDIA DLSS Ray Reconstruction**
automatically on qualifying frames, with **1280×720** input and
**3840×2160** output in Ultra Performance mode. Ray tracing and the game’s
native global illumination (GI) run, and the renderer composites the
title’s UI at presentation.

On eligible frames, RR replaces three native opaque reflection passes.
The preset keeps the game’s alpha reflection history for steadier cell
lighting and preserves the original wavy glass. Frames that miss the
filter cutoff, including during startup, use stock Super Resolution
with the native filters.
[Cell lighting results](docs/7900/ray-reconstruction.md#cell-lighting-stability) ·
[Startup investigation](docs/7900/ray-reconstruction.md#continuous-startup-investigation)

## Build and run

Set up Rust 1.98.1, the Windows x64 MSVC cross toolchain, the locked Cargo
dependencies and NVIDIA DLSS SDK 310.9.16. Build from this checkout.

```sh
export VICTORY_DLSS_SDK=/path/to/DLSS-SDK
sh build.sh ../linvxtory-renderer
```

Use a fresh output directory for each build mode. Follow the launch
guide to set up Proton and the signed NVIDIA runtime DLLs, including
`nvngx_dlssd.dll` for Ray Reconstruction. Ordinary gameplay starts
without a diagnostic trigger. Supply your own game files and keep
the prefix, saves and caches separate from the source checkout.

To run 4K Super Resolution with the same executable, set
`VICTORY_RR_AUTO=0` in a fresh shell before loading the preset. For a
dedicated SR build use `sh build.sh --sr ../linvxtory-sr`, or add
`--control` to reproduce the exact historical SR source. The explicit
`--rr` build option still selects the default renderer.
[Toolchain and launch guide](docs/7900/README.md#build-and-launch) ·
[Historical hybrid-GI measurements](docs/7900/ray-reconstruction.md#capture-free-timing-with-the-stock-fallback-guard) ·
[Diagnostic capture guide](docs/7900/dive-testing.md) ·
[Asset policy](docs/asset-policy.md)

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
