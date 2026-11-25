//! Inspect the actual scalar heap-load emitter after folding known table entries.
//! Synthetic instructions only; no DXC, GPU device, or game fixtures.

use super::*;
use crate::emit::shader::{SLOT_STRIDE_CBV, SLOT_STRIDE_UAV};
use crate::native::{ComputeIo, RootParam};
use crate::resource::Descriptor;

#[test]
fn scalar_heap_descriptor_branches_create_only_matching_handles() {
    let words: [u32; 7] = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
        0xF420_0404,
        0xFA00_0000, // s_buffer_load_dword s16, s[8:11], null
        0xBF81_0000,
    ];
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![RootParam {
            root: RootType::ResourceHeap,
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
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = crate::ir::build(&native.code, false, 32).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    assert_eq!(
        res.descriptors,
        vec![Descriptor {
            site: (0, 3),
            role: Role::Resource,
            source: Source::Loaded {
                load: (0, 2),
                first: 0
            },
        }]
    );
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
    };
    let bad = HashMap::new();
    for (info, expected, expected_stride) in [
        (0, None, 0),               // null, texture and other nonbuffer slots
        (SLOT_STRIDE_UAV, None, 0), // flag without any stride is invalid
        (1, Some((11, 0)), 1),
        (4, Some((11, 0)), 4),
        (2048, Some((11, 0)), 2048),
        (SLOT_STRIDE_UAV | 4, Some((0x100b, 0)), 4),
        (SLOT_STRIDE_UAV | 2048, Some((0x100b, 0)), 2048),
        (SLOT_STRIDE_CBV, Some((13, 0x1_0000)), 0),
    ] {
        let mut lower = Lower::new(&parts, &bad);
        let entry = lower.s.entry;
        // Inject the result of the table lookup, leaving the production
        // classification, branches, handle creation and loads untouched.
        let info_value = lower.s.m.i32(info);
        lower.strides.insert((entry, (0, 2), 0), info_value);
        let source = &res.descriptors[0].source;
        let stride = lower.slot_stride(source).unwrap().unwrap();
        let base = p.blocks[0].insts[2].read(Reg::S(4)).unwrap();
        let address = lower.s.m.i32(0);
        lower.vals.insert(base, Val::I32(address));
        lower.smem(&p.blocks[0].insts[3], (0, 3)).unwrap();
        let result = p.blocks[0].insts[3].write(Reg::S(16)).unwrap();
        let result = lower.i32_of(result).unwrap();
        // Keep the emitted result and vector stride visible to the inspection
        // helper; it folds the production predicates and phi values.
        let ty = lower.s.m.func_ty(lower.s.t.void, vec![lower.s.t.i32; 2]);
        let sink = lower.s.m.function("test.result", ty, None, false);
        lower.s.m.call(lower.cur, sink, vec![result, stride]);
        lower.s.m.terminate(lower.cur, Term::Ret);
        let calls = lower.s.m.folded_calls();
        let named = |name: &str| calls.iter().filter(|c| c.name == name).collect::<Vec<_>>();
        assert_eq!(
            named("dx.op.createHandleFromHeap").len(),
            usize::from(expected.is_some()),
            "info={info:#x}"
        );
        let props: Vec<_> = named("dx.op.annotateHandle")
            .iter()
            .map(|c| c.args[2].clone())
            .collect();
        assert_eq!(
            props,
            expected
                .map(|(kind, size)| vec![kind, size])
                .into_iter()
                .collect::<Vec<_>>(),
            "info={info:#x}"
        );
        assert_eq!(
            named("dx.op.rawBufferLoad.f32").len(),
            usize::from(expected.is_some_and(|(kind, _)| kind != 13)),
            "info={info:#x}"
        );
        assert_eq!(
            named("dx.op.cbufferLoadLegacy.i32").len(),
            usize::from(expected.is_some_and(|(kind, _)| kind == 13)),
            "info={info:#x}"
        );
        let results = named("test.result");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].args[1], [expected_stride], "info={info:#x}");
        if expected.is_none() {
            assert_eq!(results[0].args[0], [0], "info={info:#x}");
        }
    }
}
