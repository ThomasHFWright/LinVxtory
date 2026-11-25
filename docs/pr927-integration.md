# Local PR927 integration candidate

Prepared 2026-11-25 on `local/half-view-integration`, based exactly on
`984fc7c7f0af2ab186ed266fc272d381e56e55e6`. This isolated candidate is for review
after performance work. It has **not been built, tested, run or published**.
The active checkout, shared status and runtime were not edited.

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
| `5dc9e63f04cac3e508d8a5e9cb6edd72a23ca175` | `c493d6569984a4e0b329b50ad2a6c1db388f272a` |
| `befaf3b5aa91966d2569985eeba808d4d8017026` | `3de5f26edbbe029bcbf3e046cd9e2108ebee8bc0` |
| `b6bf65167f4407684bccfe201b30d084102d8862` | `fd7ddb94e7484f41e92064bcec0b199f3ffe72a9` |
| `b9c378134968f93739a9f5864052f293ed67bf4c` | `3bd82ed02e5c5b291eb2d292847d4163e5b05da5` |

Local conflict resolution, helper extraction, additional regressions and this
record are AI-assisted integration work with OpenAI Codex. Upstream PR24's CBV
size clamp is already in the base; it was not duplicated. No other rendering
or performance PR was imported.

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

## Main-owned validation gates

All gates are pending. Run from this isolated checkout with a separate target
directory and offline dependency resolution when authorized:

1. Native GPU crate `fresh_table::tests` plus native shader regressions. Inspect
   test counts rather than accepting a zero-test filtered run.
2. Windows compilation/all-target diagnostics checks and policy. Run shader and
   GPU suites, retaining the base's known-failure accounting.
3. Targeted `entries_round_trip_and_reject_damage`,
   `half_places_are_the_descriptors_the_program_loads`,
   `a_fresh_table_holds_the_half_views_past_its_range_starts`,
   `an_interior_uint_half_view_is_prepared_without_a_cbv`,
   `a_format_load_read_as_halves_loads_the_integer`, unsigned-view mapping and
   exhaustive half-pattern readbacks. Keep null-CBV, copied-descriptor,
   cross-page and mixed-mip state regressions in the integration check.
4. Hardware binder check alternating normal/half shaders on identical tables,
   including residency change and resource/counter retention. Then a matched
   visual/performance comparison only after the performance baseline is ready.

This port retains upstream limits: dynamically selected descriptors and
non-table accesses do not gain the fixed-slot half path; unsupported normalized
or wider conversion formats remain inexact. Descriptor snapshotting and
residency upload are still not atomic. The cache still uses a 64-bit contents
hash. No new claim of Linux correctness, facial improvement, stability or
performance follows from this unexecuted integration.
