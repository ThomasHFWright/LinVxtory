"""Verify the locked renderer inputs before rebuilding."""
import hashlib
import json
import sys
from pathlib import Path

source = Path(sys.argv[1])
lock = Path(__file__).with_name('build-inputs.json')
expected = json.loads(lock.read_text())
actual = {
    path.relative_to(source).as_posix()
    for folder in ('apps', 'crates', '.cargo')
    for path in (source / folder).rglob('*')
    if path.is_file()
}
extra = actual - expected.keys()
if extra:
    raise SystemExit('Unexpected build inputs: ' + ', '.join(sorted(extra)))
for name, digest in expected.items():
    path = source / name
    if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        raise SystemExit('Missing or different build input: ' + name)
print(f'PASS: {len(expected)} build inputs match.')
