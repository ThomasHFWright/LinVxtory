# Private indexed wineserver for GE-Proton11-14

This optional Linux experiment builds a private wineserver with indexed mapped-view
lookups. It targets **GE-Proton11-14 x86_64 only**, with server protocol **938**.
It does not build or distribute a Proton runtime, install anything, modify a game,
or fix the later application crash. It is not an official Wine or GE release.

Wine's mapped-view list can become expensive when an application repeatedly maps
and unmaps thousands of small views. The patch adds a red-black-tree index while
retaining the list. See [issue 184](https://github.com/fxceless/victory-fork/issues/184)
and [UwUCammie's published patch](https://github.com/fxceless/victory-fork/issues/184#issuecomment-7148271684).

## Build locally

Requires Linux x86_64, Python **3.12+**, a C compiler and libc development headers,
GNU make/patch, Autoconf/Automake, flex, bison, Perl, and bubblewrap with working user
namespaces. Downloads require HTTPS access to GitHub. No root access is needed.
Do not run this as root. Package names vary by distribution.

```sh
python3 build.py \
  --work "$HOME/.cache/ge1114-indexed-build" \
  --cache "$HOME/.cache/ge1114-indexed-downloads" \
  --jobs 2
```

The work directory must **not exist**. Existing downloads are reused only after
SHA256 verification. On failure, retain the logs and choose a new work directory;
the tool does not clean, reset, or overwrite existing work. A failed or changed
hash is a hard stop, not a request to update the lock blindly.

The output contains `servers/stock-rebuilt`, `servers/indexed`, sources, logs,
and `build-result.json`. These binaries are local outputs, **not kit contents**.
There is no `make install`, package replacement, service restart, or title launch.
Build commands run offline in a bubblewrap namespace with the work directory as
the only writable persistent mount. Downloads happen before their use. Allow a
few GB of disk space, especially for the subsequent private runtime copy.

### What is pinned

`sources.lock.json` contains public URLs and SHA256 hashes for:

- The GE-Proton11-14 source archive and the selected GE patch files.
- Valve Wine `a47c73f52fd7b2efafc1c560710f3a5099ae51e0`.
- wine-staging `6cc805ea57132eeaf44764e9213823c9b8d0d300`.
- Khronos Vulkan-Headers `v1.4.363`, used only to generate declarations required
  by Wine's build preparation. No Vulkan runtime is built or exercised.
- The exact diff extracted from issue comment 7148271684.
- The four expected server files before and after patching, and generated protocol
  header. Offset/fuzz adaptation is accepted only when final file hashes match.

The builder replays the tested server build inputs from GE's preparation order:
90 selected patches, 37 touching the server. It does not execute GE's full prep
script or rebuild client DLLs. The mapping patch preserves GE's host-page
alignment check and explicitly initializes the new tree pointer for its
list-only duplicate-base fallback. The request protocol does not change.

This reproduces the tested source, not a bit-for-bit GE build: compiler, libc,
headers, and build flags can affect the binary. Version text alone does not prove
compatibility. Do not substitute another Proton/Wine branch just because it says
"Wine 11.0".

## Validate before use

Obtain your own unmodified **GE-Proton11-14-x86_64** runtime. The smoke tool checks
its stock server and Wine/ntdll fingerprints, then copies it privately. It needs a
modern x86_64 MinGW compiler with `VirtualAlloc2`, `MapViewOfFile3`, and `mincore`
imports; the original check used LLVM-MinGW. It does not download a compiler.

```sh
python3 smoke.py \
  --build "$HOME/.cache/ge1114-indexed-build" \
  --runtime "/path/to/GE-Proton11-14-x86_64" \
  --cc "/path/to/x86_64-w64-mingw32-gcc"
```

This creates a fresh `smoke/` directory. For installed stock, rebuilt stock, and
indexed servers, it runs a synthetic Windows parent and child in distinct offline
PID/network namespaces and fresh prefixes. `/tmp` and server sockets are private.
No display or GPU devices are exposed; `/dev/ntsync` is exposed if available.
It verifies the actual server executable's SHA256 through `/proc`, rather than
assuming an environment variable selected the right server.

Expected: identical 93,359 parent and 1,017 child result records, zero failures,
child exit0, and `identity_ok: true` in all three result files. The probe covers
22,552 views, alias data, overlap rejection, randomized unmap/remap, and placeholder
replacement/preservation. Logs remain in `smoke/`; do not publish whole prefixes.
This is focused compatibility coverage, not the full Wine test suite.

## Optional integration into an existing offline launcher

The smoke step leaves an independent runtime copy at `BUILD/smoke/runtime`.
**Use that copy's Wine executable.** GE's loader tries its adjacent wineserver
before `$WINESERVER`; setting `WINESERVER` while launching the installed Wine can
silently select the installed server.

Use these paths in your existing launcher (these are configuration additions,
not a complete application-launch command):

```sh
build="$HOME/.cache/ge1114-indexed-build"
files="$build/smoke/runtime/files"
wine="$files/bin/wine"
# Environment inside your existing offline sandbox:
# WINEPREFIX=/a/separate/private/prefix-for-this-trial
# WINESERVER="$files/bin/wineserver"
# WINEDLLPATH="$files/lib/vkd3d:$files/lib/wine"
# LD_LIBRARY_PATH="$files/lib/x86_64-linux-gnu:$files/lib/i386-linux-gnu"

# Add to bwrap for indexed B, targeting ONLY the private runtime copy:
# --ro-bind "$build/servers/indexed" "$files/bin/wineserver"
# For stock A, replace the source with:
# "$build/smoke/servers/stock-installed"
```

Keep the existing offline network isolation, a private `/tmp`, PID namespace,
`--die-with-parent`, and distinct prefixes. Preserve the launcher's other data,
cache, display, and device settings. Never bind over the installed runtime path,
replace installed binaries, kill a shared wineserver, or reuse a running prefix.
For A/B, use matching prefix/data/cache snapshots taken while their owners are
stopped. This kit does not copy active saves or automate title launches.

## Evidence and limitations

The original private build passed the parent/child comparison with both
server-side synchronization and native `ntsync`; executable hashes confirmed all
three selected servers. A clean rebuild from this kit also passed all three
server/parent/child comparisons under native `ntsync`. No generated game files, screenshots, raw title logs, prefixes,
or runtime binaries belong in this source kit.

One local title A/B measured **0.403 FPS stock versus 0.992 FPS indexed** over
seconds **180–330 after the first world bind** (about 2.46x in that interval).
**The indexed run still reached the same crash.** This is one setup and one
interval, not a general speed guarantee, gameplay validation, or a crash fix.
Long-lived correctness and other workloads remain unverified. Any application
whose cost is elsewhere may gain little; faster mapping can also expose existing
application timing bugs. Keep the option experimental and the stock control.

## Attribution and licensing

- Wine and Valve Wine provide the server, protocol, and tree implementation.
  Their source is [LGPL-2.1-or-later](https://github.com/ValveSoftware/wine/blob/a47c73f52fd7b2efafc1c560710f3a5099ae51e0/LICENSE),
  with existing file notices retained in local source downloads.
- GloriousEggroll/GE-Proton and wine-staging provide the pinned runtime lineage
  and compatibility patches. Their existing authorship and individual notices
  remain in the fetched sources; this kit does not relabel those contributions.
- **datingesp-hash** reported the original mapped-view bottleneck and an indexed
  GE server in issue184. **UwUCammie** published the Wine11.22 diff used here.
  The local adaptation retains GE11-14's alignment rule and initializes the
  duplicate-base fallback pointer. Integration and validation were AI-assisted
  with OpenAI Codex.
- The original scripts, synthetic probe, metadata, and documentation in this kit
  are MIT-licensed; see `LICENSE`. Khronos registry inputs carry their own
  Apache-2.0 OR MIT notice and are fetched separately.
- **No explicit standalone license accompanied the issue-comment patch when
  inspected.** The kit links to and downloads the hash-checked public diff for a
  local build; it does not bundle it or claim to license it under MIT. Obtain
  clarification from its author before redistributing an adapted patch or
  resulting binaries. Attribution alone does not resolve that question.

Publish only the allowlisted kit files. Do not archive the build directory,
download cache, runtime copy, local logs, or generated executables with it.
