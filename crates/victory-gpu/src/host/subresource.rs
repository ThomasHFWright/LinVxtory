//! Pure state bookkeeping shared by recording and submission. No GPU calls.

pub(crate) const ALL: u32 = u32::MAX;

pub(crate) fn count(mips: u32, slices: u32, volume: bool, planes: u32) -> usize {
    (mips.max(1) * if volume { 1 } else { slices } * planes) as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) index: u32,
    pub(crate) before: u32,
    pub(crate) after: u32,
}

/// First and last use of each subresource in one command list. Unused
/// subresources remain unknown until submission, not guessed at recording.
pub(crate) struct ListState(Vec<Option<(u32, u32)>>);

impl ListState {
    pub(crate) fn new(count: usize) -> Self {
        Self(vec![None; count])
    }

    pub(crate) fn transition(&mut self, index: u32, wanted: u32) -> Result<Vec<Change>, ()> {
        let range = if index == ALL {
            0..self.0.len()
        } else if (index as usize) < self.0.len() {
            index as usize..index as usize + 1
        } else {
            return Err(());
        };
        let mut changes = Vec::new();
        for i in range {
            match &mut self.0[i] {
                Some((_, last)) => {
                    if *last != wanted {
                        changes.push(Change { index: i as u32, before: *last, after: wanted });
                        *last = wanted;
                    }
                }
                slot @ None => *slot = Some((wanted, wanted)),
            }
        }
        Ok(compact(changes, self.0.len()))
    }

    /// Bring the submitted resource to each first use, publishing each
    /// last use globally. Temporary copy states are restored for untouched
    /// subresources. Recording order never determines submission order.
    pub(crate) fn prepare(&self, global: &mut [u32], copied: Option<&[u32]>) -> Vec<Change> {
        assert_eq!(self.0.len(), global.len());
        assert!(copied.is_none_or(|s| s.len() == global.len()));
        let mut changes = Vec::new();
        for (i, usage) in self.0.iter().enumerate() {
            let before = copied.map_or(global[i], |s| s[i]);
            let (first, last) = usage.unwrap_or((global[i], global[i]));
            if before != first {
                changes.push(Change { index: i as u32, before, after: first });
            }
            global[i] = last;
        }
        compact(changes, global.len())
    }
}

/// Temporary whole-resource copies and their restoration use the same
/// exact before states as normal transitions.
pub(crate) fn between(before: &[u32], after: &[u32]) -> Vec<Change> {
    assert_eq!(before.len(), after.len());
    let changes = before.iter().zip(after).enumerate().filter_map(|(i, (&before, &after))| {
        (before != after).then_some(Change { index: i as u32, before, after })
    }).collect();
    compact(changes, before.len())
}

fn compact(mut changes: Vec<Change>, count: usize) -> Vec<Change> {
    // ALL is valid only when every subresource has the same transition.
    if !changes.is_empty() && changes.len() == count
        && changes.iter().all(|c| c.before == changes[0].before && c.after == changes[0].after)
    {
        changes.truncate(1);
        changes[0].index = ALL;
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    const SRV: u32 = 0xc0;
    const UAV: u32 = 8;
    const COPY: u32 = 0x400;

    fn change(index: u32, before: u32, after: u32) -> Change { Change { index, before, after } }

    #[test]
    fn distinct_mips_keep_read_and_write_states() {
        let mut list = ListState::new(7);
        assert!(list.transition(0, SRV).unwrap().is_empty());
        for mip in 1..4 { assert!(list.transition(mip, UAV).unwrap().is_empty()); }
        let mut global = vec![0; 7];
        assert_eq!(list.prepare(&mut global, None), vec![
            change(0, 0, SRV), change(1, 0, UAV), change(2, 0, UAV), change(3, 0, UAV),
        ]);
        assert_eq!(global, [SRV, UAV, UAV, UAV, 0, 0, 0]);
    }

    #[test]
    fn two_stage_mip_chain_transitions_only_its_intermediate_mip() {
        let mut list = ListState::new(7);
        list.transition(0, SRV).unwrap();
        for mip in [3, 2, 1] { list.transition(mip, UAV).unwrap(); }
        assert_eq!(list.transition(3, SRV).unwrap(), vec![change(3, UAV, SRV)]);
        for mip in [6, 5, 4] { assert!(list.transition(mip, UAV).unwrap().is_empty()); }
        let mut global = vec![0; 7];
        let prelude = list.prepare(&mut global, None);
        assert_eq!(prelude, (0..7).map(|i| change(i, 0, if i == 0 { SRV } else { UAV })).collect::<Vec<_>>());
        assert_eq!(global, [SRV, UAV, UAV, SRV, UAV, UAV, UAV]);
    }

    #[test]
    fn two_stage_mip_chain_in_separate_lists_uses_submitted_states() {
        // Record the consumer first, submit the producer first.
        let mut consumer = ListState::new(7);
        consumer.transition(3, SRV).unwrap();
        for mip in [6, 5, 4] { consumer.transition(mip, UAV).unwrap(); }
        let mut producer = ListState::new(7);
        producer.transition(0, SRV).unwrap();
        for mip in [3, 2, 1] { producer.transition(mip, UAV).unwrap(); }
        let mut global = vec![0; 7];
        producer.prepare(&mut global, None);
        assert_eq!(consumer.prepare(&mut global, None), vec![
            change(3, UAV, SRV), change(4, 0, UAV), change(5, 0, UAV), change(6, 0, UAV),
        ]);
        assert_eq!(global, [SRV, UAV, UAV, SRV, UAV, UAV, UAV]);
    }

    #[test]
    fn array_cube_volume_and_plane_indices_are_distinct() {
        assert_eq!(count(7, 1, false, 1), 7);
        assert_eq!(count(3, 12, false, 1), 36); // Two cubes, six faces each.
        assert_eq!(count(4, 64, true, 1), 4); // Depth slices are not subresources.
        assert_eq!(count(3, 2, false, 2), 12);
        let mut list = ListState::new(12);
        list.transition(2, SRV).unwrap(); // mip 2, array 0, plane 0
        list.transition(11, UAV).unwrap(); // mip 2, array 1, plane 1
        let mut global = vec![0; 12];
        assert_eq!(list.prepare(&mut global, None), vec![change(2, 0, SRV), change(11, 0, UAV)]);
        assert_eq!(global.iter().filter(|&&s| s != 0).count(), 2);
    }

    #[test]
    fn whole_after_partial_preserves_first_uses() {
        let mut list = ListState::new(3);
        list.transition(1, UAV).unwrap();
        assert_eq!(list.transition(ALL, SRV).unwrap(), vec![change(1, UAV, SRV)]);
        let mut global = vec![0; 3];
        assert_eq!(list.prepare(&mut global, None), vec![change(0, 0, SRV), change(1, 0, UAV), change(2, 0, SRV)]);
        assert_eq!(global, [SRV; 3]);
    }

    #[test]
    fn partial_after_whole_never_ors_read_and_write() {
        let mut list = ListState::new(3);
        list.transition(ALL, SRV).unwrap();
        assert_eq!(list.transition(2, UAV).unwrap(), vec![change(2, SRV, UAV)]);
        assert!(list.transition(2, UAV).unwrap().is_empty());
        let mut global = vec![0; 3];
        assert_eq!(list.prepare(&mut global, None), vec![change(ALL, 0, SRV)]);
        assert_eq!(global, [SRV, SRV, UAV]);
    }

    #[test]
    fn prelude_uses_submission_order_and_supports_resubmission() {
        let mut a = ListState::new(2);
        let mut b = ListState::new(2);
        a.transition(0, SRV).unwrap();
        a.transition(0, UAV).unwrap();
        b.transition(0, COPY).unwrap();
        let mut global = vec![0; 2];
        b.prepare(&mut global, None);
        assert_eq!(a.prepare(&mut global, None), vec![change(0, COPY, SRV)]);
        assert_eq!(a.prepare(&mut global, None), vec![change(0, UAV, SRV)]);
        assert_eq!(global, [UAV, 0]);
    }

    #[test]
    fn whole_transition_from_mixed_states_cannot_use_all() {
        let mut list = ListState::new(3);
        list.transition(ALL, COPY).unwrap();
        let mut global = vec![SRV, UAV, SRV];
        assert_eq!(list.prepare(&mut global, None), vec![change(0, SRV, COPY), change(1, UAV, COPY), change(2, SRV, COPY)]);
        assert_eq!(global, [COPY; 3]);
    }

    #[test]
    fn copy_prelude_restores_untouched_mips_and_planes() {
        let mut list = ListState::new(4);
        list.transition(1, UAV).unwrap();
        let mut global = vec![SRV, SRV, UAV, UAV];
        let copied = vec![COPY; 4];
        assert_eq!(list.prepare(&mut global, Some(&copied)), vec![
            change(0, COPY, SRV), change(1, COPY, UAV), change(2, COPY, UAV), change(3, COPY, UAV),
        ]);
        assert_eq!(global, [SRV, UAV, UAV, UAV]);
        assert_eq!(between(&copied, &global).len(), 4);
    }

    #[test]
    fn alias_copy_restores_mixed_read_and_write_states() {
        let mut list = ListState::new(4);
        list.transition(0, SRV).unwrap();
        list.transition(1, UAV).unwrap();
        // Two mips have no earlier list use; they stay in the copy state.
        let before = list.transition(ALL, COPY).unwrap();
        assert_eq!(before, vec![change(0, SRV, COPY), change(1, UAV, COPY)]);
        for prior in before {
            assert_eq!(list.transition(prior.index, prior.before).unwrap(), vec![change(prior.index, COPY, prior.before)]);
        }
        let mut global = vec![0; 4];
        list.prepare(&mut global, None);
        assert_eq!(global, [SRV, UAV, COPY, COPY]);
    }

    #[test]
    fn alias_copy_can_restore_a_uniform_whole_resource() {
        let mut list = ListState::new(3);
        list.transition(ALL, UAV).unwrap();
        let before = list.transition(ALL, COPY).unwrap();
        assert_eq!(before, vec![change(ALL, UAV, COPY)]);
        for prior in before { list.transition(prior.index, prior.before).unwrap(); }
        let mut global = vec![0; 3];
        list.prepare(&mut global, None);
        assert_eq!(global, [UAV; 3]);
    }

    #[test]
    fn invalid_index_changes_nothing_and_reset_starts_unknown() {
        let mut list = ListState::new(2);
        assert_eq!(list.transition(2, UAV), Err(()));
        assert_eq!(list.transition(ALL - 1, UAV), Err(()));
        let mut global = vec![SRV; 2];
        assert!(list.prepare(&mut global, None).is_empty());
        assert!(ListState::new(2).transition(1, UAV).unwrap().is_empty());
    }
}
