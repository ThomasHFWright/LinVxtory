# LinVictory baseline

This fork continues the best local visible-world result, rather than a newer
fork that has only produced black world frames on the same Linux machine.
It derives from Marvisak/VIctory and Slaktarn700's `proton-fixes` branch.
All original history and the GPL-3.0-or-later license are retained.

## Source provenance

- Proton base: `993dff8` (Slaktarn700).
- Integer atomic handle annotation fix: `600ef99` (Nicolai Van der Storm).
- Opt-in title watchdog hook: `436b3db` (ruvenesh).
- [PR #206](https://github.com/Marvisak/VIctory/pull/206), head `05981dd`:
  descriptor view fixes, adapted to the Proton branch (AnoHunted).
- A capped descriptor split of 737856 mirrored / 262144 dynamic slots,
  the local variant tested in [issue #747](https://github.com/Marvisak/VIctory/issues/747).
- [PR #1024](https://github.com/Marvisak/VIctory/pull/1024), head `67125b3`:
  copy-back writable-memory changes (Duff); fixtures adapted for Proton's
  submission boundary.
- Local opt-in submission/binding diagnostics and a synthetic GPU binding probe.

The restoration preserves all eight modified files byte-for-byte against the
archived final source snapshot. Subsequent changes should remain separate from
this baseline; diagnostics are not evidence of correct normal rendering.

## Previously observed result (2026-11-24)

CachyOS, RTX 5090, NVIDIA 620.73.04, Wine 11.22, vkd3d-proton 3.2.0
(build `30b0eb9bc12dc9f`). The test disabled `VK_NV_raw_access_chains`, used
the default shader loop limit, no shader skips, and enabled `VICTORY_DEBUG_PS=1`
and `VICTORY_SHOW_TARGET=2560x1440-f28-3`.

Partial roads, signs and character geometry appeared in debug colours. World
frames continued until 553.2 seconds, then the title stopped at a breakpoint
with INSTANCE_LIST at 45000/45000. No NVIDIA Xid or GPU fence timeout was recorded.
Normal shaded rendering and controllable movement were not established.
[Original result report](https://github.com/Marvisak/VIctory/pull/1024#issuecomment-7141641576).

Historical checks: 350 workspace tests passed, two Wine harness failures,
seven ignored across 14 executables; diagnostics GPU suite 61 passed, one ignored.
Clippy and asset policy passed. These are historical results, not a fresh rerun.

## Working method

Reproduce the pinned baseline before changing rendering behavior. Preserve a
separate control binary, shader cache, prefix and settings; run offline with
read-only title input. Compare normal pixel shaders and the final presentation
path separately from the debug/intermediate-target view. Diagnose the exhausted
instance pool rather than bypassing its assertion or silently skipping work.

Only source and aggregate results belong in this repository. Game files,
generated shaders, images, captures, binaries and raw logs remain local.
