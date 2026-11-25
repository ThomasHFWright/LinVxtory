//! Behavioral port of c36971b's scalar-buffer wide-load test. Both lowering
//! modes run in one process against the same synthetic raw view on WARP.

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, StaticSampler, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

// First SGPR, dwords, immediate byte offset; s0 supplies the dynamic offset.
const LOADS: [(u32, u32, u32); 4] = [(16, 2, 0), (20, 4, 4), (24, 8, 8), (32, 16, 12)];
const OUTPUT_DWORDS: u32 = 32;
const WRITE_MARK: u32 = 9;

struct RestoreWideLoads(Option<bool>);

impl Drop for RestoreWideLoads {
    fn drop(&mut self) {
        lower::TEST_WIDE_LOADS.set(self.0);
    }
}

fn build(wide: bool) -> HostShader {
    let _restore = RestoreWideLoads(lower::TEST_WIDE_LOADS.replace(Some(wide)));
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0 (SRV)
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20 (UAV)
    ];
    for (first, count, offset) in LOADS {
        // s_buffer_load_dwordxN s[first:...], s[8:11], s0, offset
        words.extend([0xF420_0000 | count.ilog2() << 18 | first << 6 | 4, offset]);
    }
    let mut out_dword = 0;
    for (first, count, _) in LOADS {
        for k in 0..count {
            words.push(0x7E00_0000 | 1 << 17 | 0x001 << 9 | (first + k));
            words.extend([
                0x38 << 26 | 0x1C << 18 | (4 * out_dword),
                0x80 << 24 | (12 >> 2) << 16 | 1 << 8,
            ]);
            out_dword += 1;
        }
    }
    words.push(0xBF81_0000);
    let table = |kind, at| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: at,
        lower_bound: 0,
        stride: 4,
        dwords: 4,
        user_data: 4,
        slot: 1,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 4,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![table(ResourceType::Srv, 0), table(ResourceType::Uav, 8)],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("IR");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resources");
    let io = io::raise(&p, &native).expect("I/O");
    let l = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: true,
    })
    .expect("lower scalar buffer loads");
    let mut shader = HostShader {
        stage: HwStage::Cs,
        dxil: l.dxil,
        user_data: l.user_data,
        static_samplers: l
            .samplers
            .iter()
            .map(|&w| StaticSampler::from_words(w))
            .collect(),
        samplers: l.samplers,
        root_cbvs: l.root_cbvs,
        root_srvs: l.root_srvs,
        root_uavs: l.root_uavs,
        root_indexed_views: res.root_indexed_views,
        half_views: l.half_views,
        memory: l.memory,
        memory_writes: l.memory_writes,
        view_addresses: l.view_addresses,
        heap: l.heap,
        heap_writes: l.heap_writes,
        view_marks: l.view_marks,
    };
    assert!(shader.view_marks);
    assert!(!shader.memory && !shader.memory_writes && !shader.view_addresses);
    assert!(
        shader.root_cbvs.is_empty() && shader.root_srvs.is_empty() && shader.root_uavs.is_empty()
    );
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil).expect("DXIL validation");
    }
    shader
}

/// Each width crosses the view's end and the u32 address boundary. Crossing
/// u32::MAX must retain the legacy per-dword wrapping offsets, not clamp them.
#[test]
fn scalar_buffer_loads_read_the_same_dwords_one_call_or_four() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = lower::TEST_WIDE_LOADS.get();
    let narrow = build(false);
    assert_eq!(lower::TEST_WIDE_LOADS.get(), previous);
    let wide = build(true);
    assert_eq!(lower::TEST_WIDE_LOADS.get(), previous);
    assert_ne!(
        narrow.dxil, wide.dxil,
        "the override must exercise both lowerings"
    );
    let Ok(host) = Host::new(true) else {
        eprintln!("no software adapter; scalar-buffer readback skipped");
        return;
    };
    let pipelines = [&narrow, &wide].map(|s| host.compute_pipeline(s).expect("pipeline"));
    let data: Vec<u32> = (0..12u32)
        .map(|i| 0x1111_1111u32.wrapping_mul(i + 1) ^ 0x8000_0000)
        .collect();
    let offsets = (0..=52u32)
        .step_by(4)
        .chain((1..=16u32).map(|n| 0u32.wrapping_sub(4 * n)));
    for offset in offsets {
        let mut want = Vec::new();
        for (_, count, immediate) in LOADS {
            let start = offset.wrapping_add(immediate);
            want.extend((0..count).map(|k| {
                let byte = start.wrapping_add(4 * k);
                data.get((byte / 4) as usize).copied().unwrap_or(0)
            }));
        }
        want.resize(OUTPUT_DWORDS as usize, 0);
        let expected = vec![want, vec![1 << WRITE_MARK]];
        let mut readbacks = Vec::new();
        for ((label, shader), (root, pso)) in [("narrow", &narrow), ("wide", &wide)]
            .into_iter()
            .zip(&pipelines)
        {
            let mut ud = vec![0; shader.user_data_dwords() as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot) {
                    (RootType::Constants, 0) => {
                        if u.dword == 0 {
                            offset
                        } else {
                            0
                        }
                    }
                    (RootType::Table, 1) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let out = host
                .dispatch(
                    root,
                    pso,
                    &Dispatch {
                        roots: vec![
                            (0, Root::Constants(ud)),
                            (
                                1,
                                Root::View {
                                    srv: true,
                                    data: vec![super::VIEW_MARK_NONE, 0, WRITE_MARK, 0],
                                },
                            ),
                            (2, Root::Uav(1)),
                        ],
                        views: &[
                            View::Buffer {
                                data: &data,
                                stride: Stride::Raw,
                            },
                            View::RawUav(OUTPUT_DWORDS),
                        ],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}, offset {offset:#x}: {e}"));
            assert_eq!(
                out, expected,
                "{label}, offset {offset:#x}: all readback dwords"
            );
            readbacks.push(out);
        }
        assert_eq!(
            readbacks[0], readbacks[1],
            "offset {offset:#x}: narrow versus wide"
        );
    }
}
