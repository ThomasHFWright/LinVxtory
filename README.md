# LinVxtory

LinVxtory is a Linux/Proton development fork of Marvisak’s VIctory. Marvisak and
the VIctory contributors built the loader, runtime services, graphics backend
and shader translator. The renderer we test comes from fxceless/victory-fork.
Our Linux work began on Slaktarn700’s Proton fork and continued in LinVictory,
where we integrated compatibility fixes and tested them on Linux hardware.
[VIctory](https://github.com/Marvisak/VIctory) · [fxceless/victory-fork](https://github.com/fxceless/victory-fork) · [Slaktarn700/VIctory](https://github.com/Slaktarn700/VIctory)

## Our focus

Get normal world rendering and controllable gameplay working under Wine/Proton.
On our CachyOS and RTX 5090 setup, we prioritize frame rate, player interaction,
then complete models and textures. We use repeatable checkpoint runs and record
the inputs, crashes and remaining rendering defects alongside the results.

## What builds on main

| Previous LinVictory work | How to use it on `main` |
| --- | --- |
| PR995 Linux gameplay build | The default. `build.sh` reconstructs the tested renderer from upstream commit `7406afb` and applies the Linux patch, including the DXC fallback, code-table fix and paired-wave fallback. |
| PR927 facial-animation update | An optional patch on top of PR995. Follow the [PR927 build instructions](docs/pr995-candidate/README.md#optional-pr927-facial-animation-build). |
| Earlier `linux-edge` renderer and experiments | The complete workspace remains in [the last pre-migration tree](https://github.com/ThomasHFWright/LinVxtory/tree/6b1551effd073d5dd698eaa61d492daae69b55df). Individual ports and their test results are documented in the [Linux improvements](docs/linux-improvements.md) and [upstream integration notes](docs/upstream-integration.md). |

## Build the tested renderer

From this checkout, with the Rust toolchain, cached Cargo dependencies and Windows cross-compilation SDK configured, run:

```sh
sh build.sh ../linvxtory-renderer
```

The script fetches the pinned public renderer into a new directory or reuses an existing checkout at that revision. It applies the Linux patch, checks all 188 locked build inputs and builds locally. Supply your own game files at runtime. The default target path in the external checkout is `target/x86_64-pc-windows-msvc/diag/victory.exe`. [Toolchain, preset and optional PR927 patch](docs/pr995-candidate/README.md) · [Proton runtime setup](docs/linux-runtime.md) · [Indexed wineserver source kit](docs/runtime/indexed-server/README.md)

## Gameplay status

A 30-minute offline run on CachyOS with GE-Proton 11-14 and an RTX 5090 reached the cell and responded to camera and aiming controls. Rendering averaged 4.87 FPS over nearly 25 minutes in the world. Walking remains unverified. Hair, facial surfaces and lighting have visible defects. The optional PR927 source patch measured 5.19 FPS against 5.17 FPS in a shorter matched test. Facial skin and lips were still missing in the frontal view.

## Attribution

**Default Linux build and runtime**

- **tunczzz** contributed the frame-rate work underlying the tested PR995 renderer. **fxceless and the victory-fork contributors** supplied the rendering base and its Windows investigation. [PR995](https://github.com/fxceless/victory-fork/pull/995)
- **errusso** contributed the Wine/DXC compiler fallback. **ruvenesh** contributed the opt-in watchdog hook. [PR20](https://github.com/fxceless/victory-fork/pull/20) · [PR454](https://github.com/Marvisak/VIctory/pull/454)
- **desplmfao** developed the code-table trimming fix and **OctoAlgo** investigated and ported it for Proton. The LinVictory paired-wave fallback adapts the approach from upstream shader work for 32-lane GPUs. [PR1006](https://github.com/fxceless/victory-fork/pull/1006) · [PR854](https://github.com/fxceless/victory-fork/pull/854) · [PR993](https://github.com/fxceless/victory-fork/pull/993)
- **datingesp-hash** reported the mapped-view bottleneck and an indexed solution. **UwUCammie** published the patch used in our indexed wineserver adaptation. The runtime builds on **Wine, Valve and GE-Proton contributors’** work. [Runtime credits and provenance](docs/runtime/indexed-server/README.md)

**Optional patch and earlier Linux renderer work**

- **tippythetaidum** developed typed half-data preservation for the facial-animation shaders, including fixed descriptor slots, table spans and ordinary format loads consumed as halves. [PR927](https://github.com/fxceless/victory-fork/pull/927)
- **NuraadGit** contributed the constant-buffer size limit fix, **gomeaw** the cross-page buffer transfer fix and regressions, and **ryotdev** the initialized/root UAV test support. [PR24](https://github.com/fxceless/victory-fork/pull/24) · [PR846](https://github.com/fxceless/victory-fork/pull/846) · [PR33](https://github.com/fxceless/victory-fork/pull/33)
- **gmankab** bounded texture staging allocations. **UwUCammie** contributed guarded buffer write-back. [PR989](https://github.com/fxceless/victory-fork/pull/989) · [PR1032](https://github.com/fxceless/victory-fork/pull/1032)
- **AnoHunted, tedin7, TG0000, Maraco91, divyeshganta, heromike405-arch and AscilCH** contributed the audio decoder, ring-buffer and parser fixes used in the earlier Linux integration. [PR959](https://github.com/fxceless/victory-fork/pull/959) · [PR627](https://github.com/fxceless/victory-fork/pull/627)
- **fxceless** contributed indirect-command gathering and wide-load and copyback optimizations. **cxy0072019-sketch** contributed texture-to-CPU readback and its allocation-lifetime corrections. **jtrudeau1530** reported Linux memory-range and descriptor synchronization issues that informed local fixes. [Integration notes](docs/upstream-integration.md) · [PR999](https://github.com/fxceless/victory-fork/pull/999)

**Upstream acknowledgements**

The fxceless project credits **ivall and the XCL contributors** for reference
work, **myokex** for mapped-page guards building on **Spexivus**, **joroyt** and
**tightfall**, and **Joshua Odom** for the kernel-read fix. Its README also
acknowledges development assistance from Claude Code. Linux integration and
local validation in LinVictory and LinVxtory used OpenAI Codex.
[Original credits and Windows findings](https://github.com/ThomasHFWright/LinVxtory/blob/6b1551effd073d5dd698eaa61d492daae69b55df/README.md#upstream-readme--fxcelessvictory-fork)

The project retains the GPL-3.0-or-later license. The build notes document
upstream’s separate BCPack table exception. [License](LICENSE) · [Build provenance](docs/pr995-candidate/README.md#attribution-and-exact-changes)
