//! Manual CPU throughput comparison on fixed VirtualAlloc mappings, under the
//! same runtime as the title. Run the ignored `guarded_vs_kernel` test alone.
//! No GPU, remapping or game input. Readback and canaries are checked outside
//! timing; kernel delivery status is inherited from write_mapped_with.

use super::{CANARY, PAGE, Region, guarded_write, write_mapped_with};
use std::hint::black_box;
use std::time::Instant;

const SLICE: usize = 64 * 1024;
const COPIES: usize = 1024; // 64 MiB per timed leg, identical for both variants.
const CYCLES: usize = 12; // ABBA gives 24 samples per path.

fn batch(source: &Region, target: &Region, slots: usize, guarded: bool, copies: usize) -> f64 {
    let bytes = slots * SLICE;
    let from = source.base.wrapping_add(PAGE);
    let to = target.base.wrapping_add(PAGE);
    // SAFETY: both regions remain committed, are disjoint and are used only
    // by this thread. Reset outside timing so a no-op cannot pass readback.
    unsafe {
        for i in 0..bytes {
            to.add(i).write(!from.add(i).read());
        }
    }
    let mut faults = 0usize;
    let started = Instant::now();
    for i in 0..copies {
        let offset = (i % slots) * SLICE;
        write_mapped_with(
            black_box(to.wrapping_add(offset)),
            black_box(from.wrapping_add(offset)),
            SLICE,
            true, // Explicit NtWriteVirtualMemory policy, independent of env.
            |dst, src, len| {
                if !guarded {
                    return false;
                }
                let complete = guarded_write(dst, src, len);
                faults += usize::from(!complete);
                complete
            },
        );
    }
    let ns_per_copy = started.elapsed().as_secs_f64() * 1e9 / copies as f64;
    assert_eq!(
        faults, 0,
        "a guarded fallback invalidates this timing sample"
    );
    // SAFETY: all allocations are still committed and no writes are in flight.
    unsafe {
        assert!(
            std::slice::from_raw_parts(to, bytes) == std::slice::from_raw_parts(from, bytes),
            "complete payload readback"
        );
        for region in [source, target] {
            let prefix = std::slice::from_raw_parts(region.base, PAGE);
            let suffix = std::slice::from_raw_parts(region.base.add(PAGE + bytes), PAGE);
            assert!(
                prefix.iter().chain(suffix).all(|&b| b == CANARY),
                "outside-copy canaries"
            );
        }
    }
    ns_per_copy
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_unstable_by(f64::total_cmp);
    let mid = values.len() / 2;
    (values[mid - 1] + values[mid]) / 2.0 // Always 2 * CYCLES samples.
}

#[test]
#[ignore = "manual offline Wine CPU throughput benchmark; no GPU"]
fn guarded_vs_kernel() {
    eprintln!(
        "guarded-write benchmark: VirtualAlloc only; 64 KiB/copy; {COPIES} copies/leg; \
         {CYCLES} ABBA cycles; same addresses/pattern; reset/readback outside timing"
    );
    for slots in [1, 256] {
        let bytes = slots * SLICE;
        let pages = bytes / PAGE + 2;
        let source = Region::new(pages, CANARY);
        let target = Region::new(pages, CANARY);
        // SAFETY: the payload is inside this fresh, writable allocation.
        // Pattern distinguishes positions and slots; prefix/suffix stay canaries.
        unsafe {
            for i in 0..bytes {
                source
                    .base
                    .add(PAGE + i)
                    .write((i.wrapping_mul(37) ^ (i >> 8) ^ (i >> 16)) as u8);
            }
        }
        // Touch every slot and install the handler before collecting times.
        for guarded in [true, false] {
            batch(&source, &target, slots, guarded, slots.max(64));
        }
        let mut guarded_ns = Vec::with_capacity(2 * CYCLES);
        let mut kernel_ns = Vec::with_capacity(2 * CYCLES);
        for _ in 0..CYCLES {
            for guarded in [true, false, false, true] {
                let ns = batch(&source, &target, slots, guarded, COPIES);
                if guarded {
                    guarded_ns.push(ns);
                } else {
                    kernel_ns.push(ns);
                }
            }
        }
        let a = median(&mut guarded_ns);
        let b = median(&mut kernel_ns);
        let gib_s = |ns: f64| SLICE as f64 * 1e9 / ns / (1u64 << 30) as f64;
        eprintln!(
            "payload_bytes={bytes} slots={slots} samples_per_path={} \
             guarded_median_ns_per_copy={a:.1} kernel_median_ns_per_copy={b:.1} \
             guarded_GiB_s={:.3} kernel_GiB_s={:.3} kernel_over_guarded={:.3} \
             guarded_min_max_ns={:.1}/{:.1} kernel_min_max_ns={:.1}/{:.1}",
            guarded_ns.len(),
            gib_s(a),
            gib_s(b),
            b / a,
            guarded_ns[0],
            guarded_ns[guarded_ns.len() - 1],
            kernel_ns[0],
            kernel_ns[kernel_ns.len() - 1],
        );
    }
}
