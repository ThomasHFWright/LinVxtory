# LinVxtory

LinVxtory is a Linux/Proton development fork of Marvisak’s VIctory. Marvisak and
the VIctory contributors built the loader, runtime services, graphics backend
and shader translator. The renderer we test comes from fxceless/victory-fork.
Our Linux work began on Slaktarn700’s Proton fork and continued in LinVictory,
where we integrated compatibility fixes and tested them on Linux hardware.
[VIctory](https://github.com/Marvisak/VIctory) · [fxceless/victory-fork](https://github.com/fxceless/victory-fork) · [Slaktarn700/VIctory](https://github.com/Slaktarn700/VIctory)

## Current status

**8.89 median FPS on `main`** in the warm offline Series S cell replay on
CachyOS and RTX 5090. The preset uses **1080p with ray tracing active** and
the ambient substitute off. Facial skin, the ponytail, the wire window and
room lighting were visible in both arms. The **20 FPS** target remains open.
[Current replay and build](docs/7900/README.md#findings)

**9.36 median FPS** is the highest comparable result, from one earlier warm
ray-traced control using the same timed cell window. The published preset
uses the capture improvement that held up in two matched comparisons.
[Best recorded run and comparison details](docs/7900/README.md#earlier-ray-traced-measurements)

### Biggest measured improvements

- **GPU readback capture.** The current build started at 8.14 FPS with
  capture off. Four 8 MiB slots reduced copyback from **19.35 to 16.70 ms
  per frame** and fetch wait from **10.30 to 8.80 ms**. [Capture results](docs/7900/README.md#findings)
- **More outstanding command lists.** Increasing the limit from 256 to
  512 brought median limit waits to zero in two comparisons. The
  earlier mesh-on run improved from **7.75 to 8.95 FPS**. [Queue limit results](docs/7900/README.md#earlier-ray-traced-measurements)
- **CPU/GPU write merging.** A larger exact merge budget reduced waits
  on overlapping writes from **13 to 6 ms per frame** in the earlier
  ray-tracing preview. [Memory synchronization test](docs/inkvi/README.md#memory-synchronization-test)

## Our focus

Get normal world rendering and controllable gameplay working under Wine/Proton.
On our CachyOS and RTX 5090 setup, we prioritize frame rate, player interaction,
then complete models and textures. We use repeatable checkpoint runs and record
the inputs, crashes and remaining rendering defects alongside the results.

## What this kit builds

The build uses the pinned 7900XTX renderer with LinVxtory's Linux changes,
the inkVI-derived face and hair fixes, and the tested 8 MiB GPU readback
capture. The current preset runs the Series S profile at 1080p with ray
tracing active. [Build, launch and source credits](docs/7900/README.md)

## Build the renderer

With the Rust toolchain, cached Cargo dependencies and MSVC SDK configured,
run this from the LinVxtory checkout.

```sh
sh build.sh "../linvxtory-rt-renderer"
```

Use a new output path when upgrading. The wrapper fetches the pinned
renderer, applies the Linux overlay and builds the same diagnostic
configuration used in the replay. Supply your own game files at runtime.
[Launch instructions](docs/7900/README.md#build-and-launch) · [Proton runtime setup](docs/linux-runtime.md)

## Results history

- **Ray-tracing bring-up and preview.** An early RTX 5090 build peaked at **1.3 FPS**
  after entering the world and then stalled. The first stable warm preview reached
  **5.1 median FPS** in the cell with traversal active. Holding traversal reached
  7.3 median FPS with the wall and shelving nearly black. [Earlier ray-tracing
  test](docs/inkvi/README.md#ray-tracing-preview)

- **inkVI Series S with the ambient substitute.** On RTX 5090, the build reached the cell
  with individual hair strands, the ponytail, the lit wall and the window mesh. Facial skin
  rendered in the early close-up. One NPC temporarily had a black skin texture while
  streaming. Cell samples averaged **9.03 FPS** with a median of **9.8 FPS**. [Source and
  test notes](docs/inkvi/README.md#rtx-5090-observations)

- **Earlier Series S checkpoint.** The previous Series S renderer reached **7.11 FPS** in
  the cell in a fixed checkpoint replay. [Previous Series S
  benchmark](docs/pr995-candidate/validation.md#series-s-checkpoint-benchmark)

- **Linux descriptor slot caching.** On RTX 5090, two cold room replays with this preset
  delivered **3.89 and 4.07 FPS** against **3.57 FPS** with caching off on the same
  executable. The wall, window mesh and yard posts were visible in the sampled frames.
  [Repeated cache measurements](docs/pr995-candidate/validation.md#descriptor-slot-cache)

- **Series X endurance.** On CachyOS with an RTX 5090, the earlier Series X build ran for 30
  minutes and stopped at the scheduled limit. With descriptor slot caching off, it averaged
  **4.24 FPS** over the last 25.9 measured minutes. In the fixed world +180 to +330 second
  interval, it delivered **3.57 FPS**, versus **3.15 FPS** for `f0ca006` in one replay per
  build. The player turned the camera later in the long run. The frontal view still showed
  exposed eyes and teeth with missing facial skin. [Endurance
  results](docs/pr995-candidate/validation.md#performance-build-on-the-rtx-5090)

- **First texture recovery.** The `f0ca006` build reached the cell on our RTX 5090. In one
  cold, windowed comparison it delivered 3.15 FPS against 3.11 FPS for the previous build
  over world seconds +180 to +330. It was slower earlier in loading. The first-read fix
  recovered real texture uploads. The character’s cheek still has a gap before lighting. The
  two camera angles differed, so hair quality could not be compared directly. [Checkpoint
  results](docs/pr995-candidate/validation.md#texture-recovery-build-on-the-rtx-5090)

- **Controller and dialogue on checkpoint 91b6968.** On CachyOS and GE-Proton 11-14 with an
  RTX 5090, a player used an Xbox Wireless Controller to turn the camera in both directions,
  walk through the cell, and start a conversation with an NPC. The player saw mouth movement
  during the conversation. This run used four automated Enter presses during startup. All
  camera movement and interaction in the room came from the controller. Hair and facial skin
  still have rendering defects.

  The interactive run delivered 453 presents in 144.70 seconds (3.13 FPS) in the world +180
  to +330 second window. The earlier 30-minute PR995 run averaged 4.87 FPS with a different
  camera and workload. These results do not establish a change in gameplay FPS. The
  repeatable gains were faster CPU texture detiling and reuse of the pipeline library on a
  second launch. [Measurements and hardware tests](docs/pr995-candidate/validation.md)

## Attribution

**Default Linux build and runtime**

- **OctoAlgo and the inkVI contributors** published the verified face and
  hair stack and identified the cold first-draw pipeline wait. Its patch
  history credits the original PR authors. [inkVI patch credits](https://github.com/OctoAlgo/inkVI/blob/7031c8259f4c8a906d06c1e3f4726d455126bfca/patches/README.md)
- **sagouindupm** contributed the second presentation plane and the
  bilinear upscale fallback used for Series S. LinVxtory adapts them to
  its present queue and retains the texture resources through GPU
  execution. [fxceless PR188](https://github.com/fxceless/victory-fork/pull/188)
- **kripotker** contributed placed-view deduplication and shape lookup.
  **Amine El Mahouti**, with co-author **Claude Opus 5.5**, reduced shader
  translation work. **AgonisingBasher** corrected the bounds of embedded
  shader scans. **avatarmanz and the VIctory-7900XTX contributors** provided the
  performance settings tested here. [VIctory-7900XTX changes](docs/pr995-candidate/README.md#series-s-and-7900xtx-changes)

- **UwUCammie** contributed coalesced memory protection. **Ingenioeren**, with co-author **Claude Opus 5.5**, contributed the vector bitfield shader optimization. [fxceless PR1059](https://github.com/fxceless/victory-fork/pull/1059) · [Marvisak PR2954](https://github.com/Marvisak/VIctory/pull/2954)
- **tunczzz** contributed the frame-rate work underlying the tested PR995 renderer. **fxceless and the victory-fork contributors** supplied the rendering base and its Windows investigation. [PR995](https://github.com/fxceless/victory-fork/pull/995)
- **Impre-visible** contributed the empty D3D12 pipeline-library fix under Wine. Independent Linux testers verified cache creation and reuse on NVIDIA and AMD. [Marvisak PR1722](https://github.com/Marvisak/VIctory/pull/1722) · [Tester details](docs/pr995-candidate/README.md#pr1722-host-pipeline-library)
- **0xkn0wh3r3** developed the row-bounds optimization for texture copying. The PR995 port uses it for CPU detiling of swizzled textures. [PR2026](https://github.com/Marvisak/VIctory/pull/2026)
- **Texbirds** developed over-budget mesh/pixel linking. The PR995 port selectively drops unread whole semantics on colored mesh pipelines. [PR2398](https://github.com/Marvisak/VIctory/pull/2398)
- **UwUCammie** contributed the copy-worker completion change. **Lkxex** contributed the bounded queue selection used here. [Copy workers, PR1172](https://github.com/fxceless/victory-fork/pull/1172) · [Texture queue, PR1031](https://github.com/fxceless/victory-fork/pull/1031)
- **Danigamer05** contributed the compute-only loop-limit option. **HLVM04** identified the failed first texture read on an RTX 5070 Ti and tested sparse recovery at the G-buffer stage. [Compute limit, PR683](https://github.com/fxceless/victory-fork/pull/683) · [First-read report, issue 923](https://github.com/fxceless/victory-fork/issues/923#issuecomment-7185648172)
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
