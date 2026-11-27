# Large BC texture detiling test

The original proposal sped up large compressed textures and slowed
small resources. On the Linux CPU harness, 1021×767 RGBA8 tiling lost
46.5% in release. Median throughput on the 15×9 fixtures fell 27–85%.

The revised candidate preserves the existing tile and general copy
loops. It tries a fixed-width detile path on supported 8- and 16-byte BC
blocks when a subresource has at least 2,048 blocks. Unsupported layouts
and smaller resources use the general loop.
[SlaktarnVI source](https://github.com/Slaktarn700/SlaktarnVI/blob/42b222681c7bc4d9572b8021b26055effd436ad3/crates/victory-formats/src/texture.rs#L660-L999)

## Results

Locked Windows MSVC `diag` builds ran under Wine 11.22 without a graphics
device on a Ryzen 7 9800X3D. The candidate passed 43 crate tests and 320
reference checks across BC mips and slices. A 12-pair alternating repeat
measured 20.6% faster 1021×767 BC1 detiling and 21.6% faster BC7 detiling.
The earlier six pairs also showed gains. The 15×9 and 63×35 detile
medians varied within 4.4% of the baseline. Large BC1 tile throughput
varied from −11.9% in the first run to +3.4% in the repeat, with the
same tile source loop in both builds. Title streaming and gameplay
frame times are pending.

## Reproduce the CPU comparison

Create separate baseline and candidate renderer directories with
`sh build.sh --sr --control <renderer-directory>`. On revisions where
SR is the default and `--sr` is unavailable, use `sh build.sh --control
<renderer-directory>`. Both directories start at the original SR tree
`3d033cbd633757d2fd15f97a4323d566e8a043ba`. Apply the overlay manually
in the candidate directory.

```sh
git apply --index "$kit/docs/7900/texture-copy-rows.patch.txt"
test "$(git write-tree)" = a00b10c0012f75f9e53d11c3a22f8128f54f7e5b
cargo test --locked --offline -p victory-formats --lib
```

Set `kit` to the LinVxtory checkout. Copy the candidate's included
`crates/victory-formats/examples/texture_copy_bench.rs` into the
disposable control directory. Run the same example in `diag` on both
trees and alternate the order. For a Windows comparison build with
`--target x86_64-pc-windows-msvc` and run the resulting binaries under Wine.

## Apply to other locked renderer trees

The same manual application was verified against these source trees.
The CPU timings above use the original SR control pair.

| Renderer | Base tree | Tree after applying the overlay |
| --- | --- | --- |
| Original RR control (`--rr --control`) | `7a1a47ccb01b0b13afee08c4b715fbc2793a7987` | `3a5b4f5ccae92227726882aaacc43d0d7e891e32` |
| 4K SR (`--sr` on RR-default revisions) | `1ec8cdd4791d95b60ce2457d71d7b96c70b3b700` | `3e3e0067c896d6b90a606b2e248b5653e018e8e4` |
| Default 4K RR (`--rr` on RR-default revisions) | `d6adab8c7687f8bc1ec4e794168f5490b864db78` | `97024505d7c4efc71514776773d23b4424e42e1a` |
