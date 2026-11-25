//! Synthetic native instructions through IR, resource tracking and the emitter.
//! No validator, device, game input, or execution of generated shaders.

use super::super::*;
use crate::emit::bitcode::TestCall;
use crate::native::{ComputeIo, RootParam};

const END: u32 = 0xBF81_0000;
const SAVE_EXEC: u32 = 0xBE94_037E; // s_mov_b32 s20, exec_lo
const ONLY_LANE_0: u32 = 0x7DA4_0080; // v_cmpx_eq_u32 exec_lo, 0, v0
const RESTORE_EXEC: u32 = 0xBEFE_0314; // s_mov_b32 exec_lo, s20

const fn vop1(op: u32, dst: u32, src: u32) -> u32 {
    0x7E00_0000 | dst << 17 | op << 9 | src
}

const fn add(dst: u32, a: u32, b: u32) -> u32 {
    0x03 << 25 | dst << 17 | b << 9 | (256 + a)
}

fn buffer(op: u32, data: u32, descriptor: u32) -> [u32; 2] {
    [
        0x38 << 26 | op << 18 | 1 << 13,
        0x80 << 24 | (descriptor >> 2) << 16 | data << 8,
    ]
}

fn probe(body: &[u32], dynamic: bool) -> (Vec<(u8, u32)>, Vec<TestCall>) {
    let mut words = vec![0xBE85_0380, vop1(1, 7, 128)]; // s5 = 0, v7 = 0
    if dynamic {
        words.push(vop1(2, 21, 256)); // s21 = first lane's v0
    }
    for (dst, offset) in [(8, 0), (12, 32), (16, 64)] {
        words.extend([
            0x3D << 26 | 2 << 18 | dst << 6 | 2,
            if dynamic && dst == 8 {
                21 << 25
            } else {
                125 << 25 | offset
            },
        ]);
    }
    words.extend_from_slice(body);
    // Keep the result and any descriptor-size query observable in raw output.
    for (data, offset) in [(2, 0), (7, 4)] {
        words.extend([
            0x38 << 26 | 0x1C << 18 | offset,
            0x80 << 24 | 4 << 16 | data << 8,
        ]);
    }
    words.push(END);
    let view = |kind, offset, stride| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: offset,
        lower_bound: offset / 8,
        stride,
        dwords: 4,
        user_data: 4,
        slot: 1,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
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
        used: vec![
            view(ResourceType::Srv, 0, 4),
            view(ResourceType::Uav, 8, 4),
            view(ResourceType::Uav, 16, 0),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = crate::ir::build(&native.code, false, 8).expect("IR");
    let st = crate::structure::structure(&p).expect("structure");
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st))
        .expect("resource tracking");
    let io = crate::io::raise(&p, &native).expect("IO");
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
    let mut lower = Lower::new(&parts, &bad);
    match lower.emit_body() {
        Ok(()) => {}
        Err(Stop::Fail(e)) => panic!("synthetic half views: {e}"),
        Err(Stop::Retry(v, why)) => panic!("synthetic half views retry {v:?}: {why}"),
    }
    let views = lower.half_views.iter().copied().collect();
    (views, lower.s.m.folded_calls())
}

fn count(calls: &[TestCall], name: &str) -> usize {
    calls.iter().filter(|c| c.name == name).count()
}

fn typed_properties(calls: &[TestCall], kind: u32) -> Vec<u32> {
    calls
        .iter()
        .filter(|c| c.name == "dx.op.annotateHandle")
        .filter_map(|c| match c.args[2].as_slice() {
            [k, component] if *k == kind => Some(*component),
            _ => None,
        })
        .collect()
}

#[test]
fn pure_fixed_d16_load_and_store_keep_integer_views_and_operations() {
    let body = [
        &buffer(0x83, 1, 8)[..], // load d16 xyzw
        &buffer(0x87, 1, 12),   // store d16 xyzw
        &[vop1(1, 7, 10)],     // read descriptor record count, exercising property selection
    ]
    .concat();
    let (views, calls) = probe(&body, false);
    assert_eq!(views, [(1, 0), (1, 8)]);
    assert_eq!(count(&calls, "dx.op.bufferLoad.i32"), 1);
    assert_eq!(count(&calls, "dx.op.bufferStore.i32"), 1);
    assert_eq!(count(&calls, "dx.op.bufferLoad.f32"), 0);
    assert_eq!(count(&calls, "dx.op.bufferStore.f32"), 0);
    assert!(count(&calls, "dx.op.getDimensions") > 0);
    for kind in [10, 0x100a] {
        let props = typed_properties(&calls, kind);
        assert!(!props.is_empty());
        assert!(props.iter().all(|&p| p == 0x405));
    }
}

#[test]
fn mixed_d16_and_whole_float_use_compatible_float_bindings() {
    let body = [
        &buffer(0x83, 1, 8)[..],
        &buffer(0, 3, 8), // same source, ordinary format load used as f32
        &[add(2, 3, 2)],
        &buffer(0x87, 1, 12),
        &[vop1(1, 7, 10)],
    ]
    .concat();
    let (views, calls) = probe(&body, false);
    assert_eq!(views, [(1, 8)]); // the unaliased, pure d16 destination stays raw
    assert_eq!(count(&calls, "dx.op.bufferLoad.i32"), 0);
    assert_eq!(count(&calls, "dx.op.bufferLoad.f32"), 2);
    assert_eq!(count(&calls, "dx.op.bufferStore.i32"), 1);
    assert!(count(&calls, "dx.op.getDimensions") > 0);
    let props = typed_properties(&calls, 10);
    assert!(!props.is_empty());
    assert!(props.iter().all(|&p| p == 0x409));
}

#[test]
fn dynamic_source_excludes_fixed_d16_destination_in_same_table() {
    let body = [&buffer(0x83, 1, 8)[..], &buffer(0x87, 1, 12)].concat();
    let (views, calls) = probe(&body, true);
    assert!(views.is_empty());
    assert_eq!(count(&calls, "dx.op.bufferLoad.i32"), 0);
    assert_eq!(count(&calls, "dx.op.bufferStore.i32"), 0);
    assert_eq!(count(&calls, "dx.op.bufferLoad.f32"), 1);
    assert_eq!(count(&calls, "dx.op.bufferStore.f32"), 1);
    // No alias-range proof: even the fixed destination loses raw-bit coverage.
    for kind in [10, 0x100a] {
        let props = typed_properties(&calls, kind);
        assert!(!props.is_empty());
        assert!(props.iter().all(|&p| p == 0x409));
    }
}

#[test]
fn masked_rewrite_carries_old_lanes_to_later_whole_use() {
    let body = [
        &buffer(0, 1, 8)[..],
        &[
            vop1(0x0b, 2, 257), // allowed half read
            SAVE_EXEC,
            ONLY_LANE_0,
            vop1(1, 1, 128), // v1 = 0 in active lanes; other lanes keep loaded bits
            RESTORE_EXEC,
            add(2, 1, 2), // whole use also reaches the preserved old lanes
        ],
    ]
    .concat();
    let (views, calls) = probe(&body, false);
    assert!(views.is_empty());
    assert_eq!(count(&calls, "dx.op.bufferLoad.f32"), 1);
    assert_eq!(count(&calls, "dx.op.bufferLoad.i32"), 0);
}

#[test]
fn masked_in_place_half_conversion_also_carries_old_lanes() {
    let body = [
        &buffer(0, 1, 8)[..],
        &[
            SAVE_EXEC,
            ONLY_LANE_0,
            vop1(0x0b, 1, 257), // explicit half read AND implicit preservation
            RESTORE_EXEC,
            add(2, 1, 1),
        ],
    ]
    .concat();
    let (views, calls) = probe(&body, false);
    assert!(views.is_empty());
    assert_eq!(count(&calls, "dx.op.bufferLoad.f32"), 1);
    assert_eq!(count(&calls, "dx.op.bufferLoad.i32"), 0);
}

#[test]
fn half_read_followed_by_dead_masked_rewrite_remains_half_only() {
    let body = [
        &[
            vop1(1, 2, 128),
            SAVE_EXEC,
            ONLY_LANE_0,
            0xBF88_0003, // skip load (2 words) and conversion (1 word)
        ][..],
        &buffer(0, 1, 8),
        &[
            vop1(0x0b, 2, 257),
            RESTORE_EXEC,
            SAVE_EXEC,
            ONLY_LANE_0,
            vop1(1, 1, 128),
            RESTORE_EXEC,
        ],
    ]
    .concat();
    let (views, calls) = probe(&body, false);
    assert_eq!(views, [(1, 0)]);
    assert_eq!(count(&calls, "dx.op.bufferLoad.i32"), 1);
    assert_eq!(count(&calls, "dx.op.bufferLoad.f32"), 0);
}
