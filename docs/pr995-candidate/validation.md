# Candidate source verification

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
source-kit preparation ran no title or GPU jobs. LinVictory's GPU and bounded
frame-rate results are described in the README; 30-minute stability remains
pending. Raw logs, screenshots, checkpoints, caches and runtime binaries are
excluded. All validation here concerns this exact candidate and kit, not
OctoAlgo's unpublished combined renderer.
