"""Check the Linux reconstruction lock and, when supplied, its final update."""
import hashlib
import json
import sys
from pathlib import Path

kit = Path(__file__).resolve().parent
source = Path(sys.argv[1])
expected = json.loads((kit / 'source-lock.json').read_text())['source_files_sha256']
for overlay in sys.argv[2:]:
    expected.update(json.loads(Path(overlay).read_text()))

extra = {str(p.relative_to(source)) for folder in ('apps', 'crates', '.cargo')
         for p in (source / folder).rglob('*') if p.is_file()} - expected.keys()
if extra:
    raise SystemExit('Unexpected build inputs: ' + ', '.join(sorted(extra)))
for name, digest in expected.items():
    path = source / name
    if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        raise SystemExit('Missing or different build input: ' + name)
print(f'PASS: {len(expected)} build inputs match.')
