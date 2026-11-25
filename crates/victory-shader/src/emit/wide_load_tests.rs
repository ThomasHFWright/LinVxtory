//! Actual raw_dwords emission executed on the CPU, before and after module optimization.
use super::*;
use crate::emit::bitcode::{Module, opt};
use crate::native::ComputeIo;

fn emitted(counts: &[u32], wide: bool, repeat: bool) -> (Module, usize) {
    struct Restore(Option<bool>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_WIDE_LOADS.set(self.0);
        }
    }
    let _restore = Restore(TEST_WIDE_LOADS.replace(Some(wide)));
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [1, 1, 1],
        }),
        code: 0xBF81_0000u32.to_le_bytes().to_vec(),
    };
    let p = crate::ir::build(&native.code, false, native.vgprs).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = Io::default();
    let parts = Parts {
        native: &native,
        p: &p,
        st: &st,
        res: &res,
        io: &io,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
        view_marks: false,
    };
    let bad = HashMap::new();
    let mut lower = Lower::new(&parts, &bad);
    let ty = lower.s.m.func_ty(lower.s.t.i32, vec![]);
    let input = lower.s.m.function("test.address", ty, None, false);
    let addr = lower.s.m.call(lower.cur, input, vec![]);
    let handle = lower.s.m.undef(lower.s.t.handle);
    let mut words = Vec::new();
    let mut offset = 0;
    for &count in counts {
        let off = lower.s.m.i32(offset);
        let at = lower.s.m.bin(lower.cur, Bin::Add, addr, off);
        words.extend(lower.raw_dwords(handle, at, count));
        if repeat {
            // No intervening observation calls: LOADS/CSE may reuse these reads.
            words.extend(lower.raw_dwords(handle, at, count));
        }
        offset += 4 * count;
    }
    let ty = lower.s.m.func_ty(lower.s.t.void, vec![lower.s.t.i32]);
    let sink = lower.s.m.function("test.result", ty, None, false);
    for word in words {
        lower.s.m.call(lower.cur, sink, vec![word]);
    }
    lower.s.m.terminate(lower.cur, Term::Ret);
    (lower.s.m, lower.s.entry)
}

/// A sparse synthetic view covering up to 4 GiB, without allocating it.
fn read(byte: u64, length: u64) -> u32 {
    if byte + 4 > length {
        return 0;
    }
    // Include float edge encodings: loads must transport bits without conversion.
    const WORDS: [u32; 8] = [
        0,
        0x8000_0000,
        0x7F80_0000,
        0x7FC0_1234,
        1,
        0xFFFF_FFFF,
        0x1234_5678,
        0x89AB_CDEF,
    ];
    WORDS[(byte / 4 % 8) as usize] ^ (byte as u32 & 0xFFFF_FFE0)
}

#[test]
fn every_raw_word_matches_at_view_ends_and_address_wrap() {
    for count in [1, 2, 3, 4, 8, 16] {
        let (mut scalar, entry) = emitted(&[count], false, false);
        let (mut wide, wide_entry) = emitted(&[count], true, false);
        let last_fitting = u32::MAX - 4 * (count - 1) - 3;
        let addresses = [
            0,
            4,
            12,
            32,
            0xFFFC,
            last_fitting,
            last_fitting.wrapping_add(4),
            0xFFFF_FFC0,
            0xFFFF_FFF0,
            0xFFFF_FFF8,
            0xFFFF_FFFC,
        ];
        for optimized in [false, true] {
            if optimized {
                scalar.optimise(opt::MODULE);
                wide.optimise(opt::MODULE);
            }
            for address in addresses {
                for length in [
                    0,
                    1,
                    3,
                    4,
                    7,
                    8,
                    12,
                    15,
                    16,
                    31,
                    32,
                    63,
                    64,
                    65,
                    128,
                    u64::from(address),
                    u64::from(address) + 4,
                    1u64 << 32,
                ] {
                    let expected: Vec<_> = (0..count)
                        .map(|k| read(u64::from(address.wrapping_add(4 * k)), length))
                        .collect();
                    let narrow = scalar.trace_raw_loads(entry, address, |at| read(at, length));
                    let batched = wide.trace_raw_loads(wide_entry, address, |at| read(at, length));
                    assert_eq!(
                        narrow.words, expected,
                        "scalar count={count} address={address:#x} length={length} optimized={optimized}"
                    );
                    assert_eq!(
                        batched.words, expected,
                        "wide count={count} address={address:#x} length={length} optimized={optimized}"
                    );
                    let wraps = address > u32::MAX - 4 * (count - 1);
                    assert_eq!(
                        batched.loads.len(),
                        if wraps { count } else { count.div_ceil(4) } as usize
                    );
                    assert_eq!(narrow.loads.len(), count as usize);
                }
            }
        }
    }
}

#[test]
fn fitting_loads_reduce_twenty_eight_calls_to_seven() {
    for wide in [false, true] {
        let (mut m, entry) = emitted(&[4, 8, 16], wide, false);
        for optimized in [false, true] {
            if optimized {
                m.optimise(opt::MODULE);
            }
            let trace = m.trace_raw_loads(entry, 0, |at| read(at, 112));
            assert_eq!(
                trace.words,
                (0..28).map(|k| read(k * 4, 112)).collect::<Vec<_>>()
            );
            assert_eq!(trace.loads.len(), if wide { 7 } else { 28 });
        }
    }
}

#[test]
fn repeated_reads_keep_every_result_through_load_cse() {
    for count in [1, 2, 3, 4, 8, 16] {
        for wide in [false, true] {
            let (mut m, entry) = emitted(&[count], wide, true);
            let addresses = [0, 4, 0xFFFF_FFF8, 0xFFFF_FFFC];
            let before: Vec<_> = addresses
                .iter()
                .map(|&address| m.trace_raw_loads(entry, address, |at| read(at, 1u64 << 32)))
                .collect();
            m.optimise(opt::MODULE);
            for (&address, before) in addresses.iter().zip(before) {
                let after = m.trace_raw_loads(entry, address, |at| read(at, 1u64 << 32));
                let once: Vec<_> = (0..count)
                    .map(|k| read(u64::from(address.wrapping_add(4 * k)), 1u64 << 32))
                    .collect();
                let expected = once.repeat(2);
                assert_eq!(before.words, expected);
                assert_eq!(
                    after.words, expected,
                    "count={count} wide={wide} address={address:#x}"
                );
                if !wide || count == 1 {
                    assert_eq!(before.loads.len(), 2 * count as usize);
                    assert_eq!(
                        after.loads.len(),
                        count as usize,
                        "scalar duplicate loads must exercise CSE"
                    );
                }
            }
        }
    }
}
