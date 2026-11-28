#!/bin/sh
# Reconstruct the pinned renderer and build the tested Linux configuration.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
rr=1
control=0
while [ "$#" -gt 1 ]; do
    case "$1" in
        --rr) rr=1 ;;
        --sr) rr=0 ;;
        --control) control=1 ;;
        *) break ;;
    esac
    shift
done
if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    echo 'Usage: sh build.sh [--rr | --sr] [--control] /path/to/renderer-source' >&2
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
cd "$src"
if ! git diff --quiet; then
    echo 'Renderer has unstaged changes. Use a separate output path.' >&2
    exit 1
fi
if [ -n "$(git ls-files --others -- apps crates .cargo Cargo.lock Cargo.toml rust-toolchain.toml)" ]; then
    echo 'Unexpected files in the renderer build inputs.' >&2
    exit 1
fi
tree=$(git write-tree)
if [ "$tree" = "$tested_tree" ]; then
    sha256sum --check --quiet "$kit/build-inputs.sha256.txt"
fi
expected=$tested_tree
apply_locked_patch() {
    next=$1
    patch=$2
    if [ "$tree" = "$expected" ]; then
        git apply --index "$kit/$patch"
        tree=$(git write-tree)
        if [ "$tree" != "$next" ]; then
            echo "Unexpected renderer tree after $patch" >&2
            exit 1
        fi
    fi
    expected=$next
}
if [ "$rr" -eq 1 ]; then
    while read -r next patch; do
        apply_locked_patch "$next" "$patch"
    done < "$kit/rr-build-trees.txt"
fi
if [ "$control" -eq 0 ]; then
    while read -r sr_tree rr_tree patch; do
        if [ "$rr" -eq 1 ]; then
            apply_locked_patch "$rr_tree" "$patch"
        else
            apply_locked_patch "$sr_tree" "$patch"
        fi
    done < "$kit/perf-build-trees.txt"
    if [ "$rr" -eq 1 ]; then
        apply_locked_patch 182c2119e9e17ed516e3855045f3151b445c4d70 rr-normal-activation.patch.txt
        apply_locked_patch d6adab8c7687f8bc1ec4e794168f5490b864db78 rr-pane-free-cohort.patch.txt
        apply_locked_patch 4801c44982d83927308d42fa500f5632a8bc4105 rr-retain-native-opaque.patch.txt
        apply_locked_patch e6e197ed9bd33c5eea3bf82d87b2b4b2c16813da rr-specular-albedo.patch.txt
    fi
fi
if [ "$tree" != "$expected" ]; then
    echo 'Renderer changes differ from the selected source tree.' >&2
    exit 1
fi
printf '%s\n' 'Verified the locked renderer tree and build inputs.'
exec cargo build --locked --offline --profile diag --features dlss -p victory --target x86_64-pc-windows-msvc
