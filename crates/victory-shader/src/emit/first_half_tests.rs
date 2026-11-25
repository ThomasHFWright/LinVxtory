use super::*;
use crate::native::{ComputeIo, Usage};

#[test]
fn first_half_partners_are_only_enabled_during_the_retry() {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [1, 1, 1],
        }),
        code: [
            0xC808_0000u32,
            0xC809_0001,
            0xD75A_0002,
            0x040A_0200,
            0xBF81_0000,
        ]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect(),
    };
    let p = crate::ir::build(&native.code, false, native.vgprs).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = crate::io::Io {
        interps: vec![(
            (0, 1),
            Interp::Interpolated {
                attribute: crate::io::Attribute {
                    usage: Usage::Generic,
                    row: 0,
                    channel: 0,
                    high: false,
                },
                barycentric: crate::io::Barycentric::PerspectiveCenter,
                first: vec![(0, 0)],
                unless: vec![],
            },
        )],
        ..Default::default()
    };
    let mut parts = Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
        view_marks: false,
    };
    let bad = HashMap::new();
    assert!(!FIRST_HALF_PAIRS.get());
    assert!(!Lower::new(&parts, &bad).interps.contains_key(&(0, 0)));
    let result: Result<(), Why> = with_first_half_pairs(|| {
        let lower = Lower::new(&parts, &bad);
        let enabled = std::env::var_os("VICTORY_FIRST_HALF").is_none_or(|v| v != "0");
        assert_eq!(lower.interps.contains_key(&(0, 0)), enabled);
        if enabled {
            assert_eq!(lower.interps[&(0, 0)], lower.interps[&(0, 1)]);
        }
        with_first_half_pairs(|| assert!(FIRST_HALF_PAIRS.get()));
        assert!(FIRST_HALF_PAIRS.get());
        Err(FIRST_HALF)
    });
    assert_eq!(result, Err(FIRST_HALF));
    assert!(!FIRST_HALF_PAIRS.get());
    assert!(!Lower::new(&parts, &bad).interps.contains_key(&(0, 0)));

    let empty = crate::io::Io::default();
    parts.io = &empty;
    let mut lower = Lower::new(&parts, &bad);
    let first = &p.blocks[0].insts[0];
    assert_eq!(lower.interp(first, (0, 0)), Err(FIRST_HALF));
    let value = first.write(Reg::V(2)).unwrap();
    assert!(
        !lower.vals.contains_key(&value),
        "an unmapped first half must not invent a register value"
    );
    assert_eq!(lower.interp(&p.blocks[0].insts[1], (0, 1)), Err(FIRST_HALF));

    let half = &p.blocks[0].insts[2];
    let old = half.read(Reg::V(2)).unwrap();
    let new = half.write(Reg::V(2)).unwrap();
    lower.vals.insert(old, Val::Missing(FIRST_HALF));
    let bits = lower.s.m.i32(0x3C00);
    lower.write_half(half, bits, None).unwrap();
    assert_eq!(lower.i32_of(new), Err(FIRST_HALF));
    assert!(
        lower.low16.contains_key(&new),
        "the written low half remains readable"
    );

    let mut high = half.clone();
    let Fields::Valu(v) = &mut high.fields else {
        unreachable!()
    };
    v.opsel |= 8;
    assert_eq!(lower.write_half(&high, bits, None), Err(FIRST_HALF));
    // A high write can complete a previously known low half.
    lower.low16.insert(old, bits);
    lower.write_half(&high, bits, None).unwrap();
    assert!(lower.i32_of(new).is_ok());
}
