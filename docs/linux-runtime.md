# Runtime used for the first Linux world visuals

The working local trial used GE-Proton 11-14 for Wine/DXGI and a separately
built vkd3d-proton. These are source instructions; no runtime or game binaries
are distributed by this repository. Upstream runtime licenses still apply.

## Source revisions

[The source lock](runtime/source-lock.json) records the runtime, converter and
nested dependency revisions actually used. Paths are relative to the runtime
checkout. Initialize submodules and check out those revisions before building;
the converter and several nested dependencies differ from the runtime's
original submodule pins. The converter has no additional source patch.

Apply [the runtime patch](runtime/vkd3d-proton-linux.patch.txt) to a fresh checkout
at the recorded base. The `.txt` extension keeps source patches within this
repository's text-file asset policy; `git apply` accepts it directly.

The patch keeps CUSTOM and READBACK buffer allocations separate when their
original heap types imply different Vulkan usages. It also normalizes null
constant-buffer descriptions and zero-sized CBVs through all three existing
null-descriptor backends. A zero-sized CBV ignores its address; expanding it to
a nonzero size would expose unrelated bytes.

Synthetic checks reproduced invalid buffer-usage flags before the allocator
fix and clean validation afterward. The null-CBV tests reproduced a zero-range
Vulkan error and a stale descriptor before the fix; afterward, valid data and
all null cases read back correctly with validation enabled. These fixes were
present in the failing title trial immediately before the code-table change.

## Build

The tested toolchain was llvm-mingw 20261117 (UCRT, x86-64), Meson and Ninja,
with Wine's `widl`. Put the toolchain's `bin` directory first in `PATH`. Use
separate source/build directories outside the game input folder.

From this repository, with the source revisions already checked out:

```sh
linvictory_source="$PWD"
runtime_source=/path/to/vkd3d-proton
runtime_build=/path/to/vkd3d-build

git -C "$runtime_source" apply --check "$linvictory_source/docs/runtime/vkd3d-proton-linux.patch.txt"
git -C "$runtime_source" apply "$linvictory_source/docs/runtime/vkd3d-proton-linux.patch.txt"
meson setup "$runtime_build" "$runtime_source" \
  --cross-file "$linvictory_source/docs/runtime/llvm-mingw-win64.txt" \
  --buildtype=release --wrap-mode=nodownload \
  -Denable_trace=true -Denable_descriptor_qa=true \
  -Denable_tests=false -Denable_extras=false
ninja -C "$runtime_build" -j2
```

The resulting libraries are `libs/d3d12/d3d12.dll` and
`libs/d3d12core/d3d12core.dll`. Install them beside your locally built
`victory.exe`, together with GE-Proton's `dxgi.dll`; use native overrides for
`d3d12,d3d12core,dxgi`. Keep the original runtime separately for comparisons.

Build the app with the Windows MSVC target and your configured xwin SDK/linker:

```sh
cargo build --locked --target x86_64-pc-windows-msvc \
  --profile diag -p victory --features diagnostics
```

## Trial conditions and limits

- CachyOS, RTX 5090, NVIDIA 620.73.04; DXC/DXIL 1.9.2609.
- `VICTORY_NO_WATCHDOG=1`, with an external 900-second time limit.
- `VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains` and
  `VKD3D_CONFIG=breadcrumbs`; the initial positive trial also enabled `vk_debug`
  and Vulkan validation layer 1.4.363.1, with core/synchronization checks.
- Normal shaders, default shader-loop limits and the default pipeline wait
  budget; no skipped shaders or forced debug pixel shader.
- Offline execution, read-only game input, separate prefix/cache/data folders.

These versions and flags describe the tested setup, not a guarantee that other
runtimes fail or that the game is fully playable. See [Linux validation](linux.md)
for the real-visual result, known failures and the later title assertion.
