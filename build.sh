#!/bin/sh
# Reconstruct a locked renderer in a user-owned PR995 checkout.
set -eu
kit=$(CDPATH= cd -- "$(dirname -- "$0")/docs/pr995-candidate" && pwd)
if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    echo 'Usage: sh build.sh /path/to/pr995-source' >&2
    exit 1
fi
src=$1
commit=7406afbbd9cef0dcddef0b0a23b4855437715eb5
if [ ! -e "$src" ]; then
    git init -q -- "$src"
    git -C "$src" fetch --depth=1 https://github.com/fxceless/victory-fork.git "$commit"
    git -C "$src" checkout -q --detach FETCH_HEAD
fi
test "$(git -C "$src" rev-parse HEAD)" = "$commit" || {
    echo 'Expected the pinned PR995 checkout.' >&2
    exit 1
}
if [ -z "$(git -C "$src" status --porcelain)" ]; then
    git -C "$src" apply --unidiff-zero "$kit/candidate.patch.txt"
fi
if python3 "$kit/check_inputs.py" "$src" >/dev/null 2>&1; then
    git -C "$src" apply --unidiff-zero "$kit/upstream.patch.txt"
fi
if python3 "$kit/check_inputs.py" "$src" "$kit/upstream-files.json" >/dev/null 2>&1; then
    git -C "$src" apply --unidiff-zero "$kit/next.patch.txt"
fi
sh "$kit/verify.sh" "$src"
cd "$src"
exec cargo build --locked --offline --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc
