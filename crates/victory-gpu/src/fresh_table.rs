//! Fresh descriptor table bounds and identity, without a host device.

use std::hash::{Hash, Hasher};

/// Half views can lie beyond a declared range's first descriptor. Keep
/// them in both the allocation span and the caller's readable selections.
pub(crate) fn span(
    slot: u8,
    offsets: impl Iterator<Item = u32>,
    selected: &mut Vec<u32>,
    half: &[(u8, u32)],
) -> u32 {
    selected.extend(
        half.iter()
            .filter(|&&(s, _)| s == slot)
            .map(|&(_, at)| at / 8),
    );
    selected.sort_unstable();
    selected.dedup();
    offsets
        .map(|at| at / 8)
        .chain(selected.last().copied())
        .max()
        .unwrap_or(0)
        + 1
}

/// Snapshot each descriptor's record and installed resident mip range.
/// The half-view selection also changes the installed descriptor format.
pub(crate) fn key(
    table: u64,
    slot: u8,
    count: u32,
    half: &[(u8, u32)],
    mut entry: impl FnMut(u32) -> ([u32; 3], u32),
) -> (u64, u32, u64) {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for index in 0..count {
        let (record, first_shown) = entry(index);
        record.hash(&mut hash);
        first_shown.hash(&mut hash);
        half.contains(&(slot, index * 8)).hash(&mut hash);
    }
    (table, count, hash.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternating_half_views_and_residency_keep_distinct_cached_tables() {
        let records = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];
        let make_key = |half: &[(u8, u32)], mip| {
            key(0x1000, 1, 3, half, |i| {
                (records[i as usize], if i == 2 { mip } else { 0 })
            })
        };
        let normal = make_key(&[], 0);
        let half = make_key(&[(1, 8)], 0);
        let resident = make_key(&[(1, 8)], 2);
        let other_half = make_key(&[(1, 0)], 0);
        let mut cache = std::collections::HashMap::new();
        for (entry, offset) in [(normal, 10), (half, 13), (resident, 16), (other_half, 19)] {
            assert_eq!(
                cache.insert(entry, offset),
                None,
                "a live table was reused with different views"
            );
        }
        assert_eq!(cache.get(&make_key(&[], 0)), Some(&10));
        assert_eq!(cache.get(&make_key(&[(1, 8)], 0)), Some(&13));
        assert_eq!(cache.get(&make_key(&[(1, 8)], 2)), Some(&16));
        // Other root parameters, duplicate metadata and metadata order
        // cannot change this table's installed descriptors.
        assert_eq!(make_key(&[(7, 0), (1, 8), (1, 8)], 0), half);
    }

    #[test]
    fn copied_record_changes_and_table_bounds_change_the_key() {
        let records = [[1, 2, 3], [4, 5, 6]];
        let original = key(0x1000, 1, 2, &[], |i| (records[i as usize], 0));
        for index in 0..2 {
            for word in 0..3 {
                let mut changed = records;
                changed[index][word] ^= 1;
                assert_ne!(
                    key(0x1000, 1, 2, &[], |i| (changed[i as usize], 0)),
                    original
                );
            }
        }
        assert_ne!(
            key(0x2000, 1, 2, &[], |i| (records[i as usize], 0)),
            original
        );
        assert_ne!(
            key(0x1000, 1, 1, &[], |i| (records[i as usize], 0)),
            original
        );
    }

    #[test]
    fn half_views_past_range_starts_reserve_slots_and_become_readable() {
        let mut selected = vec![];
        // One declared range begins at zero, but the half view is slot 2.
        let count = span(
            1,
            [0].into_iter(),
            &mut selected,
            &[(1, 16), (1, 16), (7, 800)],
        );
        assert_eq!(count, 3, "the next allocation would overlap the half view");
        assert_eq!(
            selected,
            [2],
            "the interior view must pass through readable on cache hits too"
        );
    }

    #[test]
    fn indexed_views_and_declared_ranges_still_extend_half_tables() {
        let mut selected = vec![5, 2, 5];
        assert_eq!(span(1, [0, 56].into_iter(), &mut selected, &[(1, 16)]), 8);
        assert_eq!(selected, [2, 5]);
        let mut selected = vec![5];
        assert_eq!(span(1, [0].into_iter(), &mut selected, &[(1, 16)]), 6);
        assert_eq!(selected, [2, 5]);
    }
}
