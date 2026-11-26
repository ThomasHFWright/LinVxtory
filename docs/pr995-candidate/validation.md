# Historical PR995 source verification

The published patch is intended for public PR995 commit
`7406afbbd9cef0dcddef0b0a23b4855437715eb5`, not for the surrounding LinVictory
renderer. Reconstructing in an isolated directory verified all 188 selected
build inputs byte-for-byte against the frozen candidate. Its executable digest
also matched the executable used in LinVictory's room test.

| Check | Result |
| --- | --- |
| Clean pinned base + zero-context patch | Applied without rejects |
| Build inputs, manifests, lockfile, included resources | All 188 hashes matched |
| Independent offline Windows diagnostics build | PASS; one inherited unused-function warning |
| Independent native all-feature shader suite | 187 passed, 2 ignored |
| Strict application Clippy, Windows target, `-D warnings` | FAIL: 34 diagnostics in the preserved source |
| Unchanged LinVictory asset policy on delivery branch | PASS |
| Linux preset numeric switches vs measured configuration | All nine matched, including indirect batch 3/window 0 |
| Private paths, upstream title-specific identifiers, dumps/traces in added kit | None included |

The Clippy diagnostics are in existing GPU source: 20 in host, three each in
brighten/watch, two each in draw/shadow, and one each in indirect/inplace/queue/
memline. They include needless borrows, modulo style, argument count, enum size,
and an unused function. No lint suppression or production edit was added to
make the frozen source appear clean. A passing build is not a clean Clippy claim.

The delivery branch retains its existing production sources, Cargo files,
license and policy. It adds only this source kit. The public upstream base
contains files that do not meet LinVictory's current import rules; this kit
neither republishes them nor bypasses the policy. The source manifest records
hashes of required inputs, not their contents.

The frozen candidate separately passed three offline CLI smoke checks. The
source-kit preparation ran no title or GPU jobs. The [run report](README.md) records the four real GPU pipeline checks,
the completed 30-minute checkpoint trial and the accepted tutorial input. Raw logs, screenshots, checkpoints, caches and runtime binaries are
excluded. All validation here concerns this exact candidate and kit, not
OctoAlgo's unpublished combined renderer.

## Updated source verification

The public pin is `7406afbbd9cef0dcddef0b0a23b4855437715eb5`. All rebuild
and upgrade paths must reach Git tree `0c158597c6af5254c2d29422bda9ddd9b6b34065`.
The final lock contains 190 full-file digests when the 11 final overrides are
combined with the 188-input Linux baseline. [Original lock](source-lock.json) · [Final overrides](final-files.json)

| Layer | Source tree after applying | Patch SHA-256 |
| --- | --- | --- |
| Published three-PR checkpoint `91b6968` | `cc6e33fe99572a2d433e82a49e8429678b70ca5a` | `c47ca445f08f13a27d4b9b9a98b4d4e0d22ed510613ca4ad1bd50836c48b28c2` |
| Five-file follow-up | `0c158597c6af5254c2d29422bda9ddd9b6b34065` | `fb1113a16121e61d9ec59e3cbcf5f01b05b01f684935c453417fe4616cc596ac` |

The follow-up changes `crates/victory-gpu/src/descriptor/host.rs`, `shadow.rs`
and `texture.rs` in the GPU crate, and `lower.rs` and `mod.rs` in
`crates/victory-shader/src/emit/`. The frozen GPU texture source has SHA-256
`97ea8719d2cd44b38cc571acb97ce86df691df185bb0c5b9834a56750390d3eb`.
[Five-file patch](next.patch.txt) · [Port scope and CPU measurements](README.md#changes-since-91b6968)

From a fresh destination or clean public pin, the wrapper reconstructs and
verifies the Linux baseline and the published checkpoint in sequence. A
verified existing checkpoint needs only the follow-up patch. A finished
checkout keeps its source intact. Extra arguments and edited build inputs
were rejected before Cargo in the packaging checks.

### Published checkpoint provenance

The controller-tested `three-pr.exe` from checkpoint `91b6968` has SHA-256
`1770ac0ec70041aebb00937618af696ee3e16d0d0efeb6e39987952d311516a5`.
The original checkpoint patch and eight-entry overrides are retained
byte-for-byte. [Checkpoint patch](upstream.patch.txt) · [Checkpoint overrides](upstream-files.json)

| Port | Changed paths under `crates/` |
| --- | --- |
| PR1722 | `victory-gpu/src/pipeline/library.rs` |
| PR2026 | `victory-formats/src/texture.rs` |
| PR2398 | `victory-gpu/src/pipeline/make.rs`, `victory-shader/src/emit/container.rs`, `lower.rs`, `mod.rs`, and new `linked.rs` and `linked_tests.rs` in the same emit directory |

## Published checkpoint hardware and gameplay

Four host-library tests and both linked-mesh tests passed on the RTX 5090
under GE-Proton 11-14. The formats suite passed 26 tests with four ignored.
The full native shader suite passed 189 tests with two ignored.

In 390-second offline checkpoint runs, the PR995 baseline refused to open an
empty D3D12 library. The PR1722-only build saved a 372,344-byte library and
2,111 distinct early keys. A fresh run seeded with just those pipeline files
loaded the library and began preparing pipelines before first use. Shader
and driver caches started empty in all three runs.

| Build | World +180 to +300 seconds |
| --- | ---: |
| Historical baseline, cold | 3.76 FPS |
| PR1722 only, cold | 3.77 FPS |
| PR1722 only, seeded host library | 4.06 FPS |

Each figure uses complete groups of frame presents in the same elapsed-time
window after the first world pipeline bind. One replay per condition establishes
cache reuse. The FPS difference would need repeated runs to establish a speedup.

The combined renderer subsequently reached the cell with a real controller
connected in XInput slot 0. The player turned the camera in both directions,
walked and spoke to an NPC, and reported mouth movement. This cold run used
only four scripted Enter presses at startup, and the live view was mirrored
to a second monitor. At world seconds +180 to +330 it delivered 453 presents
in 144.70 seconds (3.13 FPS). The earlier scripted-camera runs used a different
workload. No linked mesh pipeline was selected in this session. Room captures
still show missing facial surfaces on both the player character and cellmate.
The harness closed the title at the player's request after 489 seconds.

## Current build on the RTX 5090

The Windows diagnostics build completed. The native shader suite passed 190 tests
with two ignored, and the formats suite passed 26 with four ignored. Under
GE-Proton 11-14, all 12 focused GPU-crate tests and three shader tests passed.
They cover sparse first reads, preserving a previously uploaded texture, queue
selection order, concurrent worker batches, pipeline libraries and linked mesh
pixel output. The locally tested executable had SHA-256
`931892229fe4c56dc29475e6a2de4787b1e8b54187c9215a351f0e22d3172846`.

Both builds ran from the same checkpoint with cold caches, in a 1600×900 window
on CachyOS with an RTX 5090 and NVIDIA 620.73.04. The harness sent four Enter
presses at startup. It requested a synchronous dump of rendering buffers at
frame 1820 in both runs, then stopped each at the scheduled time.

| Seconds after first world bind | Published `91b6968` | Current build |
| --- | ---: | ---: |
| 0–90 | 1.20 FPS | 1.11 FPS |
| 90–180 | 2.87 FPS | 2.00 FPS |
| 180–300 | 2.94 FPS | 2.95 FPS |
| 180–330 | 3.11 FPS | 3.15 FPS |

The final interval contains 443 presents in 142.638 seconds for the control
and 454 in 144.350 seconds for the current build. Each column represents one
run. The new path recovered initial uploads for BC7 sRGB and BC5 textures.
The six-target character pass at frame 1820 still has a cheek gap in base
color, normals and depth. The bunk is visible through it. The control faces
the yard and the current build faces the bunks, which prevents a matched
hair comparison. The first eight recovered uploads included five BC1, one BC5
and two BC7 sRGB textures. Their material identities are unknown.
