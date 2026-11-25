//! CPU-only Win32 memory tests: no host device, shaders or title input.

use super::write_mapped_with;
use crate::plaincopy::guarded_write;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};

const PAGE: usize = 4096;
const CANARY: u8 = 0xCC;

struct Region {
    base: *mut u8,
    len: usize,
}

impl Region {
    fn new(pages: usize, value: u8) -> Self {
        let len = pages * PAGE;
        // SAFETY: a new reservation owned solely by this test.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as *mut u8;
        assert!(!base.is_null());
        // SAFETY: the whole allocation is committed and writable.
        unsafe { base.write_bytes(value, len) };
        Self { base, len }
    }

    fn source(pages: usize) -> Self {
        let region = Self::new(pages, 0);
        // SAFETY: newly committed memory, with no concurrent user.
        for (at, byte) in unsafe { std::slice::from_raw_parts_mut(region.base, region.len) }
            .iter_mut()
            .enumerate()
        {
            *byte = pattern(at);
        }
        region
    }

    fn hole(&self, page: usize) {
        assert!((page + 1) * PAGE <= self.len);
        // SAFETY: one whole page of this test's reservation; no live slices.
        assert_ne!(
            unsafe {
                VirtualFree(
                    self.base.wrapping_add(page * PAGE).cast(),
                    PAGE,
                    MEM_DECOMMIT,
                )
            },
            0
        );
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: no thread or slice still uses this owned reservation.
        unsafe { VirtualFree(self.base.cast(), 0, MEM_RELEASE) };
    }
}

fn pattern(at: usize) -> u8 {
    (at * 7 + at / PAGE * 17 + 3) as u8
}

#[test]
fn guarded_write_preserves_bytes_outside_unaligned_and_empty_copies() {
    write_mapped_with(
        std::ptr::null_mut(),
        std::ptr::null(),
        0,
        true,
        |_, _, _| panic!("an empty copy must not attempt a memory access"),
    );
    for direct in [false, true] {
        for len in [1, 17, 255, PAGE + 31] {
            let source = Region::source(2);
            let target = Region::new(2, CANARY);
            write_mapped_with(
                target.base.wrapping_add(41),
                source.base.wrapping_add(73),
                len,
                direct,
                |to, from, bytes| {
                    let copied = guarded_write(to, from, bytes);
                    assert!(copied);
                    copied
                },
            );
            // SAFETY: all target pages remain committed.
            for (at, &byte) in unsafe { std::slice::from_raw_parts(target.base, target.len) }
                .iter()
                .enumerate()
            {
                let expected = if (41..41 + len).contains(&at) {
                    pattern(at - 41 + 73)
                } else {
                    CANARY
                };
                assert_eq!(byte, expected, "byte {at}, length {len}, direct {direct}");
            }
        }
    }
}

#[test]
fn guarded_write_fallback_copies_around_source_and_destination_holes() {
    // Cross a 64 KiB boundary as well as 4 KiB holes; unaligned endpoints
    // leave byte canaries outside the requested range.
    const PAGES: usize = 18;
    let start = 37;
    let end = PAGES * PAGE - 53;
    for direct in [false, true] {
        for (source_hole, target_hole) in [(Some(8), None), (None, Some(8)), (Some(16), Some(8))] {
            let source = Region::source(PAGES);
            let target = Region::new(PAGES, CANARY);
            if let Some(page) = source_hole {
                source.hole(page);
            }
            if let Some(page) = target_hole {
                target.hole(page);
            }
            let first_hole = source_hole.into_iter().chain(target_hole).min().unwrap();
            let mut attempted = false;
            write_mapped_with(
                target.base.wrapping_add(start),
                source.base.wrapping_add(start),
                end - start,
                direct,
                |to, from, bytes| {
                    attempted = true;
                    let copied = guarded_write(to, from, bytes);
                    assert!(!copied, "must exercise the kernel fallback");
                    // SAFETY: all pages before the first hole remain mapped.
                    let prefix =
                        unsafe { std::slice::from_raw_parts(to, first_hole * PAGE - start) };
                    assert!(
                        prefix
                            .iter()
                            .enumerate()
                            .all(|(i, &b)| b == pattern(start + i))
                    );
                    copied
                },
            );
            assert!(attempted);
            for page in 0..PAGES {
                if target_hole == Some(page) {
                    continue;
                }
                // SAFETY: skip the only uncommitted destination page.
                let bytes =
                    unsafe { std::slice::from_raw_parts(target.base.add(page * PAGE), PAGE) };
                for (offset, &byte) in bytes.iter().enumerate() {
                    let at = page * PAGE + offset;
                    let expected = if (start..end).contains(&at) && source_hole != Some(page) {
                        pattern(at)
                    } else {
                        CANARY
                    };
                    assert_eq!(
                        byte, expected,
                        "byte {at}, direct {direct}, holes {source_hole:?}/{target_hole:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn guarded_write_retries_the_prefix_after_a_concurrent_mapping_change() {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    for direct in [false, true] {
        let source = Region::source(3);
        let target = Region::new(3, CANARY);
        target.hole(1);
        let (start, end) = (37, target.len - 53);
        let (remap_tx, remap_rx) = channel();
        let (done_tx, done_rx) = channel();
        let (base, len) = (target.base as usize, target.len);
        std::thread::scope(|scope| {
            let remapper = scope.spawn(move || {
                remap_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                let _changing = crate::watch::title_memory_remapping();
                // SAFETY: the writer is paused after its guarded attempt;
                // no slices are alive, and the reservation stays owned.
                unsafe {
                    assert_ne!(VirtualFree(base as *mut _, len, MEM_DECOMMIT), 0);
                    assert_eq!(
                        VirtualAlloc(base as *const _, len, MEM_COMMIT, PAGE_READWRITE) as usize,
                        base
                    );
                    (base as *mut u8).write_bytes(CANARY, len);
                }
                done_tx.send(()).unwrap();
            });
            write_mapped_with(
                target.base.wrapping_add(start),
                source.base.wrapping_add(start),
                end - start,
                direct,
                |to, from, bytes| {
                    let copied = guarded_write(to, from, bytes);
                    assert!(!copied);
                    // SAFETY: page zero is still committed; the remapper
                    // has not been released yet.
                    let prefix = unsafe { std::slice::from_raw_parts(to, PAGE - start) };
                    assert!(
                        prefix
                            .iter()
                            .enumerate()
                            .all(|(i, &b)| b == pattern(start + i))
                    );
                    remap_tx.send(()).unwrap();
                    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    copied
                },
            );
            remapper.join().unwrap();
        });
        // SAFETY: remapper joined; all target pages are committed again.
        for (at, &byte) in unsafe { std::slice::from_raw_parts(target.base, target.len) }
            .iter()
            .enumerate()
        {
            let expected = if (start..end).contains(&at) {
                pattern(at)
            } else {
                CANARY
            };
            assert_eq!(
                byte, expected,
                "the full retry must restore even the remapped prefix: {at}, direct {direct}"
            );
        }
    }
}

#[path = "guarded_write_bench.rs"]
mod benchmark;
