//! An integer image atomic must not inherit an earlier float annotation.
//! Synthetic CPU-only emitter checks; no game input, DXC, or GPU device.

use super::*;
use crate::native::{ComputeIo, RootParam};

#[test]
fn integer_atomic_after_float_handle_keeps_uint_properties_in_every_cache() {
    let words = [
        0xBE84_0300u32, // s_mov_b32 s4, s0
        0xBE85_0380,    // s_mov_b32 s5, 0
        0xF40C_0202,
        0xFA00_0000, // s_load_dwordx8 s[8:15], s[4:5], 0
        0xBF06_8000, // s_cmp_eq_u32 s0, 0
        0xBF85_0002, // s_cbranch_scc1 second
        0xBE86_0380, // s_mov_b32 s6, 0
        0xBF82_0001, // s_branch join
        0xBE86_0381, // second: s_mov_b32 s6, 1
        0xBF81_0000, // join: s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [1, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
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
    let load = (0, 2);
    let base = p.blocks[load.0].insts[load.1].read(Reg::S(4)).unwrap();
    let join = p
        .blocks
        .iter()
        .position(|b| b.preds.len() == 2)
        .expect("diamond join");

    for cache in ["entry", "block", "join"] {
        let mut lower = Lower::new(&parts, &bad);
        // Supply a known descriptor address, keeping the production cache,
        // handle creation, annotation and join-index emission intact.
        let zero = lower.s.m.i32(0);
        lower.vals.insert(base, Val::I32(zero));
        lower.scalar.insert(
            load,
            Address {
                param: 0,
                offset: 0,
                // A dynamic address selects the block-local handle cache.
                dynamic: (cache == "block").then_some(base),
            },
        );
        let loaded = Source::Loaded { load, first: 0 };
        let source = if cache == "join" {
            for b in 0..p.blocks.len() {
                let block = if b == 0 {
                    lower.s.entry
                } else {
                    lower.s.m.new_block()
                };
                lower.block[b] = Some(block);
                lower.tail[b] = Some(block);
            }
            let preds = &p.blocks[join].preds;
            assert_eq!(p.blocks.len(), 4);
            let yes = lower.s.m.bool(true);
            lower.s.m.terminate(
                lower.s.entry,
                Term::CondBr(
                    yes,
                    lower.block[preds[0]].unwrap(),
                    lower.block[preds[1]].unwrap(),
                ),
            );
            for &pred in preds {
                lower.s.m.terminate(
                    lower.block[pred].unwrap(),
                    Term::Br(lower.block[join].unwrap()),
                );
            }
            lower.cur = lower.block[join].unwrap();
            Source::Phi {
                block: join,
                args: vec![loaded.clone(); preds.len()],
            }
        } else {
            loaded
        };
        // Same texture UAV kind, different component types and counts:
        // float4 (descriptor prewarm) then uint1 (image_atomic_or).
        let float_props = (0x1002, 0x409);
        let uint_props = (0x1002, 0x105);
        let float = lower.handle(load, &source, false, float_props).unwrap();
        let uint = lower.handle(load, &source, false, uint_props).unwrap();
        assert_ne!(float, uint, "{cache}: atomic inherited float annotation");
        assert_eq!(
            lower.handle(load, &source, false, float_props).unwrap(),
            float,
            "{cache}"
        );
        assert_eq!(
            lower.handle(load, &source, false, uint_props).unwrap(),
            uint,
            "{cache}"
        );
        let count = match cache {
            "entry" => lower.entry_handles.len(),
            "block" => lower.handles.len(),
            _ => lower.join_handles.len(),
        };
        assert_eq!(count, 2, "{cache}: cache route and reuse");

        let op = lower.s.m.i32(2); // integer OR
        let bits = lower.s.m.i32(0x80);
        let unused = lower.s.m.undef(lower.s.t.i32);
        lower.s.op(
            lower.cur,
            "atomicBinOp.i32",
            Attr::NoUnwind,
            lower.s.t.i32,
            78,
            &[uint, op, zero, zero, unused, bits],
        );
        lower.s.m.terminate(lower.cur, Term::Ret);
        let calls = lower.s.m.folded_calls();
        let props: Vec<_> = calls
            .iter()
            .filter(|c| c.name == "dx.op.annotateHandle")
            .map(|c| c.args[2].clone())
            .collect();
        assert_eq!(
            props.iter().filter(|p| **p == vec![0x1002, 0x409]).count(),
            1,
            "{cache}"
        );
        assert_eq!(
            props.iter().filter(|p| **p == vec![0x1002, 0x105]).count(),
            1,
            "{cache}"
        );
        let atomics: Vec<_> = calls
            .iter()
            .filter(|c| c.name == "dx.op.atomicBinOp.i32")
            .collect();
        assert_eq!(atomics.len(), 1, "{cache}");
        assert_eq!(atomics[0].args[2], [2]);
        assert_eq!(atomics[0].args[6], [0x80]);
    }
}
