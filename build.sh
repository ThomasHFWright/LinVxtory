#!/bin/sh
# Reconstruct the pinned inkVI renderer and apply LinVxtory's Linux overlay.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    echo 'Usage: sh build.sh /path/to/renderer-source' >&2
    exit 1
fi

src=$1
base=ceb9e1f7a0180e61961bfde85d69d24e2dd84b39
inkvi=7031c8259f4c8a906d06c1e3f4726d455126bfca
stack_tree=3535e80cb6d1f7b4adff282678fe41d77f99a22c
base_url=https://github.com/fxceless/victory-fork.git

if [ ! -e "$src" ]; then
    git init -q -- "$src"
fi
src=$(CDPATH= cd -- "$src" && pwd)
if [ ! -d "$src/.git" ]; then
    echo 'Use a new path or the pinned renderer checkout.' >&2
    exit 1
fi
if ! git -C "$src" rev-parse --verify HEAD >/dev/null 2>&1; then
    test -z "$(git -C "$src" status --porcelain)"
    git -C "$src" fetch -q --no-tags --depth=1 "$base_url" "$base"
    git -C "$src" checkout -q --detach FETCH_HEAD
fi
head=$(git -C "$src" rev-parse HEAD)
if [ "$head" != "$base" ] && [ "$(git -C "$src" rev-parse 'HEAD^{tree}')" != "$stack_tree" ]; then
    echo 'Expected the pinned inkVI stack. Use a new output path to build it.' >&2
    exit 1
fi

kit="$src/.git/inkvi-kit"
if [ ! -d "$kit/.git" ]; then
    git init -q -- "$kit"
fi
if ! git -C "$kit" rev-parse --verify HEAD >/dev/null 2>&1; then
    git -C "$kit" fetch -q --no-tags --depth=1 https://github.com/OctoAlgo/inkVI.git "$inkvi"
    git -C "$kit" checkout -q --detach FETCH_HEAD
fi
test "$(git -C "$kit" rev-parse HEAD)" = "$inkvi"
git -C "$kit" diff --quiet HEAD
if [ "$head" = "$base" ]; then
    test -z "$(git -C "$src" status --porcelain)" || {
        echo 'The pinned base has local changes.' >&2
        exit 1
    }
    (cd "$kit/patches" && sha256sum --check --quiet SHA256SUMS)
    set --
    while IFS= read -r patch; do
        set -- "$@" "$kit/patches/$patch"
    done < "$kit/patches/series"
    git -C "$src" -c user.name=inkvi -c user.email=inkvi@localhost -c commit.gpgsign=false \
        am -q --committer-date-is-author-date "$@"
fi
test "$(git -C "$src" rev-parse 'HEAD^{tree}')" = "$stack_tree"
if [ -z "$(git -C "$src" status --porcelain)" ]; then
    git -C "$src" apply "$root/docs/inkvi/linux-overlay.patch.txt"
fi

helper=crates/victory-gpu/src/brighten.cso
if [ ! -f "$src/$helper" ]; then
    from=7406afbbd9cef0dcddef0b0a23b4855437715eb5
    git -C "$src" fetch -q --no-tags --depth=1 "$base_url" "$from"
    git -C "$src" show "$from:$helper" > "$src/$helper"
fi
python3 "$root/docs/inkvi/verify.py" "$src"
cd "$src"
exec cargo build --locked --offline --release -p victory --target x86_64-pc-windows-msvc
