# PR995 Linux candidate: source and reproduction kit

This kit reconstructs the source of the tested standalone candidate from
public [PR995](https://github.com/fxceless/victory-fork/pull/995), pinned at
`7406afbbd9cef0dcddef0b0a23b4855437715eb5`. The tested three-PR renderer adds the PR1722 cache fix, the PR2026 CPU detile port and the PR2398 whole-row mesh port to the frozen Linux baseline.

The final tree is `cc6e33fe99572a2d433e82a49e8429678b70ca5a`. Its 190 locked inputs reproduce the source of the executable on which mouth movement was observed. [Source verification and tested executable digest](validation.md#updated-source-verification)

## Historical PR995 Linux baseline

Before PR1722, LinVictory's RTX 5090 test created all four previously failing compute pipelines
and reached the rendered room. Two windows using the same checkpoint measured:

| Elapsed window | Presents / measured seconds | Candidate FPS | Published baseline FPS |
| --- | --- | --- | --- |
| +180..330 seconds | 766 / 142.81 | 5.364 | 0.709 |
| +330..480 seconds | 816 / 148.14 | 5.508 | 0.720 |

The run completed its 30-minute limit. The harness closed the game after
1,801.97 seconds. No NVIDIA Xid, title crash report or memory-limit kill was
observed. Over nearly 25 minutes following world load, the renderer delivered
7,276 presents in 1,492.97 seconds (4.87 FPS). The comparison build delivered
1,039 in 1,494.99 seconds (0.70 FPS).

Opposite controller inputs turned the camera from the window to the bunks and
back. Holding the mapped left trigger at the tutorial changed the view and
advanced its instruction. Releasing it restored the original prompt. Walking was unverified in that baseline run, and some lighting, hair and surfaces render incorrectly.

Historical PR995 executable SHA-256:
`c24b8578d2a8d51882f2d2c3b5b983383e85235d8a9d7d0758aa4b131d203403`.
The matching PDB digest and all 188 selected build-input digests are in
[source-lock.json](source-lock.json). Reconstruction verified every selected
input byte-for-byte, including Cargo.lock, manifests, translator build script,
and included resources. This is source equivalence, not a reproducible PE/PDB
byte claim: build paths and compiler/linker metadata can differ.

## Reconstruct and build

From the repository root, give the wrapper one external source directory.
Use a new path, a clean checkout at the pinned commit, the reconstructed Linux
baseline or the finished three-PR source. Keep this directory outside game,
cache and data folders. Configure the toolchain and cache dependencies before
building offline.

```sh
sh build.sh "../linvxtory-renderer"
```

To verify the finished source without building, run
`sh docs/pr995-candidate/verify.sh "../linvxtory-renderer"`. The wrapper
uses the original 188-input lock before applying the three-PR delta, then
checks all 190 final inputs before invoking Cargo.

The zero-context patch deliberately excludes untouched upstream text; use
`--unidiff-zero` only after checking the pinned clean base. All patch content is
source: no private fixtures, dumps, traces, captures, executable or runtime.
Do not copy the reconstructed upstream tree into LinVictory: upstream includes
a compiled helper shader and title-specific material, and two candidate source
files exceed LinVictory's existing 512-KiB policy. This kit neither imports those
files nor relaxes that policy. The public base is a required external input.

The frozen build used Rust 1.98.1, clang-cl/lld-link, xwin MSVC 14.44.17.14 and
Windows SDK 10.0.26100. Configure the MSVC linker, compiler and library paths for
your local SDK. The existing build wrapper mounted a writable copy of cached
blake3 1.8.7: every archive file was checked identical to its cached published
crate; no dependency source patch was used. The source-lock records its digest.
No active LinVictory Cargo.lock is substituted for PR995's pinned lockfile.

CPU regression command (no GPU):

```sh
cargo test -p victory-shader --lib --all-features --offline --locked
```

The historical PR995 source passed 187 native shader tests, with two ignored, including
code-table boundaries, paired-wave guards and retained host annotations. Further GPU/title validation is separate from source reconstruction. Native exact-input tests can use a private external
directory; no such inputs are distributed. The final donor and this candidate each passed all four GPU pipeline-creation checks.

## PR1722 host pipeline library

Impre-visible’s PR passes a null pointer and zero bytes to
`CreatePipelineLibrary` when starting an empty library. vkd3d-proton rejects
the non-null pointer from an empty Rust slice. PR995 keeps its existing
nonempty-blob load and locking code. Upstream had already taken the locking
change by the final PR head, `ee84a44859d4384ea5b945fa59e09ad9f9ac8d61`.
[Upstream diff](https://github.com/Marvisak/VIctory/pull/1722/files) · [Author’s scope correction](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7148430206)

PR1722 changes one build input, `crates/victory-gpu/src/pipeline/library.rs`.
Its SHA-256 is `427c8eb03d1f96a88ebee05c5beb041fbe6ef160a85603ad2dbd2a4fb723942c`.
The default lock combines the frozen lock with the eight full-file hashes for this round.
[Baseline lock](source-lock.json) · [Updated file hashes](upstream-files.json) · [Source verification](validation.md#updated-source-verification)

| Independent Linux tester | Measured result |
| --- | --- |
| axknxla, RTX 2070 SUPER, GE-Proton 11-14 | The cold run saved 27,128 bytes and 1,279 early keys. Two full-cache replays each scheduled 1,050 distinct keys and loaded 28 pipelines before exiting. The library-only replay loaded 182. Follow-up traces found a compiler fault after a library miss. [Matched runs](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7167202555) · [Crash follow-up](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7167543708) |
| ntgata1-dotcom, RX 9070 XT, Wine 11.22/RADV | First-boot menus saved 1,208 bytes and reloaded all six pipelines on the second run. [Menu comparison](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7159448409) |
| OsiPog, Wine-staging 11.21/RADV | A 4.5 MB library loaded in 9–17 ms, with about 2,400 pipelines reused on the next run. [Linux report](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7161314791) |
| necrobytes, RX 9060 XT, Fedora/Proton Experimental | The library saved 313,400 bytes and loaded 1,360 pipelines before world entry. Only one patched run completed the 40-second sampling window, leaving the FPS effect unresolved. [World comparison](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7179758064) |
| arogan178, RX 9070 XT, CachyOS/Proton Experimental | The offline shader-cache prebuild saved 4.5 MB. Its repeat reported zero made and 27,120 already present in 2.0 s. This measures prebuild reuse. [Prebuild results](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7186428452) |

On the author’s GTX 1660, the second short run loaded all 14 cached pipelines.
A separate world-scale run on another tree kept 1,717 early entries. Three
warm launches reached the world in 201, 157 and 159 seconds against 151 seconds
cold. The replicated benefit is persistence and reuse. [Author’s short runs](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7148430206) · [Author’s world runs](https://github.com/Marvisak/VIctory/pull/1722#issuecomment-7159492357)

## PR2026 CPU detiling

0xkn0wh3r3’s upstream change bounds-checks a row once before copying its
contiguous runs. The PR995 port uses the new path only for detiling swizzled
textures. Tiling and linear copies use the existing PR995 code. A regression
covers pitched rows, shorter final runs and untouched padding. [Marvisak PR2026 at `1ec3877`](https://github.com/Marvisak/VIctory/pull/2026/commits/1ec3877c526ccbb8decafeb57a2c3722b2c7ff3a)

The private CPU harness compared the frozen texture source against baseline in
one executable, checked output bytes for equality and alternated execution
order across 16 pairs per case. Each column gives the median paired speedup
for that run. Values above 1 mean faster detiling.

| Swizzled texture | First run | Repeat |
| --- | ---: | ---: |
| RGBA8 3840×2160, 64 KiB S | 1.217× | 1.185× |
| RGBA8 3840×2160, 64 KiB D | 1.316× | 1.380× |
| RGBA8 2048×2048, 256 B | 1.138× | 1.166× |
| R8 2048×2048 | 1.129× | 1.198× |
| RGBA16F 3840×2160 | 0.959× | 1.167× |
| BC1 4096×4096 | 1.378× | 1.235× |
| BC7 4096×4096 | 1.106× | 1.252× |
| RGBA32F 2048×2048 | 1.163× | 1.078× |
| RGBA8 1024×1024, 64 KiB D | 1.225× | 1.324× |

The recorded formats suite passed 26 tests with four ignored in both debug
and release. These timings measure CPU texture copies in isolation.

## PR2398 whole-row mesh linking

Texbirds’s upstream work links mesh and pixel signatures to fit D3D12’s
32,768-byte mesh output limit. This port only attempts a linked pipeline for an
over-budget mesh with a pixel shader on a mesh-capable device. It drops whole
unread user semantics, preserves packed-half locations from the original
signatures, and checks the surviving pixel reads, producer types and locations
before using the pair. A failed link or PSO creation falls back to PR995’s
standalone pipeline attempt. Depth-only linking and partial-channel trimming
are outside this port. [Marvisak PR2398 at `13ac07b`](https://github.com/Marvisak/VIctory/pull/2398/commits/13ac07b570b806fbe77434a6240b6f6d190b063e)

The local synthetic 128-thread fixture expects 34,816 bytes standalone and
6,144 bytes after dropping unread rows. The 64-thread fixture expects 17,408
bytes after tightening the standalone counts. The recorded native suite passed
189 tests with two ignored. Two Windows mesh regressions passed under GE-Proton
on the RTX 5090, including a draw that checked all 256 output texels. The
Wine WARP request selected the hardware adapter again. The linked selection
counter in the diagnostics build records the first use of each linked pipeline.

## Historical PR927 experiment

tippythetaidum’s half-view work and the associated Linux tests are recorded
with the original source patch at commit `4331d2b`. Its matched checkpoint
runs measured 5.17 FPS on the historical baseline and 5.19 FPS with PR927.
[Historical source kit](https://github.com/ThomasHFWright/LinVxtory/tree/4331d2b95ab2a56af74884c1b62a2966a35588b3/docs/pr995-candidate) · [Linux integration notes](../pr927-integration.md)

## Linux preset and runtime

[linux-preset.sh](linux-preset.sh) records the measured candidate flags without
starting anything. It requires user-chosen private prefix/data/shader/driver
cache locations and Windows paths to **DXC/DXIL 1.9.2609**. The DXC fallback
compiles the layer's own HLSL when Wine's compiler fails or drops a root
signature. A matching validator DLL is needed to sign the resulting containers.

The trial used **GE-Proton 11-14**, the separately built vkd3d-proton described
by the [Linux runtime instructions](../linux-runtime.md), and the indexed GE11-14 wineserver
source kit (Wine `a47c73f52fd7b2efafc1c560710f3a5099ae51e0`, staging
`6cc805ea57132eeaf44764e9213823c9b8d0d300`, server protocol 938).
The indexed source patch digest is
`3d2c0e5151128f204ee048343eb9847fae4739956448a72e980915e9e24f2001`;
its [public source discussion](https://github.com/fxceless/victory-fork/issues/184#issuecomment-7148271684)
and the [indexed source kit release](../runtime/indexed-server/README.md) preserve that separate provenance.
The indexed source-kit archive SHA256 is
`1df74bc99a538339de347b24633855fe1a62ffe8d18e35150ae3f7701f7269c8`.
Runtime binaries are not part of this kit. Stock GE is not claimed performance
equivalent. Use native d3d12/d3d12core/dxgi overrides and disable
VK_NV_raw_access_chains as recorded in the preset. The window bound controls
window size, not the title's render-target resolution.

Keep the title input read-only and tests offline. The test used a separate copy of
an organically generated checkpoint. This kit supplies no saves and makes no
save/cache compatibility guarantee. Keep watchdog-disabled trials externally
time-bounded. Do not inject the observation harness's timed inputs into normal
launches; no input automation is included here.

## Attribution and exact changes

The original compatibility layer is by **Marvisak and VIctory contributors**;
**fxceless/victory-fork contributors** supply the rendering base. **tunczzz's
PR995** supplies this renderer revision. The base repository retains their
history, notices and [GPL-3.0-or-later license](LICENSE); this kit keeps those
terms. The pinned upstream checkout supplies 77,982 bytes of BCPack decoder tables,
which are embedded when you build locally. Upstream identifies these format
constants as an exception outside its repository license. The source kit
contains the patches and build instructions. Supply your own game files at runtime.

- Code table: **desplmfao's** original fix, **OctoAlgo's** public
  [PR1006](https://github.com/fxceless/victory-fork/pull/1006) commit
  `4e7af7b01356417864b935bd1fffc7d0dece266f`. Computed tables start after
  instructions while fixed earlier reads remain valid; its synthetic boundary
  regression is retained.
- Waves: LinVictory `10532b20c14209b53bdb4e205698777cd53cc1b1`, adapted through
  the final four-file current-fx donor patch, SHA256
  `859da8ccba562d85967de140b1804545b044c050b1a6dc1d7a89a4e63194b105`.
  The paired-wave work follows the upstream #854/#993 approach. This is the
  narrow validated fallback, not their entire branch stack. Existing PR995
  successful retries stay first; descriptor annotations retain their full tuple
  keys. No superseded scratch port script was used.
- Linux overlay: the frozen eleven-file compatibility overlay derived from
  LinVictory `7c09c9913a1df4cd3a280f67633391387aa3c8f4`, unchanged. DXC fallback
  builds on **errusso's PR20**; the opt-in watchdog compatibility follows
  **ruvenesh's PR454**. Host-wave discovery precedes graphics translation/cache
  lookup; bounded-window settings remain opt-in.
- Native test portability: the existing Windows validator/host test module is
  gated to Windows. Synthetic CPU wave and code-table tests remain enabled.

Linux adaptation, regression tests and validation are AI-assisted with OpenAI
Codex. Exact file hashes and patch provenance are supplied so these changes can
be reviewed independently.

## Remaining differences and validation limits

This is not OctoAlgo's unpublished combined 3.54-FPS source tree. It lacks #938's
loop-latch normalization and early-pipeline-list fallback, and does not import
the full #854/#993 stack, #1002/#683/#917, ambient changes or current LinVictory host
optimizations. The frozen PR995 baseline retains the empty pipeline-library pointer behavior.
The default includes the PR1722 fix for that call. No shader-table provenance
or unsupported fixture is inferred from the four passing cases.

The default PR995 selection retains earlier successful paths for two inputs;
their group dimensions differ from all-paired lowering. Paired fallback is on
by default and can be disabled with VICTORY_WAVE_PAIRS=0. CPU checks with
VICTORY_WAVE_LOCAL=0 and VICTORY_WAVE_WIDE=0 also pass all four using paired
lowering, but those settings are not the measured room preset.

The frozen build has one inherited unused-function warning, and native tests
have four Windows-specific unused import/variable warnings. This kit preserves
source equivalence rather than changing unrelated code to suppress warnings.
The independent offline Windows build passed. Strict application Clippy with
`-D warnings` reports 34 diagnostics in this exact candidate; no lint suppression
or source cleanup was added. See [validation](validation.md). No push, publication, title launch or new GPU job was performed
while preparing this kit.
