# PR995 Linux candidate: source and reproduction kit

This kit reconstructs the source of the tested standalone candidate from
public [PR995](https://github.com/fxceless/victory-fork/pull/995), pinned at
`7406afbbd9cef0dcddef0b0a23b4855437715eb5`. It is a separate renderer candidate,
not an incremental renderer update to this branch. The surrounding branch
retains current LinVictory code, Cargo inputs and asset policy unchanged.

LinVictory's RTX 5090 test created all four previously failing compute pipelines
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
advanced its instruction. Releasing it restored the original prompt. Walking
remains unverified, and some lighting, hair and surfaces render incorrectly.

Frozen executable SHA256:
`c24b8578d2a8d51882f2d2c3b5b983383e85235d8a9d7d0758aa4b131d203403`.
The matching PDB digest and all 188 selected build-input digests are in
[source-lock.json](source-lock.json). Reconstruction verified every selected
input byte-for-byte, including Cargo.lock, manifests, translator build script,
and included resources. This is source equivalence, not a reproducible PE/PDB
byte claim: build paths and compiler/linker metadata can differ.

## Reconstruct and build

Use a fresh checkout of the public repository at the pinned commit, outside
this repository and all game/cache/data folders. Obtain dependencies before
entering the offline build environment; this kit does not fetch anything.
With the checkout already at the exact commit:

```sh
kit=/absolute/path/to/this/kit
candidate=/absolute/path/to/fresh/public-pr995-checkout

test "$(git -C "$candidate" rev-parse HEAD)" = 7406afbbd9cef0dcddef0b0a23b4855437715eb5
test -z "$(git -C "$candidate" status --porcelain)"
git -C "$candidate" apply --check --unidiff-zero "$kit/candidate.patch.txt"
git -C "$candidate" apply --unidiff-zero "$kit/candidate.patch.txt"
sh "$kit/verify.sh" "$candidate"
cd "$candidate"
export CARGO_TARGET_DIR=/absolute/path/to/private-pr995-target
export CARGO_BUILD_JOBS=2
cargo build -p victory --profile diag --features diagnostics \
  --target x86_64-pc-windows-msvc --offline --locked
```

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

The frozen source passed 187 native shader tests, with two ignored, including
code-table boundaries, paired-wave guards and retained host annotations. Further GPU/title validation is separate from source reconstruction. Native exact-input tests can use a private external
directory; no such inputs are distributed. The final donor and this candidate each passed all four GPU pipeline-creation checks.

## Optional PR927 facial-animation build

To build the PR995 renderer with the half-precision buffer fix, first reconstruct
the frozen source and run `verify.sh` above. Then apply this additional patch
from the same kit and rebuild with the same diagnostics features and preset.

```sh
git -C "$candidate" apply --check --unidiff-zero "$kit/pr927.patch.txt"
git -C "$candidate" apply --unidiff-zero "$kit/pr927.patch.txt"
(cd "$candidate" && cargo build -p victory --profile diag --features diagnostics \
  --target x86_64-pc-windows-msvc --offline --locked)
```

The patch ports tippythetaidum’s half-view changes and the Linux fork’s
mixed-access and descriptor-cache safeguards. On the RTX 5090, the integer
path preserved all 65,536 possible 16-bit patterns in GPU readback. The
float-conversion control preserved one. All 12 targeted Windows regressions
passed, and the native shader suite passed 193 tests with two ignored.
[Original PR 927](https://github.com/fxceless/victory-fork/pull/927) ·
[Linux integration notes](../pr927-integration.md)

In matched offline checkpoint runs on the same RTX 5090, the original build
averaged 5.17 FPS and the PR927 build averaged 5.19 FPS over seconds 180–330
after the first world pipeline bind. Both runs started with empty shader and
driver caches and ended at the 390-second harness limit. The PR927 run cached
24 compute translations that selected half views. The cell still showed exposed
eyes and detached jaw geometry in frontal views. Lip motion during speech
remains visually unverified.

## Linux preset and runtime

[linux-preset.sh](linux-preset.sh) records the measured candidate flags without
starting anything. It requires user-chosen private prefix/data/shader/driver
cache locations and Windows paths to **DXC/DXIL 1.9.2609**. The DXC fallback
compiles the layer's own HLSL when Wine's compiler fails or drops a root
signature. A matching validator DLL is needed to sign the resulting containers.

The trial used **GE-Proton 11-14**, the separately built vkd3d-proton described
by the [Linux runtime instructions](https://github.com/ThomasHFWright/LinVictory/blob/c53c8e7764fa6779abf5d6e8a876b355b3f5e13e/docs/linux-runtime.md), and the indexed GE11-14 wineserver
source kit (Wine `a47c73f52fd7b2efafc1c560710f3a5099ae51e0`, staging
`6cc805ea57132eeaf44764e9213823c9b8d0d300`, server protocol 938).
The indexed source patch digest is
`3d2c0e5151128f204ee048343eb9847fae4739956448a72e980915e9e24f2001`;
its [public source discussion](https://github.com/fxceless/victory-fork/issues/184#issuecomment-7148271684)
and the [indexed source kit release](https://github.com/ThomasHFWright/LinVictory/releases/tag/linux-edge-10532b2) preserve that separate provenance.
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
terms. The upstream documented BCPack-format-table exception is not relicensed
or redistributed here.

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
optimizations. PR995's empty pipeline-library pointer behavior remains; cache
persistence parity with current LinVictory is unverified. No shader-table provenance
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
