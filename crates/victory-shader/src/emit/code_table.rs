//! Code-relative constant table bounds, from upstream PR1006.
// shortcut: dynamic CODE reads are assumed to target data after the final
// instruction. Embedded data gaps need proven address bounds before trimming
// can be generalized; the tested title follows the trailing-table layout.

/// Where a program's constant table starts in its code: where its
/// instructions end, unless a load at a fixed offset reads below that (then
/// at `lowest`, the lowest offset any load or descriptor names). The
/// program's constants follow its instructions, and a load at a computed
/// offset reads them, not instruction bytes.
pub(super) fn start(
    p: &crate::ir::Program,
    addresses: &[&crate::resource::Address],
    lowest: u32,
) -> u32 {
    let code_end = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .map(|i| i.machine.offset + 4 * i.machine.words.len())
        .max()
        .unwrap_or(0) as u32;
    let fixed_below = addresses
        .iter()
        .any(|a| a.dynamic.is_none() && a.offset < code_end);
    if fixed_below {
        lowest
    } else {
        lowest.max(code_end & !3)
    }
}

#[cfg(test)]
mod tests {
    use super::start as code_table_start;
    use crate::{ir, resource};
    const S_ENDPGM: u32 = 0xBF81_0000;

    /// A program's constant table starts after its instructions, unless a
    /// load at a fixed offset reads below that.
    #[test]
    fn the_code_table_starts_after_the_instructions() {
        // s_nop 0, s_endpgm, then 1,000 dwords of constants.
        let mut words = vec![0xBF80_0000, S_ENDPGM];
        words.extend(0..1000u32);
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = ir::build(&code, false, 256).unwrap();
        let at = |offset: u32, dynamic: bool| resource::Address {
            param: resource::CODE,
            offset,
            dynamic: dynamic.then_some(ir::Value(0)),
        };
        let (computed, fixed_after, fixed_inside) = (at(0, true), at(12, false), at(4, false));
        // A load at a computed offset from the code's start: the table is
        // the constants only, not the whole program.
        assert_eq!(code_table_start(&p, &[&computed], 0), 8);
        assert_eq!(code_table_start(&p, &[&computed, &fixed_after], 0), 8);
        // A load at a fixed offset inside the instructions keeps the start.
        assert_eq!(code_table_start(&p, &[&computed, &fixed_inside], 0), 0);
        // Loads past the instructions only: from the lowest of them.
        assert_eq!(code_table_start(&p, &[&fixed_after], 12), 12);
    }
}
