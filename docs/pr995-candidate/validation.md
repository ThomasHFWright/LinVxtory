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
and upgrade paths must reach Git tree `f150c5e8a19110b27362d0517e8c68441db5e353`.
The final lock contains 192 full-file digests when the 20 final overrides are
combined with the 188-input Linux baseline. [Original lock](source-lock.json) · [Final overrides](raw2885-files.json)

| Layer | Source tree after applying | Patch SHA-256 |
| --- | --- | --- |
| Published three-PR checkpoint `91b6968` | `cc6e33fe99572a2d433e82a49e8429678b70ca5a` | `c47ca445f08f13a27d4b9b9a98b4d4e0d22ed510613ca4ad1bd50836c48b28c2` |
| Five-file follow-up `f0ca006` | `0c158597c6af5254c2d29422bda9ddd9b6b34065` | `fb1113a16121e61d9ec59e3cbcf5f01b05b01f684935c453417fe4616cc596ac` |
| Three-file performance follow-up | `9087a7f7796671ddec0f176aa5931be73c514624` | `2bd39cdda0174091632022ad0e075510beaa91014fa397cd6d12932fba025d36` |
| [Overlapping buffer views](raw2885.patch.txt) | `f150c5e8a19110b27362d0517e8c68441db5e353` | `15570181685c20fb7d4f475cb940d7403c1fa989bdbb5ac48ee896c7786de67c` |

The five-file patch changes `crates/victory-gpu/src/descriptor/host.rs`, `shadow.rs`
and `texture.rs` in the GPU crate, and `lower.rs` and `mod.rs` in
`crates/victory-shader/src/emit/`. The frozen GPU texture source has SHA-256
`97ea8719d2cd44b38cc571acb97ce86df691df185bb0c5b9834a56750390d3eb`.
[Five-file patch](next.patch.txt) · [Port scope and CPU measurements](README.md#changes-since-91b6968)

From a fresh destination or clean public pin, the wrapper reconstructs and
verifies the Linux baseline and the published checkpoint in sequence. Each verified existing checkpoint receives the remaining layers. A finished
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

## Texture recovery build on the RTX 5090

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


## Performance build on the RTX 5090

The native shader suite passed 190 tests with two ignored. The Windows diagnostics
build completed, and GE-Proton 11-14 passed 13 focused GPU-crate tests and four
shader tests. These include the 9,216-case protection fallback test and hardware
readback of signed and unsigned bitfield extraction at zero width, wrapped
offsets and the sign boundary. The locally tested executable had SHA-256
`7a396475bad10d71aea95c9046567d6625491e835da9cc46000c1103e0b49bae`.
[Source patch](followup.patch.txt) · [CPU measurements](README.md#performance-changes-since-f0ca006)

In one cold comparison with `f0ca006`, this build delivered **520 presents in
145.485 seconds (3.57 FPS)** over world seconds +180 to +330. The earlier
build delivered 454 in 144.350 seconds (3.15 FPS). Over +180 to +300 the rates
were 3.43 and 2.95 FPS respectively. Both runs used the same preset and
synchronous frame-1820 dump. Their cameras faced different parts of the room.

About 13 minutes into the longer test the player rotated the camera. In a
frontal view at +810 seconds, eyes, teeth and hair rendered, while the nose,
cheeks, lips and jaw remained missing. The turn happened after the fixed
performance comparison. Window mesh and yard posts remained visible.

The harness stopped the title at the 30-minute limit (1,802.11 seconds
including shutdown). From world +181.35 to +1734.41 the renderer delivered
6,589 presents in 1,553.061 seconds (4.24 FPS). The corresponding late
slices averaged 4.07, 4.20, 4.34, 4.39 and 4.67 FPS. The last slice spans
204 seconds. The player’s camera turn changes the visible workload during
this run. Only the fixed initial window has a matching control.

### Facial rendering follow-up

A separate ten-minute replay tested PiloteCode’s pixel helper-lane change
from fxceless PR993. We verified that the shader bound to the skinned draw
used the new translation. Both the old and new shaders passed the synthetic
edge-pixel test under GE-Proton 11-14 on the RTX 5090. In both frontal
intervals the helper build still showed exposed eyes and teeth and a gap
down to the shirt collar. This experiment remains outside the published
build. The captures leave the source of missing facial coverage open.
[Upstream shader change](https://github.com/fxceless/victory-fork/commit/c0647e1232563ac1aa4787593913808d40d48f45)

### Descriptor slot cache

The Linux preset enables PR995’s `VICTORY_HEAP_SLOT_CACHE=1`. The renderer
reuses the resolved buffer for a descriptor slot after checking the recorded
descriptor, live view and host copy. We changed only this setting, using the
same executable, private checkpoint, cold caches, capture frame and window
size in all three trials. The two cache trials ran to their 390-second limits.

| RTX 5090 world interval or counter | Off | On, first run | On, repeat |
| --- | ---: | ---: | ---: |
| +90 to +180 s | 2.42 FPS | 3.02 FPS | 3.09 FPS |
| +180 to +300 s | 3.43 FPS | 3.77 FPS | 4.03 FPS |
| +180 to +330 s | 3.57 FPS | 3.89 FPS | 4.07 FPS |
| Median descriptor walk in the last interval | 35.25 ms | 24.20 ms | 22.75 ms |
| Median slots reused per report | 0 | 4,333 | 4,400 |

The first cache run and the control faced the yard window in frames 1820
and 2048. Both showed the wall, uprights, window mesh, bunk rail and distant
watchtower. The 30-minute endurance result above used the same executable
with this setting off. [Upstream performance work](https://github.com/fxceless/victory-fork/pull/995)

### Room comparison for VIctory PR2885

Both trials used cold caches, the same checkpoint and scripted camera route,
and the published descriptor slot cache setting. The control delivered 2.95
FPS over world seconds +180 to +330. The candidate delivered 3.03 FPS over
that interval. Each condition had one replay. The candidate log recorded a
live structured buffer bound through a raw view with an 8-byte prefix.

The side views retained the same swept crown and hanging hair. The matched
near-frontal views still showed exposed eyes and teeth, with the cheek, lips
and jaw missing. The launch harness ended by signal after about 572 seconds,
after both camera turns. The control completed its 600-second limit. The
candidate log contains no device-loss or fatal-error report. The port remains
a private source candidate while the visible defects are investigated.
