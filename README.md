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

## What this kit builds

The kit reconstructs the source of the tested `three-pr.exe`. It combines the
PR995 Linux renderer with the PR1722 Wine pipeline-library fix, PR2026 CPU
detiling and PR2398 whole-row mesh/pixel linking. [Source and measurements](docs/pr995-candidate/README.md)

## Build the renderer

From this checkout, with the Rust toolchain, cached Cargo dependencies and Windows cross-compilation SDK configured, run:

```sh
sh build.sh "../linvxtory-renderer"
```

The wrapper fetches the pinned public source for a new directory or reuses an
existing pinned checkout. It accepts the reconstructed Linux baseline and the
finished three-PR tree. It checks all 190 build inputs before invoking Cargo.
Supply your own game files at runtime. The default target path in the external checkout is `target/x86_64-pc-windows-msvc/diag/victory.exe`. [Toolchain and preset](docs/pr995-candidate/README.md) · [Proton runtime setup](docs/linux-runtime.md) · [Indexed wineserver source kit](docs/runtime/indexed-server/README.md)

## Gameplay status

On CachyOS and GE-Proton 11-14 with an RTX 5090, a player used an Xbox Wireless Controller to turn the camera in both directions, walk through the cell, and start a conversation with an NPC. The player saw mouth movement during the conversation. This run used four automated Enter presses during startup. All camera movement and interaction in the room came from the controller. Hair and facial skin still have rendering defects.

The interactive run delivered 453 presents in 144.70 seconds (3.13 FPS) in the world +180 to +330 second window. The earlier 30-minute PR995 run averaged 4.87 FPS with a different camera and workload. These results do not establish a change in gameplay FPS. The repeatable gains were faster CPU texture detiling and reuse of the pipeline library on a second launch. [Measurements and hardware tests](docs/pr995-candidate/validation.md)

## Attribution

**Default Linux build and runtime**

- **tunczzz** contributed the frame-rate work underlying the tested PR995 renderer. **fxceless and the victory-fork contributors** supplied the rendering base and its Windows investigation. [PR995](https://github.com/fxceless/victory-fork/pull/995)
- **Impre-visible** contributed the empty D3D12 pipeline-library fix under Wine. Independent Linux testers verified cache creation and reuse on NVIDIA and AMD. [Marvisak PR1722](https://github.com/Marvisak/VIctory/pull/1722) · [Tester details](docs/pr995-candidate/README.md#pr1722-host-pipeline-library)
- **0xkn0wh3r3** developed the row-bounds optimization for texture copying. The PR995 port uses it for CPU detiling of swizzled textures. [PR2026](https://github.com/Marvisak/VIctory/pull/2026)
- **Texbirds** developed over-budget mesh/pixel linking. The PR995 port selectively drops unread whole semantics on colored mesh pipelines. [PR2398](https://github.com/Marvisak/VIctory/pull/2398)
- **errusso** contributed the Wine/DXC compiler fallback. **ruvenesh** contributed the opt-in watchdog hook. [PR20](https://github.com/fxceless/victory-fork/pull/20) · [PR454](https://github.com/Marvisak/VIctory/pull/454)
- **desplmfao** developed the code-table trimming fix and **OctoAlgo** investigated and ported it for Proton. The LinVictory paired-wave fallback adapts the approach from upstream shader work for 32-lane GPUs. [PR1006](https://github.com/fxceless/victory-fork/pull/1006) · [PR854](https://github.com/fxceless/victory-fork/pull/854) · [PR993](https://github.com/fxceless/victory-fork/pull/993)
- **datingesp-hash** reported the mapped-view bottleneck and an indexed solution. **UwUCammie** published the patch used in our indexed wineserver adaptation. The runtime builds on **Wine, Valve and GE-Proton contributors’** work. [Runtime credits and provenance](docs/runtime/indexed-server/README.md)

**Earlier Linux renderer work**

- **tippythetaidum** developed typed half-data preservation for the facial-animation shaders, including fixed descriptor slots, table spans and ordinary format loads consumed as halves. [PR927](https://github.com/fxceless/victory-fork/pull/927) · [Historical source kit](https://github.com/ThomasHFWright/LinVxtory/tree/4331d2b95ab2a56af74884c1b62a2966a35588b3/docs/pr995-candidate)
- **NuraadGit** contributed the constant-buffer size limit fix, **gomeaw** the cross-page buffer transfer fix and regressions, and **ryotdev** the initialized/root UAV test support. [PR24](https://github.com/fxceless/victory-fork/pull/24) · [PR846](https://github.com/fxceless/victory-fork/pull/846) · [PR33](https://github.com/fxceless/victory-fork/pull/33)
- **gmankab** bounded texture staging allocations. **UwUCammie** contributed guarded buffer write-back. [PR989](https://github.com/fxceless/victory-fork/pull/989) · [PR1032](https://github.com/fxceless/victory-fork/pull/1032)
- **AnoHunted, tedin7, TG0000, Maraco91, divyeshganta, heromike405-arch and AscilCH** contributed the audio decoder, ring-buffer and parser fixes used in the earlier Linux integration. [PR959](https://github.com/fxceless/victory-fork/pull/959) · [PR627](https://github.com/fxceless/victory-fork/pull/627)
- **fxceless** contributed indirect-command gathering and wide-load and copyback optimizations. **cxy0072019-sketch** contributed texture-to-CPU readback and its allocation-lifetime corrections. **jtrudeau1530** reported Linux memory-range and descriptor synchronization issues that informed local fixes. [Integration notes](docs/upstream-integration.md) · [PR999](https://github.com/fxceless/victory-fork/pull/999)

The earlier `linux-edge` workspace and Linux integration history are preserved in the [pre-migration tree](https://github.com/ThomasHFWright/LinVxtory/tree/6b1551effd073d5dd698eaa61d492daae69b55df) and [Linux improvements](docs/linux-improvements.md).

**Upstream acknowledgements**

The fxceless project credits **ivall and the XCL contributors** for reference
work, **myokex** for mapped-page guards building on **Spexivus**, **joroyt** and
**tightfall**, and **Joshua Odom** for the kernel-read fix. Its README also
acknowledges development assistance from Claude Code. Linux integration and
local validation in LinVictory and LinVxtory used OpenAI Codex.
[Original credits and Windows findings](https://github.com/ThomasHFWright/LinVxtory/blob/6b1551effd073d5dd698eaa61d492daae69b55df/README.md#upstream-readme--fxcelessvictory-fork)

The project retains the GPL-3.0-or-later license. The build notes document
upstream’s separate BCPack table exception. [License](LICENSE) · [Build provenance](docs/pr995-candidate/README.md#attribution-and-exact-changes)
