# Notes from a second RTX 5090 on Linux

Findings from another CachyOS + RTX 5090 setup (also an RTX 5070 Ti), NVIDIA
620.73.04, Proton 11.0 / GE-Proton 11-14, running fxceless/victory-fork main
plus Linux fixes. All numbers are from our own runs. They are measured on a
different base than the PR995 renderer, so treat them as leads, not as
promises for this tree. No binaries, game data or captures are included.

## Biggest single win: small buffers in the card's memory

`VICTORY_CARD_BUFFERS_MIN_KB=64` with `VICTORY_CARD_BUFFERS_MB=1536` makes
all title-buffer copies eligible for GPU memory within the residency budget. Without it, buffers under
1 MiB stay in PC memory: thousands of small buffers, and every shader access
to them crosses the bus. On our RTX 5090 (fork main, release, desktop, world
frames 300–600 of our cell save):

| | FPS | Waiting for the GPU | Copy-back |
|---|---:|---:|---:|
| Our previous defaults | 6.14 | 60 ms | 90 ms |
| `CARD_BUFFERS_MIN_KB=64`, `CARD_BUFFERS_MB=1536` | **8.19 (+33.5 % ± 6.4 %)** | 23 ms | 62 ms |

Keep `CARD_BUFFERS_MB` at 1536: with `MIN_KB=64` and 1024 MB, our runs were
about 50 % slower on our setup. The [standard source build](pr995-candidate/README.md#small-buffer-copies-in-gpu-memory)
includes the threshold from fxceless's
[c36971b](https://github.com/fxceless/victory-fork/commit/c36971b213770e48f513f0d6e3ced3f1f65a32b2).

In our earlier PR995 experiment (`438872d` plus three experimental patches,
RTX 5090, Xvfb, release, one run each), setting `MIN_KB=64`, `CARD_BUFFERS_MB=1536`,
`BACK_QUIET=1` and `DYNAMIC_SLOTS=524288` together gave 4.91 FPS. With nothing
set, it gave 4.34 FPS. That's +13 %, with copy-back down from 84 to 61 ms a
frame. The switched run paid for the cold shader translations. Neither run
had an Xid.

One caveat: in 4 of 17 of our runs the cell came up dark with very few shader
lookups. This happened with the switch on and off, and we haven't found the
cause. Check the picture when you compare.

## Wineserver: memory views in a tree

[CachyOS/wine-cachyos PR #50](https://github.com/CachyOS/wine-cachyos/pull/50)
(ajsem1337, based on a version by neonbyte1) keeps each process's memory views
in a red-black tree by base address. It is the same idea as the indexed GE11-14
wineserver used here. It touches only `server/file.h`, `server/mapping.c`,
`server/process.c` and `server/process.h` (+56/-21), with no protocol change.
It applied cleanly to Valve's Proton 11.0 wine (`e60892cc`).

On Proton 11.0 with only the wineserver swapped:

| | Stock wineserver | View tree |
|---|---:|---:|
| Loading the world from a save | 118–131 s | 69 s |
| Cutscene right after loading | 2.3–2.4 FPS | 4.9 FPS |
| Steady FPS in the cell | unchanged | unchanged |

An unpatched wineserver built with the same compiler loaded like stock, so the
gain comes from the patch. Wine reuses a running wineserver for the same
prefix, so stop the old one before you switch.

## The title's breakpoint on a new game (`0x5149aec`)

The opening cutscene's prestreaming fills the title's 45,000-entry instance
list. The title then stops at its own breakpoint (`int3`, exit
`0x80000003`), usually 6–10 minutes into a new game. This matches the runs
here that stopped at `0x5149aec` with 45,000 of 45,000 instance slots
occupied. It is tracked in
[Marvisak/VIctory#698](https://github.com/Marvisak/VIctory/issues/698).
Starting from a save made after the intro avoids it. All our measured runs
load such a save.

## Picture fixes we use

- **`VICTORY_TEXTURE_BACK=1`**
  ([fork PR 999](https://github.com/fxceless/victory-fork/pull/999),
  cxy0072019-sketch): in Chapter 1, the boat no longer sinks. Others confirm
  this on a 5080, a 3080 and an RX 9070 XT on Linux. The pre-migration
  `linux-edge` tree has it, but the PR995 renderer does not.
- **Ambient stand-in** ([fork PR 917](https://github.com/fxceless/victory-fork/pull/917)
  plus Belobug's SHADE/TINT commits): `VICTORY_AMBIENT=0.3`,
  `VICTORY_AMBIENT_TINT=1,0.6,0.4` and `VICTORY_AMBIENT_SHADE=1.5,0.8,0.1,0.45`
  came closest to PS5 reference footage at the cell window. A value of 0.6
  washed the cell out. The PR995 renderer does not include #917.
- From #995's own list of settings, `VICTORY_VIEW_GUARD=0` and
  `VICTORY_COMMIT_FLOOR_MB=6144` hid the cell's left wall and the window's wire
  mesh on our machine. The preset here does not set them.

## Frame-rate settings on fork main (RTX 5090, our cell save)

These are release builds at warm cache. Each value is the FPS over world frames
300–600 of the same save. The base averages three runs at 5.68 FPS (sd 3.3 %).
Any single run within ±6.3 % of the base is noise.

| On top of our base | FPS | vs base |
|---|---:|---:|
| `VICTORY_CARD_HASH_WIDE=1` ([fork PR 1014](https://github.com/fxceless/victory-fork/pull/1014)) | 6.06 | +6.7 % |
| `VICTORY_TABLE_STAGES=1` (fork main `5d659e0c8`) | 6.08 | +7.1 % |
| Both, plus `VICTORY_CARD_BUFFERS_MB=1536`, `VICTORY_DYNAMIC_SLOTS=524288` and `VICTORY_BACK_QUIET=1` | **6.58** | **+15.8 %** |
| `VICTORY_LOOKUP_CACHE`, `HEAP_PASS_MARKED`, `CARD_SYNC_BATCH`, `STEP_INDEX` or `FILLS_PLAIN` (one at a time) | 5.59–5.88 | noise |

With the combined set, the submitting thread spends 19 ms in heap sync instead
of 34 ms, and 23 ms instead of 31–36 ms on write-watch queries. The rest of
each frame now goes to waiting for the GPU.

The combined run included two experimental renderer changes. The copy-back
experiment can clear a concurrent title write from write-watch before the
renderer sees it. The enlarged dynamic ring can let a direct shader read
select a temporary view. These switches are recorded to describe our
measurements. The source build ships the small-buffer threshold.

`CARD_HASH_WIDE` (344 lines) and `TABLE_STAGES` (226 lines) do not apply to the
candidate without larger rework. They are listed here in case you rebase onto
fork main.

## Face rendering on the separate fork build

OctoAlgo reported visible facial skin, eyes and a closed jaw in the diner and
bar close-ups on a separate [fxceless build at `46bf535`](https://github.com/fxceless/victory-fork/tree/46bf5351707b1697c04e5d871c30973ebaa3e973).
Their build also included full [#854/#993 shader pairing](https://github.com/fxceless/victory-fork/pull/993),
[#938](https://github.com/fxceless/victory-fork/pull/938),
[#1002](https://github.com/fxceless/victory-fork/pull/1002) and
[#927](https://github.com/fxceless/victory-fork/pull/927). Face recovery was
observed with the combined renderer. The contributor did not bisect it.
[Original test report](https://github.com/TomTomAIbot/LinVxtory/pull/1)
