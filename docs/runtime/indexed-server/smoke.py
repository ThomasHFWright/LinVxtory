#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Offline synthetic validation; never launches a title or reuses a shared prefix."""
import argparse
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import sys
import time
from build import HERE, LOCK, checked, digest


def inside(root, variant):
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    runtime = root / 'runtime/files'
    out = root / variant
    env = os.environ.copy()
    for name in ('DISPLAY', 'WAYLAND_DISPLAY', 'WINESERVERSOCKET', 'WINELOADER', 'LD_PRELOAD'):
        env.pop(name, None)
    env.update(WINEPREFIX=str(out / 'prefix'), WINESERVER=str(runtime / 'bin/wineserver'),
               WINEDEBUG='-all', WINEDLLOVERRIDES='mscoree,mshtml=',
               WINEDLLPATH=str(runtime / 'lib/wine'),
               LD_LIBRARY_PATH=str(runtime / 'lib/x86_64-linux-gnu') + ':' + str(runtime / 'lib/i386-linux-gnu'),
               XDG_CACHE_HOME=str(out / 'cache'), XDG_RUNTIME_DIR='/tmp',
               PULSE_SERVER='unix:/tmp/no-audio-server')
    (out / 'prefix').mkdir()
    identities = {}
    start = time.monotonic()
    with (out / 'stdout.log').open('w') as stdout, (out / 'stderr.log').open('w') as stderr:
        child = subprocess.Popen([str(runtime / 'bin/wine'), str(root / 'mapping-probe.exe')],
                                 env=env, stdout=stdout, stderr=stderr)
        while child.poll() is None and time.monotonic() - start < 100:
            for proc in Path('/proc').iterdir():
                if not proc.name.isdigit() or proc.name in identities:
                    continue
                try:
                    executable = os.readlink(proc / 'exe')
                    if Path(executable).name == 'wineserver':
                        identities[proc.name] = {'exe': executable, 'sha256': digest(proc / 'exe')}
                except (OSError, ProcessLookupError):
                    pass
            time.sleep(.05)
        if child.poll() is None:
            child.kill()
            child.wait()
    # Only this fresh prefix/socket is accessible inside this private PID and /tmp namespace.
    subprocess.run([str(runtime / 'bin/wineserver'), '-k'], env=env, timeout=10,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    expected = digest(root / 'servers' / variant)
    identity_ok = len(identities) == 1 and all(x['sha256'] == expected for x in identities.values())
    result = {'variant': variant, 'exit': child.returncode, 'identity_ok': identity_ok,
              'servers': identities, 'expected_sha256': expected,
              'stdout': (out / 'stdout.log').read_text(), 'seconds': time.monotonic() - start}
    (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    return 0 if child.returncode == 0 and identity_ok else 1


def smoke(args):
    build = args.build.expanduser().resolve()
    runtime = args.runtime.expanduser().resolve()
    root = build / 'smoke'
    if root.exists():
        raise RuntimeError('smoke directory already exists; retain its evidence and use a fresh build')
    if build == runtime or build.is_relative_to(runtime) or runtime.is_relative_to(build):
        raise RuntimeError('Input GE runtime and output build must be separate directories')
    checked(runtime / 'files/bin/wineserver', LOCK['stock_runtime_sha256'])
    for name, sha in LOCK['runtime_client_sha256'].items():
        checked(runtime / 'files' / name, sha)
    result = json.loads((build / 'build-result.json').read_text())
    for name in ('stock-rebuilt', 'indexed'):
        checked(build / 'servers' / name, result['servers'][name]['sha256'])
    root.mkdir()
    subprocess.run(['cp', '-a', '--reflink=auto', str(runtime), str(root / 'runtime')], check=True)
    (root / 'servers').mkdir()
    for name in ('stock-rebuilt', 'indexed'):
        shutil.copy2(build / 'servers' / name, root / 'servers' / name)
    shutil.copy2(runtime / 'files/bin/wineserver', root / 'servers/stock-installed')
    subprocess.run([args.cc, '-O2', str(HERE / 'mapping-probe.c'), '-o',
                    str(root / 'mapping-probe.exe'), '-lmincore'], check=True)
    results = []
    for variant in ('stock-installed', 'stock-rebuilt', 'indexed'):
        out = root / variant
        out.mkdir()
        command = ['bwrap', '--ro-bind', '/', '/', '--bind', str(out), str(out),
                   '--ro-bind', str(root / 'servers' / variant), str(root / 'runtime/files/bin/wineserver'),
                   '--tmpfs', '/tmp', '--unshare-net', '--unshare-pid', '--die-with-parent',
                   '--new-session', '--dev', '/dev', '--proc', '/proc']
        if Path('/dev/ntsync').exists():
            command += ['--dev-bind', '/dev/ntsync', '/dev/ntsync']
        command += [sys.executable, str(HERE / 'smoke.py'), '_inside', str(root), variant]
        with (out / 'supervisor.log').open('w') as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, timeout=120, check=True)
        results.append(json.loads((out / 'result.json').read_text()))
        print(variant, 'PASS: child exit0 and actual server SHA verified', flush=True)
    if len({r['stdout'] for r in results}) != 1 or 'failures=0' not in results[0]['stdout']:
        raise RuntimeError('Stock/indexed synthetic output differs')
    (root / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
    print('PASS: identical parent/child results. No title/GPU launch; no shared server stopped.')


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '_inside':
        sys.exit(inside(Path(sys.argv[2]), sys.argv[3]))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build', type=Path, required=True)
    parser.add_argument('--runtime', type=Path, required=True, help='unmodified GE-Proton11-14 root, read only')
    parser.add_argument('--cc', default='x86_64-w64-mingw32-gcc', help='modern MinGW/LLVM-MinGW C compiler')
    smoke(parser.parse_args())
