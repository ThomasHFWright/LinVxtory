//! Consecutive upload fills, adapted from tunczzz's victory-fork PR995.
//! Only independent host destinations run together; every other step is a fence.

type Fill = (usize, usize, usize); // destination, source, bytes
const MAX_FILLS: usize = 256;
// Scoped-worker setup lost at 512 KiB in the local CPU copy benchmark.
const MIN_BYTES: usize = 4 * 1024 * 1024;

#[cfg(windows)]
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_WIDE_FILLS").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                "independent consecutive upload fills run in parallel (VICTORY_WIDE_FILLS=1)"
            );
        }
        on
    })
}

/// Ends at the first non-fill, including callbacks, refreshes and title writes.
/// Joins every worker before returning to the submission's next step.
pub(crate) fn run_prefix<T>(
    steps: &[T],
    fill: impl Fn(&T) -> Option<Fill>,
    copy: impl Fn(Fill) + Sync,
) -> (usize, usize) {
    let fills: Vec<_> = steps.iter().take(MAX_FILLS).map_while(fill).collect();
    let bytes = fills.iter().fold(0usize, |n, f| n.saturating_add(f.2));
    let threads = if fills.len() < 8 || bytes < MIN_BYTES {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get() / 4)
            .clamp(1, 4)
    };
    run(&fills, bytes, threads, copy);
    (fills.len(), bytes)
}

/// Reject write/write and read/write dependencies, including self-overlap.
/// Upload destinations are host allocations, never alternate title mappings.
fn independent(fills: &[Fill]) -> bool {
    let mut destinations = Vec::with_capacity(fills.len());
    for &(dst, src, len) in fills {
        let (Some(end), Some(_)) = (dst.checked_add(len), src.checked_add(len)) else {
            return false;
        };
        if len != 0 {
            destinations.push((dst, end));
        }
    }
    destinations.sort_unstable();
    if destinations.windows(2).any(|w| w[0].1 > w[1].0) {
        return false;
    }
    for &(_, src, len) in fills {
        if len == 0 {
            continue;
        }
        let i = destinations.partition_point(|&(_, end)| end <= src);
        if destinations
            .get(i)
            .is_some_and(|&(start, _)| start < src + len)
        {
            return false;
        }
    }
    true
}

fn run(fills: &[Fill], bytes: usize, threads: usize, copy: impl Fn(Fill) + Sync) {
    if threads < 2 || fills.len() < 8 || bytes < MIN_BYTES || !independent(fills) {
        fills.iter().copied().for_each(copy);
        return;
    }
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let next = AtomicUsize::new(0);
    let work = || {
        loop {
            let i = next.fetch_add(1, Relaxed);
            let Some(&fill) = fills.get(i) else { break };
            copy(fill);
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..threads {
            scope.spawn(&work);
        }
        work();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependencies_include_sources_adjacency_zero_and_overflow() {
        assert!(independent(&[(100, 0, 10), (110, 0, 10)]));
        assert!(!independent(&[(100, 0, 11), (110, 20, 10)]));
        assert!(!independent(&[(100, 0, 10), (200, 105, 10)]));
        assert!(!independent(&[(200, 105, 10), (100, 0, 10)]));
        assert!(!independent(&[(100, 100, 10)]));
        assert!(independent(&[(100, 100, 0), (100, 0, 10)]));
        assert!(!independent(&[(usize::MAX, 0, 1)]));
        assert!(!independent(&[(0, usize::MAX, 1)]));
    }

    #[test]
    fn disjoint_fills_match_serial_bytes_and_finish_before_return() {
        let source: Vec<u8> = (0..MIN_BYTES * 2)
            .map(|i| (i.wrapping_mul(37) >> 8) as u8)
            .collect();
        let mut result = vec![0xa5u8; source.len()];
        let width = source.len() / 16;
        let fills: Vec<_> = (0..16)
            .map(|i| {
                (
                    result.as_mut_ptr() as usize + i * width,
                    source.as_ptr() as usize + i * width,
                    width,
                )
            })
            .collect();
        assert!(independent(&fills));
        run(&fills, source.len(), 4, |(dst, src, len)| {
            // SAFETY: disjoint live destinations, immutable source; run joins.
            unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
        });
        assert_eq!(result, source);
    }

    #[test]
    fn dependent_fills_keep_recorded_order() {
        for reverse in [false, true] {
            let mut bytes: Vec<u8> = (0..MIN_BYTES * 2).map(|i| (i / 8192) as u8).collect();
            let mut expected = bytes.clone();
            let mut offsets: Vec<_> = (0..8)
                .map(|i| ((i + 1) * 32768, i * 32768, 32768))
                .collect();
            if reverse {
                offsets.reverse();
            }
            for &(dst, src, len) in &offsets {
                expected.copy_within(src..src + len, dst);
            }
            let base = bytes.as_mut_ptr() as usize;
            let fills: Vec<_> = offsets
                .iter()
                .map(|&(dst, src, len)| (base + dst, base + src, len))
                .collect();
            assert!(!independent(&fills));
            run(&fills, MIN_BYTES, 4, |(dst, src, len)| {
                // SAFETY: in-bounds fixture copies; dependencies force serial order.
                unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
            });
            assert_eq!(bytes, expected);
        }
    }

    #[test]
    fn every_non_fill_splits_the_run_and_long_runs_are_bounded() {
        let fill = (100, 0, 1);
        let mut steps = vec![Some(fill); MAX_FILLS + 3];
        steps.extend([None, Some(fill)]);
        let called = std::sync::atomic::AtomicUsize::new(0);
        let copy = |_| {
            called.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        };
        assert_eq!(run_prefix(&steps, |s| *s, copy), (MAX_FILLS, MAX_FILLS));
        assert_eq!(run_prefix(&steps[MAX_FILLS..], |s| *s, copy), (3, 3));
        assert_eq!(run_prefix(&steps[MAX_FILLS + 3..], |s| *s, copy), (0, 0));
        assert_eq!(
            called.load(std::sync::atomic::Ordering::Relaxed),
            MAX_FILLS + 3
        );
        assert_eq!(run_prefix(&steps[MAX_FILLS + 4..], |s| *s, copy), (1, 1));
    }
    #[test]
    fn callback_between_fill_runs_observes_old_then_new_source() {
        let mut source = vec![7u8; MIN_BYTES];
        let mut first = vec![0u8; source.len()];
        let mut second = vec![0u8; source.len()];
        let width = source.len() / 16;
        let make = |dst: usize| {
            (0..16)
                .map(|i| Some((dst + i * width, source.as_ptr() as usize + i * width, width)))
                .collect::<Vec<_>>()
        };
        let mut steps = make(first.as_mut_ptr() as usize);
        steps.push(None);
        steps.extend(make(second.as_mut_ptr() as usize));
        let copy = |(dst, src, len): Fill| {
            // SAFETY: each prefix has disjoint live destinations; source changes only after join.
            unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
        };
        assert_eq!(run_prefix(&steps, |s| *s, copy), (16, MIN_BYTES));
        assert_eq!(first, source);
        assert!(second.iter().all(|&b| b == 0));
        source.fill(9); // The intervening callback must finish before the next fill run.
        assert_eq!(run_prefix(&steps[17..], |s| *s, copy), (16, MIN_BYTES));
        assert_eq!(second, source);
        assert!(first.iter().all(|&b| b == 7));
    }

    #[test]
    fn overlapping_destinations_keep_the_last_write_and_untouched_bytes() {
        let source: Vec<u8> = (0..MIN_BYTES).map(|i| (i / 4096) as u8).collect();
        let mut result = vec![0xa5u8; MIN_BYTES];
        let mut expected = result.clone();
        let width = MIN_BYTES / 8;
        let offsets: Vec<_> = (0..8)
            .map(|i| (width / 2 + i * width / 2, i * width, width))
            .collect();
        for &(dst, src, len) in &offsets {
            expected[dst..dst + len].copy_from_slice(&source[src..src + len]);
        }
        let fills: Vec<_> = offsets
            .iter()
            .map(|&(dst, src, len)| {
                (
                    result.as_mut_ptr() as usize + dst,
                    source.as_ptr() as usize + src,
                    len,
                )
            })
            .collect();
        assert!(!independent(&fills));
        run(&fills, MIN_BYTES, 4, |(dst, src, len)| {
            // SAFETY: source is immutable and destinations are in bounds; overlap forces serial order.
            unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
        });
        assert_eq!(result, expected);
    }
}
