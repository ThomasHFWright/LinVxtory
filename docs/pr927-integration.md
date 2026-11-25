# PR927 half-data integration

Integrated on `linux-edge` on 2026-11-25. This record covers provenance,
binding changes, synthetic validation and known limits.

## Provenance

[fxceless/victory-fork PR927](https://github.com/fxceless/victory-fork/pull/927)
was rechecked through the GitHub connector before porting: open, non-draft,
head `b9c378134968f93739a9f5864052f293ed67bf4c`, source fork
`tippythetaidum/victory-fork`, base `0b0074e0b2765e574c622e9a3de566f20f04135b`.
All four commits were fetched and cherry-picked in order with `-x` provenance.
Original author names, addresses, author dates and co-author trailers remain.
Their commit messages describe upstream findings, not local validation.

| Upstream commit | Local port |
|---|---|
| `5dc9e63f04cac3e508d8a5e9cb6edd72a23ca175` | `97d729bbce07eb227a677a56b65ea539f9575bef` |
| `befaf3b5aa91966d2569985eeba808d4d8017026` | `eb4a6a3b65ebb8c5acddebff153f75ed005ce9a7` |
| `b6bf65167f4407684bccfe201b30d084102d8862` | `fb7c55904947e6ccef4b13df3a0220a513db2a9d` |
| `b9c378134968f93739a9f5864052f293ed67bf4c` | `49d3cad92aa1e3bb7b590d70470ee0e2a089d086` |

Local conflict resolution, helper extraction, additional regressions and this
record are AI-assisted integration work with OpenAI Codex. Upstream PR24's CBV
size clamp is already in the base; it was not duplicated. Other Linux changes are documented separately in [linux.md](linux.md).

## Changes and retained behavior

- The complete latest half-data path is retained: exact fixed descriptor
  offsets, interior half-view table spans, ordinary format loads consumed only
  as halves, integer buffer operations, companion UINT buffer views, and
  `HostShader::half_views` cache serialization.
- `draw.rs` merges the half-view selection into the existing residency-aware
  table identity. A small device-independent `fresh_table.rs` contains the
  production span/key calculations so CPU regressions exercise those exact
  functions. Dynamic selections and half selections both reach the existing
  cache-hit/miss readable paths. Existing bounds and allocation failure handling
  remain in the caller.
- Companion views are derived from the same `View` snapshot used for resource
  preparation, instead of upstream's extra metadata lookup. Resource identity,
  counter, range and all descriptor words except the remapped format survive.
  The unsigned-view test now uses a nonzero counter identity too.
- Follow-up cross-review found that an interior half slot already using UINT
  did not trigger a fresh table and was missed by ordinary range-start
  preparation. `State::tables` now also selects a fresh table for half metadata
  without an exact used-resource entry. The existing selected-slot path prepares
  it on misses and hits, without changing shader specialization or conversion
  warnings. Explicitly named UINT slots retain ordinary preparation. This adds
  dynamic-table work only for previously uncovered interior half slots.
- Both original and upstream descriptor tests survive the insertion conflict.
  Shader test support keeps `RawUavData` and `NullRawUav`, alongside the new
  `HalfUintUav`. The diagnostic view-trace scope remains around table binding.
- The latest helper-module insertion keeps our `code_table`, descriptor-kind
  and loop-cap test modules. The half-use analysis starts in `emit_body`, the
  local equivalent of upstream's initial lowering pass.
- Existing inline lowering tests move unchanged, apart from wrapper/indent,
  into `emit/lower_tests.rs`, still `lower::tests`. This move is included in
  the first conflict-resolved port so the new commits stay under the file-size
  limit. Final `lower.rs` is 506,816 bytes, below 524,288; upstream `halves.rs`
  remains a separate child module. Source fingerprinting recursively includes
  these files; shader-cache serialization changes also affect its cache key.

## Static inspection performed

`git diff --check` is clean. Text comparison confirms:

- `host.rs`, `resource.rs`, `list.rs`, `texture.rs`, `capture.rs`, `queue.rs` and
  `host/subresource.rs` exactly match the base: per-list first/last states,
  submission prelude, mixed-state copy/restore, twins and present remain intact.
- `emit/code_table.rs`, `descriptor_kind_tests.rs`, and `loop_cap_tests.rs`
  exactly match the base. The lowering diff adds the upstream half-data path
  and moves tests; it retains the local loop and cross-page lowering changes.
- `descriptor.rs` production code is unchanged, including null-CBV metadata.
  `descriptor/host.rs` production changes are only the added half-view helpers:
  copied-descriptor snapshots/fingerprints, retained resources and mip/plane
  view selection are intact. The `ensure_view_state`/readable tail in `draw.rs`
  exactly matches the base.
- The extracted lowering test body matches the base after removing its module
  wrapper and one indentation level. Both `State::tables` callers pass the new
  half metadata. HostShader constructors and cache serialization were reviewed.
- Changed/new source files are below 512 KiB; no conflict markers or specific
  title names were found in added text. No proprietary fixtures or generated
  assets were introduced. The repository policy executable was not run.

## Regressions added, not executed

The four device-independent tests in `fresh_table.rs` are compiled on native
test builds as well as Windows builds. They check:

1. Alternating normal/half views and resident mip changes get separate cached
   tables, while repeated equivalent selections reuse their earlier entries.
2. Every copied-record word, table base and table length contributes to the key.
3. An interior half view extends the allocation past its range start and joins
   the readable selections; duplicate or other-root metadata does not inflate it.
4. Indexed descriptors and declared ranges can extend the table beyond half views.

They would catch omission of half-selection or residency hashing, and omission
of half slots from the span. No mutation control has been run. The device-backed
span fixture now also covers `an_interior_uint_half_view_is_prepared_without_a_cbv`:
one SRV range at dword 0, half slot at dword 8, R16_UINT, no CBV. It asserts a
fresh two-slot span, reuse on the second bind, unchanged original metadata, and
ordinary preparation when the UINT slot is explicitly named. Removing the
interior-slot trigger would fail the fresh-table assertion by inspection; that
negative control has not been executed. Both fixture variants now require a
D3D12 host instead of silently returning when none exists.

## Linux validation

Validated on CachyOS / RTX 5090 / GE-Proton 11-14 with the runtime fixes described
in [linux-runtime.md](linux-runtime.md):

- Native shader suite: 179 passed, 2 ignored.
- Windows shader suite: 228 passed, 1 pre-existing stub-pipeline failure, 3 ignored.
- GPU suite: 211 passed, the same 4 pre-existing failures, 8 ignored (including
  the manual guarded-copy benchmark).
- Exhaustive unsigned-view readback: all 65,536 half bit patterns preserved;
  the float-conversion control preserved one.
- Fixed-slot/span, interior UINT preparation, cache serialization, alternating
  half-view/residency keys, null-CBV and subresource regressions passed.

The upstream readback test's compiler arguments were adapted to this fork's
portable compiler interface. The alternating cache-key check is synthetic; it
is not an exhaustive concurrent descriptor/residency test. The separate
negative mutations described above were not executed. See [Linux results](linux.md)
for title-run observations and remaining limitations.

This port retains upstream limits: dynamically selected descriptors and
non-table accesses do not gain the fixed-slot half path; unsupported normalized
or wider conversion formats remain inexact. Descriptor snapshotting and
residency upload are still not atomic. The cache still uses a 64-bit contents
hash. Passing these regressions establishes the tested data and binding behavior,
not complete facial rendering, stability or gameplay performance.
