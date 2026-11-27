#!/bin/sh
# Reconstruct the pinned renderer and build the tested Linux configuration.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    echo 'Usage: sh build.sh /path/to/renderer-source' >&2
    exit 1
fi

src=$1
base=014d52dedf837d1ac660a0bd256c4704be9eeb40
base_tree=0b2726fef1258f1d7817afdf0af8b251870e408b
linux_tree=0ab430bbecb29f450d2d9e837673ad10aa1210b4
tested_tree=3d033cbd633757d2fd15f97a4323d566e8a043ba
base_url=https://github.com/TheInfamous404/VIctory-7900XTX.git
kit="$root/docs/7900"

if [ -z "${VICTORY_DLSS_SDK:-}" ] ||
   [ ! -f "$VICTORY_DLSS_SDK/lib/Windows_x86_64/nvsdk_ngx_d.lib" ]; then
    echo 'Set VICTORY_DLSS_SDK to the local NVIDIA DLSS SDK directory.' >&2
    exit 1
fi

if [ ! -e "$src" ]; then
    git init -q -- "$src"
fi
src=$(CDPATH= cd -- "$src" && pwd)
if [ ! -d "$src/.git" ]; then
    echo 'Use a new directory for the pinned renderer.' >&2
    exit 1
fi
if ! git -C "$src" rev-parse --verify HEAD >/dev/null 2>&1; then
    test -z "$(git -C "$src" status --porcelain)"
    git -C "$src" fetch -q --no-tags --depth=1 "$base_url" "$base"
    git -C "$src" checkout -q --detach FETCH_HEAD
fi

if [ "$(git -C "$src" rev-parse HEAD)" != "$base" ] ||
   [ "$(git -C "$src" rev-parse 'HEAD^{tree}')" != "$base_tree" ]; then
    echo 'Expected the pinned 7900XTX base. Use a new output path.' >&2
    exit 1
fi
if [ -z "$(git -C "$src" status --porcelain)" ]; then
    git -C "$src" apply --index "$kit/linux-overlay.patch.txt"
fi
if [ "$(git -C "$src" write-tree)" = "$linux_tree" ] && git -C "$src" diff --quiet; then
    git -C "$src" apply --index "$kit/dlss-4k.patch.txt"
fi
if [ "$(git -C "$src" write-tree)" != "$tested_tree" ] ||
   ! git -C "$src" diff --quiet; then
    echo 'Renderer changes differ from the tested source tree.' >&2
    exit 1
fi
if [ -n "$(git -C "$src" ls-files --others -- apps crates .cargo Cargo.lock Cargo.toml rust-toolchain.toml)" ]; then
    echo 'Unexpected files in the renderer build inputs.' >&2
    exit 1
fi
cd "$src"
sha256sum --check --quiet "$kit/build-inputs.sha256.txt"
printf '%s\n' 'Verified the locked renderer tree and build inputs.'
exec cargo build --locked --offline --profile diag --features dlss -p victory --target x86_64-pc-windows-msvc
