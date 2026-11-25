use super::super::{bound_counted, done};
use super::*;
use crate::host::{HostList, host, transition};

struct Readback {
    _buffer: ID3D12Resource,
    data: *mut u8,
    words: usize,
}

impl Readback {
    // Call only after host.execute has waited for the submission.
    fn read(&self) -> Vec<u32> {
        // SAFETY: the live mapped buffer holds `words` words; execution finished.
        unsafe { std::slice::from_raw_parts(self.data.cast::<u32>(), self.words).to_vec() }
    }
}

/// Capture only argument words, never undefined stride padding. Every call
/// gets its own readback, since the baseline reuses its argument scratch.
fn capture(list: &HostList, out: &ID3D12Resource, offset: u64, words: usize) -> Readback {
    let (buffer, data) = host().unwrap().shared_buffer(words as u64 * 4).unwrap();
    // SAFETY: open list, live buffers and a meaningful record within `out`.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            out,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.list
            .CopyBufferRegion(&buffer, 0, out, offset, words as u64 * 4);
        list.list.ResourceBarrier(&[transition(
            out,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        )]);
    }
    Readback {
        _buffer: buffer,
        data,
        words,
    }
}

fn source(records: &[Vec<u32>], stride: u32) -> (ID3D12Resource, *mut u8) {
    let bytes = records.len() * stride as usize;
    let (buffer, data) = host().unwrap().shared_buffer(bytes as u64).unwrap();
    // SAFETY: mapped allocation; initialized before any GPU use.
    unsafe {
        data.write_bytes(0xa5, bytes);
        for (i, record) in records.iter().enumerate() {
            assert!(record.len() * 4 <= stride as usize);
            std::ptr::copy_nonoverlapping(
                record.as_ptr().cast::<u8>(),
                data.add(i * stride as usize),
                record.len() * 4,
            );
        }
    }
    (buffer, data)
}

fn baseline(
    src: &ID3D12Resource,
    offsets: &[u64],
    kind: u32,
    stride: u32,
    words: usize,
) -> Vec<Vec<u32>> {
    let host = host().unwrap();
    let list = host.new_list().unwrap();
    // SAFETY: a live buffer held through execution.
    let gpu = unsafe { src.GetGPUVirtualAddress() };
    let mut reads = Vec::new();
    for &offset in offsets {
        let out = bound_counted(&list, gpu + offset, None, (kind, stride, 1)).unwrap();
        reads.push(capture(&list, &out, 0, words));
        done(&list, &out);
    }
    list.close();
    host.execute(&[&list]);
    reads.iter().map(Readback::read).collect()
}

fn gathered(
    src: &ID3D12Resource,
    offsets: &[u64],
    kind: u32,
    stride: u32,
    words: usize,
) -> Vec<Vec<u32>> {
    let host = host().unwrap();
    let body = host.new_list().unwrap();
    let prelude = host.new_list().unwrap();
    let mut gather = Gather::default();
    let (&fallback, prefix) = offsets.split_last().unwrap();
    let outputs: Vec<_> = prefix
        .iter()
        .map(|&offset| {
            gather
                .push_enabled(&body, src, offset, (kind, stride, 1))
                .unwrap()
        })
        .collect();
    assert!(gather.has_work());
    for pair in outputs.windows(2) {
        // SAFETY: live argument buffers; repeated inputs still need distinct outputs.
        unsafe {
            assert_ne!(
                pair[0].0.GetGPUVirtualAddress() + pair[0].1,
                pair[1].0.GetGPUVirtualAddress() + pair[1].1
            );
        }
    }
    gather.close();
    gather.record(&prelude.list);
    prelude.close();
    let mut reads: Vec<_> = outputs
        .iter()
        .map(|(out, offset)| capture(&body, out, *offset, words))
        .collect();
    // The last small record falls back after the prefix exhausted this same
    // owner's budget: a fresh fallback budget would incorrectly let it run.
    assert!(
        gather
            .push_enabled(&body, src, fallback, (kind, stride, 1))
            .is_none()
    );
    // SAFETY: source remains live through both submissions.
    let gpu = unsafe { src.GetGPUVirtualAddress() };
    let out = bound_counted(&body, gpu + fallback, None, (kind, stride, 1)).unwrap();
    reads.push(capture(&body, &out, 0, words));
    done(&body, &out);
    gather.finish(&body.list);
    body.close();
    // The body owns the budget reset; execute processes deferred work before GPU use.
    host.execute(&[&prelude, &body]);
    let first: Vec<_> = reads.iter().map(Readback::read).collect();
    for read in &reads {
        // SAFETY: execution finished; poison the mapped destinations so stale
        // readback cannot masquerade as a successful second execution.
        unsafe {
            read.data.write_bytes(0xcd, read.words * 4);
        }
    }
    host.execute(&[&prelude, &body]);
    let second: Vec<_> = reads.iter().map(Readback::read).collect();
    assert_eq!(
        second, first,
        "resubmission resets budget and reuses common scratch"
    );
    first
}

#[test]
fn repeated_offsets_and_resubmission_match_the_existing_clamp() {
    if host().is_none() {
        return;
    }
    for kind in [0, 1, 10] {
        let (records, order): (Vec<Vec<u32>>, Vec<usize>) = match kind {
            0 => (
                vec![
                    vec![3 << 20, 16, 11, 22],
                    vec![3 << 20, 16, 33, 44],
                    vec![3, 1, 55, 66],
                    vec![u32::MAX, u32::MAX, 77, 88],
                ],
                [0, 1, 0, 2, 1, 3]
                    .into_iter()
                    .cycle()
                    .take(70)
                    .chain([2])
                    .collect(),
            ),
            1 => (
                vec![
                    vec![3 << 20, 16, 11, (-13i32) as u32, 22],
                    vec![3 << 20, 16, 33, 17, 44],
                    vec![3, 1, 55, (-7i32) as u32, 66],
                    vec![u32::MAX, u32::MAX, 77, 19, 88],
                ],
                [0, 1, 0, 2, 1, 3]
                    .into_iter()
                    .cycle()
                    .take(70)
                    .chain([2])
                    .collect(),
            ),
            _ => (
                vec![
                    vec![u32::MAX, u32::MAX, u32::MAX],
                    vec![70_000, 2, 3],
                    vec![1, 1, 1],
                ],
                (0..70).map(|i| i % 2).chain([2]).collect(),
            ),
        };
        let words = records[0].len();
        for stride in [words as u32 * 4, 32] {
            let (src, data) = source(&records, stride);
            let offsets: Vec<_> = order
                .iter()
                .map(|&i| i as u64 * u64::from(stride))
                .collect();
            let expected = baseline(&src, &offsets, kind, stride, words);
            let actual = gathered(&src, &offsets, kind, stride, words);
            assert_eq!(actual, expected, "kind {kind}, stride {stride}");
            // Ensure the fixture actually reaches budget exhaustion; the oracle
            // above, not a second clamp implementation, determines every result.
            assert_ne!(expected[0][0], 0);
            assert_eq!(expected.last().unwrap()[0], 0);
            for (i, record) in records.iter().enumerate() {
                // SAFETY: live mapped source, both submissions have completed.
                let after = unsafe {
                    std::slice::from_raw_parts(data.add(i * stride as usize).cast::<u32>(), words)
                };
                assert_eq!(after, record, "the source stays unchanged");
            }
        }
    }
}

/// Run one old record, then a GPU copy changes the source before fallback.
/// Explicit source states keep the two-list and one-list cases equivalent.
fn across_source_write(use_gather: bool) -> Vec<Vec<u32>> {
    let host = host().unwrap();
    let (src, _) = source(&[vec![3, 2, 11, 22]], 16);
    let (replacement, _) = source(&[vec![5, 4, 77, 88]], 16);
    let body = host.new_list().unwrap();
    let prelude = host.new_list().unwrap();
    let mut gather = Gather::default();
    let first_list = if use_gather { &prelude } else { &body };
    // SAFETY: open list; the source starts common.
    unsafe {
        first_list.list.ResourceBarrier(&[transition(
            &src,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
    }
    let first = if use_gather {
        let out = gather.push_enabled(&body, &src, 0, (0, 16, 1)).unwrap();
        gather.close();
        gather.record(&prelude.list);
        out
    } else {
        // SAFETY: source lives through submission.
        let gpu = unsafe { src.GetGPUVirtualAddress() };
        (bound_counted(&body, gpu, None, (0, 16, 1)).unwrap(), 0)
    };
    // SAFETY: the explicit read state above remains until this barrier.
    unsafe {
        first_list.list.ResourceBarrier(&[transition(
            &src,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COMMON,
        )]);
    }
    prelude.close();
    let old = capture(&body, &first.0, first.1, 4);
    if !use_gather {
        done(&body, &first.0);
    }
    if use_gather {
        assert!(
            gather.push_enabled(&body, &src, 0, (0, 16, 1)).is_none(),
            "a closed prefix cannot move a later read before the write"
        );
    }
    // SAFETY: live buffers, open body; four words fit both allocations.
    unsafe {
        body.list.ResourceBarrier(&[transition(
            &src,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )]);
        body.list.CopyBufferRegion(&src, 0, &replacement, 0, 16);
        body.list.ResourceBarrier(&[transition(
            &src,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COMMON,
        )]);
    }
    // SAFETY: source lives through submission; clamp is recorded after its GPU write.
    let gpu = unsafe { src.GetGPUVirtualAddress() };
    let next = bound_counted(&body, gpu, None, (0, 16, 1)).unwrap();
    let new = capture(&body, &next, 0, 4);
    done(&body, &next);
    if use_gather {
        gather.finish(&body.list);
    }
    body.close();
    host.execute(&[&prelude, &body]);
    vec![old.read(), new.read()]
}

#[test]
fn fallback_after_a_gpu_source_write_reads_the_new_arguments() {
    if host().is_none() {
        return;
    }
    let expected = across_source_write(false);
    assert_ne!(
        expected[0], expected[1],
        "the GPU write must change the observed record"
    );
    assert_eq!(expected[1], [5, 4, 77, 88]);
    assert_eq!(across_source_write(true), expected);
}

#[test]
fn the_first_ineligible_call_closes_the_prefix() {
    let Some(host) = host() else { return };
    let (src, _) = source(&[vec![1; 8], vec![1; 8]], 32);
    let (other, _) = source(&[vec![1; 8], vec![1; 8]], 32);
    // Different source, kind, stride, max; unsupported dispatch, short,
    // unaligned and oversized strides; unaligned source offset.
    let cases = [
        (&other, 0, (0, 32, 1)),
        (&src, 0, (1, 32, 1)),
        (&src, 0, (10, 32, 1)),
        (&src, 0, (0, 16, 1)),
        (&src, 0, (0, 32, 0)),
        (&src, 0, (0, 32, 2)),
        (&src, 0, (2, 32, 1)),
        (&src, 0, (3, 32, 1)),
        (&src, 0, (0, 12, 1)),
        (&src, 0, (0, 18, 1)),
        (&src, 0, (0, 36, 1)),
        (&src, 2, (0, 32, 1)),
        (&src, 64, (0, 32, 1)),
        (&src, u64::MAX - 3, (0, 32, 1)),
        (&src, u64::from(u32::MAX) - 15, (0, 32, 1)),
    ];
    for (bad_source, offset, spec) in cases {
        let owner = host.new_list().unwrap();
        let mut gather = Gather::default();
        assert!(gather.push_enabled(&owner, &src, 0, (0, 32, 1)).is_some());
        assert!(
            gather
                .push_enabled(&owner, bad_source, offset, spec)
                .is_none(),
            "{spec:?} at {offset}"
        );
        assert!(gather.has_work(), "already accepted work is preserved");
        assert!(
            gather.push_enabled(&owner, &src, 0, (0, 32, 1)).is_none(),
            "closure is permanent"
        );
        owner.close();
    }
    for spec in [(2, 12, 1), (0, 16, 0), (0, 16, 2), (0, 36, 1)] {
        let owner = host.new_list().unwrap();
        let mut gather = Gather::default();
        assert!(gather.push_enabled(&owner, &src, 0, spec).is_none());
        assert!(!gather.has_work());
        assert!(gather.push_enabled(&owner, &src, 0, (0, 16, 1)).is_none());
        owner.close();
    }
}

#[test]
fn an_empty_gather_can_close_without_work() {
    let mut gather = Gather::default();
    assert!(!gather.closed);
    assert!(gather.batch.is_none());
    assert!(!gather.has_work());
    gather.close();
    gather.close();
    assert!(gather.closed);
    assert!(!gather.has_work());
}

#[test]
fn closing_before_the_first_candidate_denies_gathering() {
    let Some(host) = host() else { return };
    let (src, _) = source(&[vec![3, 1, 0, 0]], 16);
    let owner = host.new_list().unwrap();
    let mut gather = Gather::default();
    gather.close();
    assert!(gather.push_enabled(&owner, &src, 0, (0, 16, 1)).is_none());
    assert!(gather.batch.is_none());
    assert!(!gather.has_work());
    owner.close();
}

#[test]
fn capacity_counts_repeated_offsets_and_preserves_the_accepted_prefix() {
    let Some(host) = host() else { return };
    let (src, _) = source(&[vec![3, 1, 0, 0]], 16);
    let owner = host.new_list().unwrap();
    let mut gather = Gather::default();
    let mut first_gpu = None;
    for slot in 0..MOST {
        let (out, offset) = gather.push_enabled(&owner, &src, 0, (0, 16, 1)).unwrap();
        // SAFETY: the batch holds the live output allocation.
        let gpu = unsafe { out.GetGPUVirtualAddress() };
        assert_eq!(gpu, *first_gpu.get_or_insert(gpu));
        assert_eq!(
            offset,
            u64::from(slot) * 16,
            "every repeated input gets a slot"
        );
    }
    assert_eq!(gather.batch.as_ref().unwrap().records, MOST);
    assert!(gather.push_enabled(&owner, &src, 0, (0, 16, 1)).is_none());
    assert!(gather.closed);
    assert!(gather.has_work());
    assert_eq!(gather.batch.as_ref().unwrap().records, MOST);
    assert!(gather.push_enabled(&owner, &src, 0, (0, 16, 1)).is_none());
    owner.close();
}

/// Exercise the real owner hooks: no standalone Gather or manually recorded
/// prelude/finish. Reset must discard the closed prefix and its allocation.
#[test]
fn host_list_gather_submits_replays_and_resets_through_production_hooks() {
    use crate::host::tests::{issued, replay};
    use windows::core::Interface;

    let Some(host) = host() else { return };
    let list = host.new_list().unwrap();
    let mut retired: Option<ID3D12Resource> = None;
    for generation in 0..2 {
        if generation != 0 {
            list.reset();
            assert!(list.is_empty(), "reset discards gather and deferred work");
        }
        list.enable_gather_for_test();
        // A different source, kind and stride after reset cannot append to the
        // old batch. Both generations spend their budget before the last call.
        let (kind, stride, records, offsets) = if generation == 0 {
            (
                0,
                16,
                vec![vec![3 << 20, 16, 11, 22], vec![3, 1, 33, 44]],
                vec![0, 0, 0, 16],
            )
        } else {
            (
                10,
                12,
                vec![vec![70_000, 2, 3], vec![1, 1, 1]],
                std::iter::repeat_n(0, 18).chain([12]).collect(),
            )
        };
        let words = records[0].len();
        let (src, _) = source(&records, stride);
        let expected = baseline(&src, &offsets, kind, stride, words);
        assert_ne!(expected[0][0], 0);
        assert_eq!(expected.last().unwrap()[0], 0);
        issued();
        let outputs: Vec<_> = offsets
            .iter()
            .map(|&offset| list.gather(&src, offset, (kind, stride, 1)).unwrap())
            .collect();
        assert_eq!(outputs[0].1, 0, "the new prefix starts at slot zero");
        let output = outputs[0].0.clone();
        if let Some(old) = &retired {
            assert_ne!(
                output.as_raw(),
                old.as_raw(),
                "reset replaces the old batch"
            );
        }
        let reads: Vec<_> = outputs
            .iter()
            .map(|(out, offset)| capture(&list, out, *offset, words))
            .collect();
        list.gather_close();
        assert!(
            list.gather(&src, 0, (kind, stride, 1)).is_none(),
            "the real close wrapper honors the per-instance test override"
        );
        list.close(); // Production finish must return the output to COMMON.
        let body_barriers = issued();
        let key = output.as_raw() as usize;
        let mut states = std::collections::HashMap::from([(key, 0u32)]);
        for submission in 0..2 {
            for read in &reads {
                // SAFETY: no submission is in flight; mapped live readbacks.
                unsafe {
                    read.data.write_bytes(0xcd, read.words * 4);
                }
            }
            host.execute(&[&list]); // Production prelude and deferred budget reset.
            let mut barriers = issued();
            // The prelude is built at submission, but executes before the body
            // whose barriers were built while recording it.
            barriers.extend_from_slice(&body_barriers);
            replay(&barriers, &mut states).unwrap();
            assert_eq!(
                states[&key], 0,
                "scratch common after submission {submission}"
            );
            let actual: Vec<_> = reads.iter().map(Readback::read).collect();
            assert_eq!(
                actual, expected,
                "generation {generation}, submission {submission}"
            );
        }
        // Keep the retired allocation alive across reset so its identity
        // cannot be reused and conceal a retained batch.
        retired = Some(output);
    }
    list.reset();
    assert!(list.is_empty());
    list.gather_close(); // A write before the first candidate closes the prefix.
    list.enable_gather_for_test();
    let (src, _) = source(&[vec![3, 1, 0, 0]], 16);
    assert!(list.gather(&src, 0, (0, 16, 1)).is_none());
    list.close();
}
