#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Build only private, GE-Proton11-14-compatible control/indexed wineservers."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import time
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
LOCK = json.loads((HERE / 'sources.lock.json').read_text())
WORK_ROOT = None


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def checked(path, expected):
    actual = digest(path)
    if actual != expected:
        raise RuntimeError(f'SHA256 mismatch: {path}: {actual}; expected {expected}')


def download(url, path):
    if path.exists():
        return
    partial = path.with_suffix(path.suffix + '.part')
    request = urllib.request.Request(url, headers={'User-Agent': 'ge1114-indexed-source-kit/1'})
    try:
        for attempt in range(4):
            try:
                with urllib.request.urlopen(request, timeout=60) as source, partial.open('wb') as target:
                    shutil.copyfileobj(source, target)
                partial.rename(path)
                break
            except (urllib.error.URLError, TimeoutError):
                if attempt == 3:
                    raise
                time.sleep(attempt + 1)
    finally:
        partial.unlink(missing_ok=True)


def run(argv, cwd, log, data=None):
    with log.open('a') as stream:
        stream.write(f'cwd={cwd}\nargv={argv!r}\n')
        stream.flush()
        # A fixed private mount avoids Wine makefile bugs with whitespace in source paths.
        mapped = lambda p: '/tmp/build/' + str(Path(p).relative_to(WORK_ROOT))
        argv = [mapped(a) if os.path.isabs(a) and Path(a).is_relative_to(WORK_ROOT) else a for a in argv]
        sandbox = ['bwrap', '--ro-bind', '/', '/', '--tmpfs', '/tmp',
                   '--bind', str(WORK_ROOT), '/tmp/build', '--unshare-net', '--die-with-parent',
                   '--dev', '/dev', '--proc', '/proc', '--chdir', mapped(cwd)]
        subprocess.run(sandbox + argv, input=data, text=True, stdout=stream,
                       stderr=subprocess.STDOUT, check=True)


def apply_ge(wine, source, item, log):
    patch = source / item['path']
    checked(patch, item['sha256'])
    pieces = re.split(r'(?=^diff --git )', patch.read_text(), flags=re.M)
    selected, names = [], []
    for piece in pieces:
        match = re.match(r'diff --git a/(\S+) b/(\S+)', piece)
        if match and match[2] in item['files']:
            selected.append(piece)
            names.append(match[2])
    if names != item['files']:
        raise RuntimeError(f'Patch file selection changed: {patch}')
    run(['patch', '--batch', '--fuzz=0', '-p1'], wine, log, ''.join(selected))


def verify_server(wine, expected):
    for name, sha in expected.items():
        checked(wine / 'server' / name, sha)


def build(args):
    global WORK_ROOT
    # Refuse shared/in-place work and never install, edit an existing runtime, or run Wine.
    work = args.work.expanduser().resolve()
    cache = args.cache.expanduser().resolve()
    WORK_ROOT = work
    if work.exists():
        raise RuntimeError('Work directory already exists; choose a new private directory')
    if cache == work or cache.is_relative_to(work):
        raise RuntimeError('Cache must be outside the fresh work directory')
    for tool in ['cc', 'make', 'patch', 'autoreconf', 'flex', 'bison', 'perl', 'bwrap']:
        if not shutil.which(tool):
            raise RuntimeError(f'Missing build dependency: {tool}')
    work.mkdir(parents=True)
    cache.mkdir(parents=True, exist_ok=True)
    logs = work / 'logs'
    logs.mkdir()
    sources = work / 'sources'
    sources.mkdir()
    trees = {}
    for source in LOCK['sources']:
        archive = cache / source['archive']
        print('Verify/extract', source['name'], flush=True)
        download(source['url'], archive)
        checked(archive, source['sha256'])
        with tarfile.open(archive) as tar:
            # Python 3.12+ data filter prevents paths/links escaping the extraction root.
            tar.extractall(sources, filter='data')
        tree = sources / source['root']
        destination = sources / source['name']
        tree.rename(destination)
        trees[source['name']] = destination
    # The comment's patch is downloaded, not redistributed or licensed by this kit.
    comment = cache / 'issue184-comment.json'
    download(LOCK['upstream_patch']['url'], comment)
    comments = json.loads(comment.read_text())
    if isinstance(comments, dict):
        comments = [comments]
    body = next(c['body'] for c in comments if c['id'] == LOCK['upstream_patch']['comment_id'])
    patch_text = body.split('```diff\n')[1].split('```')[0]
    upstream = work / 'upstream-views.patch'
    upstream.write_text(patch_text)
    checked(upstream, LOCK['upstream_patch']['sha256'])
    wine = trees['wine']
    for item in LOCK['patches']:
        apply_ge(wine, trees[item['source']], item, logs / 'ge-patches.log')
    verify_server(wine, LOCK['baseline'])
    # Reproduce the tested server-only preparation; no foreign shell script is executed.
    config = wine / 'configure.ac'
    config.write_text(re.sub(r'^WINE_CONFIG_MAKEFILE\(([^)]+)\).*$',
        lambda m: m[0] if m[1] == 'server' else '', config.read_text(), flags=re.M))
    run(['./tools/make_requests'], wine, logs / 'generate.log')
    checked(wine / 'include/wine/server_protocol.h', LOCK['protocol_sha256'])
    registry = trees['vulkan-headers'] / 'registry'
    run(['dlls/winevulkan/make_vulkan', '-x', str(registry / 'vk.xml'),
         '-X', str(registry / 'video.xml')], wine, logs / 'generate.log')
    run(['autoreconf', '-f'], wine, logs / 'generate.log')
    # Wine makefiles need space-free source references, even when the work path has spaces.
    configure = str(wine / 'configure')
    stock = work / 'build-stock'
    stock.mkdir()
    run([configure, '--enable-win64', '--without-mingw', '--without-x',
         '--without-freetype', '--disable-tests'], stock, logs / 'configure.log')
    run(['make', f'-j{args.jobs}', 'server/wineserver'], stock, logs / 'stock-build.log')
    servers = work / 'servers'
    servers.mkdir()
    shutil.copy2(stock / 'server/wineserver', servers / 'stock-rebuilt')

    run(['patch', '--batch', '--fuzz=2', '-p1'], wine, logs / 'index-patch.log', patch_text)
    mapping = wine / 'server/mapping.c'
    text = mapping.read_text()
    anchor = 'static void add_view_to_tree( struct process *process, struct memory_view *view )\n{\n'
    if text.count(anchor) != 1:
        raise RuntimeError('Unexpected add_view_to_tree definition')
    mapping.write_text(text.replace(anchor, anchor + '    view->tree = NULL;\n'))
    # Offset/fuzz adaptation is accepted ONLY if all four final files match the tested source.
    verify_server(wine, LOCK['indexed'])
    checked(wine / 'include/wine/server_protocol.h', LOCK['protocol_sha256'])
    indexed = work / 'build-indexed'
    # Independently configured, with exactly the same flags as the stock control.
    indexed.mkdir()
    run([configure, '--enable-win64', '--without-mingw', '--without-x',
         '--without-freetype', '--disable-tests'], indexed, logs / 'configure-indexed.log')
    run(['make', f'-j{args.jobs}', 'server/wineserver'], indexed, logs / 'indexed-build.log')
    shutil.copy2(indexed / 'server/wineserver', servers / 'indexed')
    result = {'protocol': LOCK['protocol'], 'servers': {}}
    for server in servers.iterdir():
        version = subprocess.check_output([str(server), '--version'], text=True, stderr=subprocess.STDOUT).strip()
        if version != 'Wine 11.0':
            raise RuntimeError(f'Unexpected server version: {version}')
        result['servers'][server.name] = {'sha256': digest(server), 'version': version}
    (work / 'build-result.json').write_text(json.dumps(result, indent=2) + '\n')
    print('Built private servers; no runtime installed or launched:', servers)
    print('Version is not sufficient: run the isolated synthetic validation before use.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work', type=Path, required=True, help='new, nonexistent private directory')
    parser.add_argument('--cache', type=Path, required=True, help='download cache; all content hash checked')
    parser.add_argument('--jobs', type=int, default=2)
    args = parser.parse_args()
    if not 1 <= args.jobs <= 64:
        parser.error('--jobs must be between 1 and 64')
    build(args)
