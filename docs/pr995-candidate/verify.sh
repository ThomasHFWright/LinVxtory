#!/bin/sh
# Verify the patched public checkout, without running its code.
set -eu
kit_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
python3 - "$1" "$kit_dir/source-lock.json" <<'PY'
import hashlib, json, sys
from pathlib import Path
root = Path(sys.argv[1]).resolve()
lock = json.loads(Path(sys.argv[2]).read_text())
expected = lock['source_files_sha256']
extra = {str(p.relative_to(root)) for directory in ('apps', 'crates', '.cargo')
         for p in (root / directory).rglob('*') if p.is_file()} - expected.keys()
if extra:
    raise SystemExit('Unexpected build inputs: ' + ', '.join(sorted(extra)))
for name, digest in expected.items():
    path = root / name
    if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        raise SystemExit('Missing or different build input: ' + name)
print(f'PASS: {len(expected)} build inputs match the frozen executable source.')
print('Source equivalence only; paths/toolchain metadata can change EXE/PDB bytes.')
PY
