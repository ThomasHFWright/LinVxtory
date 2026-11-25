//! CPU-only three-way copyback regressions, independent of the SIMD width.

use super::{scalar, wide};

type Merge = fn(&mut [u8], &mut [u8], &[u8]) -> bool;

fn implementations() -> [(&'static str, Merge); 2] {
    [("scalar", scalar), ("wide", wide)]
}

// Compute the final image from untouched inputs, independently of either
// implementation: GPU changes win; otherwise retain the CPU's current byte.
fn reference(title: &[u8], host: &[u8], before: &[u8]) -> (Vec<u8>, bool) {
    assert_eq!(title.len(), host.len());
    assert_eq!(title.len(), before.len());
    let result: Vec<_> = title
        .iter()
        .zip(host)
        .zip(before)
        .map(|((&cpu, &gpu), &old)| if gpu == old { cpu } else { gpu })
        .collect();
    let changed = result.as_slice() != title;
    (result, changed)
}

fn padded(bytes: &[u8], offset: usize, guard: u8) -> (Vec<u8>, usize) {
    let start = 64 + offset;
    let mut buffer = vec![guard; start + bytes.len() + 64];
    buffer[start..start + bytes.len()].copy_from_slice(bytes);
    (buffer, start)
}

fn check(title: &[u8], host: &[u8], before: &[u8], offsets: [usize; 3]) {
    let len = title.len();
    let (want, changed) = reference(title, host, before);
    for (name, merge) in implementations() {
        let (mut cpu, t) = padded(title, offsets[0], 0xa5);
        let (mut gpu, h) = padded(host, offsets[1], 0x5a);
        let (old, b) = padded(before, offsets[2], 0xc3);
        let old_snapshot = old.clone();
        let mut want_cpu = cpu.clone();
        let mut want_gpu = gpu.clone();
        want_cpu[t..t + len].copy_from_slice(&want);
        want_gpu[h..h + len].copy_from_slice(&want);
        assert_eq!(
            merge(&mut cpu[t..t + len], &mut gpu[h..h + len], &old[b..b + len]),
            changed,
            "{name}: changed flag, len={len}, offsets={offsets:?}"
        );
        assert_eq!(
            cpu, want_cpu,
            "{name}: title and guards, len={len}, offsets={offsets:?}"
        );
        assert_eq!(
            gpu, want_gpu,
            "{name}: host and guards, len={len}, offsets={offsets:?}"
        );
        assert_eq!(old, old_snapshot, "{name}: before must remain read-only");
        // Once synchronized, even a GPU value differing from the old
        // snapshot does not count as another title change.
        assert!(
            !merge(&mut cpu[t..t + len], &mut gpu[h..h + len], &old[b..b + len]),
            "{name}: repeated merge, len={len}, offsets={offsets:?}"
        );
        assert_eq!(cpu, want_cpu);
        assert_eq!(gpu, want_gpu);
        assert_eq!(old, old_snapshot);
    }
}

fn random_byte(state: &mut u64) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state >> 32) as u8
}

fn inputs(len: usize, seed: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut state = seed;
    let mut title = Vec::with_capacity(len);
    let mut host = Vec::with_capacity(len);
    let mut before = Vec::with_capacity(len);
    for _ in 0..len {
        let old = random_byte(&mut state);
        let (cpu, gpu) = match random_byte(&mut state) % 6 {
            0 => (old, old),               // unchanged
            1 => (old ^ 0x5a, old),        // CPU only
            2 => (old, old ^ 0xa5),        // GPU only
            3 => (old ^ 1, old ^ 1),       // both wrote the same value
            4 => (old ^ 0x80, old ^ 0x40), // conflicting writes
            _ => (random_byte(&mut state), random_byte(&mut state)),
        };
        title.push(cpu);
        host.push(gpu);
        before.push(old);
    }
    (title, host, before)
}

#[test]
fn unchanged_cpu_only_gpu_only_and_conflicting_writes() {
    // Whole equal vectors and whole changed vectors exercise SIMD fast paths,
    // not just the mixed lanes in the randomized cases.
    for len in [0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 257, 65_536] {
        for (cpu, gpu, old, expected_changed) in [
            (0x80, 0x80, 0x80, false), // all identical
            (0xff, 0x80, 0x80, false), // CPU-only change must reach host
            (0x80, 0xff, 0x80, true),  // GPU-only change
            (0xff, 0xff, 0x80, false), // same CPU/GPU update
            (0x01, 0xff, 0x80, true),  // GPU wins conflict
            (0xff, 0x00, 0x80, true),  // zero is a real GPU update
        ] {
            let (title, host, before) = (vec![cpu; len], vec![gpu; len], vec![old; len]);
            assert_eq!(
                reference(&title, &host, &before).1,
                len != 0 && expected_changed
            );
            check(&title, &host, &before, [1, 3, 7]);
        }
    }
}

#[test]
fn deterministic_mixed_changes_cover_every_short_length_and_large_boundaries() {
    let lengths = (0..=257).chain([
        511, 512, 513, 1023, 1024, 1025, 4095, 4096, 4097, 65_535, 65_536, 65_537,
    ]);
    for len in lengths {
        let (title, host, before) = inputs(len, 0x24d5_89ab_cdef_1731 ^ len as u64);
        for offsets in [[0, 0, 0], [1, 3, 5], [15, 31, 63], [63, 1, 17]] {
            check(&title, &host, &before, offsets);
        }
    }
}

#[test]
fn every_alignment_and_tail_preserves_cpu_bytes_beside_one_gpu_write() {
    // A lane whose GPU byte changed must not copy its unchanged neighbours
    // over CPU-only writes. Walk the sole GPU change across vector boundaries.
    for len in [15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 257] {
        for offset in 0..64 {
            let before = vec![0x11; len];
            let title = vec![0x55; len];
            let mut host = before.clone();
            let at = offset % len;
            host[at] = 0xaa;
            check(
                &title,
                &host,
                &before,
                [offset, (offset + 17) % 64, (offset + 39) % 64],
            );
        }
        // In particular, the final byte must be processed, not rounded down.
        let before = vec![0; len];
        let title = before.clone();
        let mut host = before.clone();
        host[len - 1] = 1;
        check(&title, &host, &before, [3, 1, 5]);
    }
}

#[test]
fn disjoint_slices_in_one_allocation_work_in_every_address_order() {
    // Backing allocations may be shared, but overlapping nonempty mutable
    // slices would be invalid Rust. Exercise all valid relative address
    // orders without manufacturing aliased references or using unsafe code.
    for len in [0, 1, 16, 17, 32, 33, 64, 65, 257, 65_536] {
        let (title, host, before) = inputs(len, 0x681e_19bf_4a73_d025 ^ len as u64);
        let (want, changed) = reference(&title, &host, &before);
        let stride = len + 128;
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            for (name, merge) in implementations() {
                let mut storage = vec![0xbd; 3 * stride];
                for (region, bytes) in order.into_iter().zip([&title, &host, &before]) {
                    let start = region * stride + 1;
                    storage[start..start + len].copy_from_slice(bytes);
                }
                let mut expected = storage.clone();
                for region in [order[0], order[1]] {
                    let start = region * stride + 1;
                    expected[start..start + len].copy_from_slice(&want);
                }
                let (a, tail) = storage.split_at_mut(stride);
                let (b, c) = tail.split_at_mut(stride);
                let (t, h, old) = match order {
                    [0, 1, 2] => (a, b, c),
                    [0, 2, 1] => (a, c, b),
                    [1, 0, 2] => (b, a, c),
                    [1, 2, 0] => (b, c, a),
                    [2, 0, 1] => (c, a, b),
                    [2, 1, 0] => (c, b, a),
                    _ => unreachable!(),
                };
                assert_eq!(
                    merge(&mut t[1..1 + len], &mut h[1..1 + len], &old[1..1 + len]),
                    changed,
                    "{name}: len={len}, order={order:?}"
                );
                // Includes all untouched gaps and the complete before region.
                assert_eq!(storage, expected, "{name}: len={len}, order={order:?}");
            }
        }
    }
}
