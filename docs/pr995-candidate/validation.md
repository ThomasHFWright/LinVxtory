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

The public pin is `7406afbbd9cef0dcddef0b0a23b4855437715eb5`.
The complete reconstructed Git tree is `cc6e33fe99572a2d433e82a49e8429678b70ca5a`.
The tested `three-pr.exe` has SHA-256
`1770ac0ec70041aebb00937618af696ee3e16d0d0efeb6e39987952d311516a5`.
A different build path or compiler can change the executable digest.

The three-PR delta changes six files and adds two. Its SHA-256 is
`c47ca445f08f13a27d4b9b9a98b4d4e0d22ed510613ca4ad1bd50836c48b28c2`.

| Port | Changed paths under `crates/` |
| --- | --- |
| PR1722 | `victory-gpu/src/pipeline/library.rs` |
| PR2026 | `victory-formats/src/texture.rs` |
| PR2398 | `victory-gpu/src/pipeline/make.rs`, `victory-shader/src/emit/container.rs`, `lower.rs`, `mod.rs`, and new `linked.rs` and `linked_tests.rs` in the same emit directory |

The verifier checks full-file SHA-256 hashes for all 190 inputs and rejects
extra files under the source directories. The reconstruction uses the original
Linux patch and its lock, followed by the three-PR delta. [Original lock](source-lock.json) · [Three-PR overrides and additions](upstream-files.json)

A clean pinned checkout, the reconstructed Linux baseline and an already
updated checkout all reached the same source tree through the one-argument
wrapper. The finished checkout needed no further patching. Extra arguments
and incompatible source stacks were rejected before Cargo. An offline Windows
diagnostics build from this exact tree passed, as did 189 native shader tests
with two ignored.

## Linux checkpoint and hardware checks

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
