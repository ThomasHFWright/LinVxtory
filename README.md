# LinVxtory

LinVxtory is a Linux/Proton fork of [Marvisak/VIctory](https://github.com/Marvisak/VIctory). The default build uses tunczzz’s PR995 renderer from [fxceless/victory-fork](https://github.com/fxceless/victory-fork/pull/995), with Linux DXC compatibility, code-table correction and a wave32 fallback. Earlier Slaktarn700 and LinVictory work is preserved in the Git history.

## Build the tested renderer

From this checkout, with the Rust toolchain, cached Cargo dependencies and Windows cross-compilation SDK configured, run:

```sh
sh build.sh ../linvxtory-renderer
```

The script fetches the pinned public renderer into a new directory or reuses an existing checkout at that revision. It applies the Linux patch, checks all 188 locked build inputs and builds locally. Supply your own game files at runtime. The default target path in the external checkout is `target/x86_64-pc-windows-msvc/diag/victory.exe`. [Toolchain, preset and optional PR927 patch](docs/pr995-candidate/README.md) · [Proton runtime setup](docs/linux-runtime.md) · [Indexed wineserver source kit](docs/runtime/indexed-server/README.md)

## Gameplay status

A 30-minute offline run on CachyOS with GE-Proton 11-14 and an RTX 5090 reached the cell and responded to camera and aiming controls. Rendering averaged 4.87 FPS over nearly 25 minutes in the world. Walking remains unverified. Hair, facial surfaces and lighting have visible defects. The optional PR927 source patch measured 5.19 FPS against 5.17 FPS in a shorter matched test. Facial skin and lips were still missing in the frontal view.
