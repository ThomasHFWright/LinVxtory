#!/bin/sh
# Build the tested PR995 source in a user-owned external checkout.
set -eu
kit=$(CDPATH= cd -- "$(dirname -- "$0")/docs/pr995-candidate" && pwd)
src=${1:?Usage: sh build.sh /path/to/external/pr995-source}
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
if git -C "$src" diff --quiet HEAD -- && git -C "$src" apply --check --unidiff-zero "$kit/candidate.patch.txt"; then
    git -C "$src" apply --unidiff-zero "$kit/candidate.patch.txt"
fi
sh "$kit/verify.sh" "$src"
cd "$src"
exec cargo build --locked --offline --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc
