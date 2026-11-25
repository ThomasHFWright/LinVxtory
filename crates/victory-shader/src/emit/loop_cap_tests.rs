//! CPU execution of the actual emitter's synthetic loop CFG, including PHIs.
//! No game data, DXC, device creation or GPU commands.

use super::*;
use crate::native::ComputeIo;

#[test]
fn loop_cap_reaches_follow_barrier_with_current_phi_values() {
    // Each host wave counts to its first thread's ID * 256. Wave zero
    // leaves immediately; wave one needs 8192 iterations and hits the cap.
    let words: [u32; 10] = [
        0x3400_0088, // v_lshlrev_b32 v0, 8, v0
        0x7E00_0500, // v_readfirstlane_b32 s0, v0
        0xBE81_0380, // s_mov_b32 s1, 0
        0xBF06_0001, // header: s_cmp_eq_u32 s1, s0
        0xBF85_0002, // s_cbranch_scc1 -> follow
        0x8001_8101, // s_add_u32 s1, s1, 1
        0xBF82_FFFC, // s_branch -> header
        0xBF8A_0000, // follow: s_barrier
        0x7E04_0201, // v_mov_b32 v2, s1 (observe loop-carried value)
        0xBF81_0000, // s_endpgm
    ];
    for (cmp, branch) in [(0xBF06_0001, 0xBF85_0002), (0xBF07_0001, 0xBF84_0002)] {
        for shift in [6, 7, 8] {
            let mut words = words;
            words[0] = 0x3400_0080 | shift;
            words[3] = cmp;
            words[4] = branch;
            let (m, entry, follows) = emitted(&words);
            check_follow(&m, entry, [0, (32 << shift).min(4096)]);
            assert_eq!(follows, 1);
        }
    }
}

#[test]
fn conditional_loop_caps_keep_both_exit_orientations() {
    // The add, compare and conditional self-backedge share one loop block.
    for (cmp, branch) in [(0xBF06_0001, 0xBF84_FFFD), (0xBF07_0001, 0xBF85_FFFD)] {
        let words = [
            0x3400_0088, // v_lshlrev_b32 v0, 8, v0
            0x4A00_0081, // v_add_nc_u32 v0, 1, v0
            0x7E00_0500, // v_readfirstlane_b32 s0, v0
            0xBE81_0380, // s_mov_b32 s1, 0
            0x8001_8101, // header: s_add_u32 s1, s1, 1
            cmp,         // s_cmp_eq/lg_u32 s1, s0
            branch,      // s_cbranch_scc0/1 -> header
            0xBF8A_0000, // follow: s_barrier
            0x7E04_0201, // v_mov_b32 v2, s1
            0xBF81_0000,
        ];
        let (m, entry, follows) = emitted(&words);
        check_follow(&m, entry, [1, 4096]);
        assert_eq!(follows, 0, "conditional latch keeps its original exit");
    }
}

#[test]
fn mixed_latches_share_one_counter_and_header_follow() {
    for (cmp, branch) in [(0xBF06_0001, 0xBF84_FFF8), (0xBF07_0001, 0xBF85_FFF8)] {
        let (m, entry, follows) = emitted(&[
            0x3400_0088, // v_lshlrev_b32 v0, 8, v0
            0x7E00_0500, // v_readfirstlane_b32 s0, v0
            0xBE81_0380, // s_mov_b32 s1, 0
            0xBF06_0001, // header: s_cmp_eq_u32 s1, s0
            0xBF85_0006, // s_cbranch_scc1 -> follow
            0x8001_8101, // s_add_u32 s1, s1, 1
            0x8703_8101, // s_and_b32 s3, s1, 1; alternate latches
            0xBF84_0001, // s_cbranch_scc0 -> conditional latch
            0xBF82_FFFA, // unconditional latch -> header
            cmp,         // conditional latch: s_cmp_eq/lg_u32 s1, s0
            branch,      // s_cbranch_scc0/1 -> header
            0xBF8A_0000, // follow: s_barrier
            0x7E04_0201, // v_mov_b32 v2, s1
            0xBF81_0000,
        ]);
        check_follow(&m, entry, [0, 4096]);
        assert_eq!(follows, 1);
    }
}

#[test]
fn nested_loop_caps_reset_inner_counter_and_preserve_outer_phi() {
    let (m, entry, follows) = emitted(&[
        0x3400_0088, // v_lshlrev_b32 v0, 8, v0
        0x7E00_0500, // v_readfirstlane_b32 s0, v0
        0xBE81_0380, // s_mov_b32 s1, 0 (outer counter)
        0xBE84_0380, // s_mov_b32 s4, 0 (total work)
        0xBF06_8201, // outer header: s_cmp_eq_u32 s1, 2
        0xBF85_0008, // s_cbranch_scc1 -> outer follow
        0xBE83_0380, // s_mov_b32 s3, 0 (reset inner counter)
        0xBF06_0003, // inner header: s_cmp_eq_u32 s3, s0
        0xBF85_0003, // s_cbranch_scc1 -> inner follow
        0x8003_8103, // s_add_u32 s3, s3, 1
        0x8004_8104, // s_add_u32 s4, s4, 1
        0xBF82_FFFB, // s_branch -> inner header
        0x8001_8101, // inner follow: s_add_u32 s1, s1, 1
        0xBF82_FFF6, // s_branch -> outer header
        0xBF8A_0000, // outer follow: s_barrier
        0x7E04_0204, // v_mov_b32 v2, s4
        0xBF81_0000,
    ]);
    check_follow(&m, entry, [0, 8192]);
    assert_eq!(follows, 2);
}

#[test]
fn loop_without_header_follow_keeps_bounded_fallback() {
    let (m, entry, follows) = emitted(&[
        0xBE81_0380, // s_mov_b32 s1, 0
        0x8001_8101, // header: s_add_u32 s1, s1, 1
        0xBF82_FFFE, // s_branch -> header; no genuine exit to invent
    ]);
    assert_eq!(follows, 0);
    for lane in [0, 32] {
        assert!(m.trace_integer_calls(entry, lane).is_empty());
    }
}

#[test]
fn loop_with_multiple_follows_does_not_invent_a_cap_exit() {
    let (m, entry, follows) = emitted(&[
        0x3400_0088, // v_lshlrev_b32 v0, 8, v0
        0x7E00_0500, // v_readfirstlane_b32 s0, v0
        0xBE81_0380, // s_mov_b32 s1, 0
        0xBF06_0001, // header: s_cmp_eq_u32 s1, s0
        0xBF85_0004, // s_cbranch_scc1 -> first follow
        0x8001_8101, // s_add_u32 s1, s1, 1
        0xBF06_8101, // s_cmp_eq_u32 s1, 1
        0xBF85_0004, // s_cbranch_scc1 -> second follow
        0xBF82_FFFA, // s_branch -> header
        0xBF8A_0000, // first follow: s_barrier
        0x7E04_0201, // v_mov_b32 v2, s1
        0xBF81_0000,
        0xBF8A_0000, // second follow: s_barrier
        0x7E04_0201, // v_mov_b32 v2, s1
        0xBF81_0000,
    ]);
    assert_eq!(follows, 0);
    // Check normal control flow only: the two different barriers do not
    // constitute a valid workgroup rendezvous and are not claimed safe.
    check_follow(&m, entry, [0, 1]);
}

fn emitted(words: &[u32]) -> (super::super::bitcode::Module, usize, usize) {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [64, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = crate::ir::build(&native.code, false, 4).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = crate::io::raise(&p, &native).unwrap();
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
    // Also exercise production cleanup and serialization when DXIL validation
    // is available, before adding the CPU-only observation calls below.
    #[cfg(windows)]
    if let Ok(mut validator) = super::super::validator::Validator::load(
        super::super::validator::Validator::default_path().as_deref(),
    ) {
        let mut finished = super::lower(&parts).unwrap().dxil;
        validator.validate(&mut finished).unwrap();
    }
    let bad = HashMap::new();
    let mut lower = Lower::new(&parts, &bad);
    match lower.emit_body() {
        Ok(()) => {}
        Err(Stop::Fail(e)) => panic!("synthetic loop: {e}"),
        Err(Stop::Retry(v, why)) => panic!("synthetic loop retry {v:?}: {why}"),
    }
    for (b, block) in p.blocks.iter().enumerate() {
        if let Some(result) = block.insts.iter().find_map(|i| i.write(Reg::V(2))) {
            let value = lower.i32_of(result).unwrap();
            let ty = lower.s.m.func_ty(lower.s.t.void, vec![lower.s.t.i32]);
            let sink = lower.s.m.function("test.result", ty, None, false);
            lower.s.m.call(lower.tail[b].unwrap(), sink, vec![value]);
        }
    }
    (lower.s.m, lower.s.entry, lower.cap_follows.len())
}

fn check_follow(m: &super::super::bitcode::Module, entry: usize, expected: [u32; 2]) {
    for (lane, expected) in [0, 32].into_iter().zip(expected) {
        let calls = m.trace_integer_calls(entry, lane);
        let barriers: Vec<_> = calls.iter().filter(|c| c.name == "dx.op.barrier").collect();
        assert_eq!(
            barriers.len(),
            1,
            "lane {lane} must reach the follow barrier"
        );
        assert_eq!(
            barriers[0].args[1],
            [9],
            "synchronization must stay enabled"
        );
        let result = calls
            .iter()
            .find(|c| c.name == "test.result")
            .expect("follow result");
        assert_eq!(result.args[0], [expected], "lane {lane}: current PHI");
    }
}
