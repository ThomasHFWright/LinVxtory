//! Synthetic PR854/993 paired-wave regressions, usable without a device or game input.
//! Native checks exercise translation and binary PSV/host contracts; only the Windows
//! readback test executes generated shaders. Optional private inputs stay outside the repo.
#![cfg(test)]

use super::{HostShader, Options, lower};
use crate::dxbc::{Container, FourCC, u32_at};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

struct Overrides {
    wave: Option<u32>,
    pairs: Option<bool>,
}
impl Overrides {
    fn new(wave: u32, pairs: bool) -> Self {
        Self {
            wave: lower::TEST_HOST_WAVE.replace(Some(wave)),
            pairs: super::TEST_PAIRS.replace(Some(pairs)),
        }
    }
}
impl Drop for Overrides {
    fn drop(&mut self) {
        lower::TEST_HOST_WAVE.set(self.wave);
        super::TEST_PAIRS.set(self.pairs);
    }
}

const VCC: u32 = 106;
const EXEC: u32 = 126;
const LIT: u32 = 255;

const fn v(n: u32) -> u32 {
    256 + n
}

const fn k(n: u32) -> u32 {
    128 + n
}

const fn sop1(op: u32, sdst: u32, src0: u32) -> u32 {
    0xBE80_0000 | sdst << 16 | op << 8 | src0
}

const fn sop2(op: u32, sdst: u32, src0: u32, src1: u32) -> u32 {
    0x8000_0000 | op << 23 | sdst << 16 | src1 << 8 | src0
}

/// A branch by `imm` dwords from the next instruction (or `s_barrier`, `imm` 0).
const fn sopp(op: u32, imm: i16) -> u32 {
    0xBF80_0000 | op << 16 | imm as u16 as u32
}

const fn vop1(op: u32, vdst: u32, src0: u32) -> u32 {
    0x7E00_0000 | vdst << 17 | op << 9 | src0
}

const fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

const fn vopc(op: u32, src0: u32, vsrc1: u32) -> u32 {
    0x7C00_0000 | op << 17 | vsrc1 << 9 | src0
}

const fn vop3(op: u32, dst: u32, src0: u32, src1: u32) -> [u32; 2] {
    [0xD400_0000 | op << 16 | dst, src0 | src1 << 9]
}

const fn ds(op: u32, vdst: u32, addr: u32, data: u32, offset: u32) -> [u32; 2] {
    [
        0xD800_0000 | op << 18 | offset,
        vdst << 24 | data << 8 | addr,
    ]
}

/// `buffer_store_dword v<data>, v<addr>, s[8:11], 0 offen offset:<offset>`
const fn store(data: u32, addr: u32, offset: u32) -> [u32; 2] {
    [0xE070_1000 | offset, 0x8002_0000 | data << 8 | addr]
}

/// `v<regs>` stored as each thread's dwords of the UAV, the thread's index (v0) times 16 on.
fn stores(regs: [u32; 4]) -> Vec<u32> {
    let mut w = vec![vop2(0x1A, 15, k(4), 0)]; // v_lshlrev_b32 v15, 4, v0
    for (n, r) in regs.into_iter().enumerate() {
        w.extend(store(r, 15, 4 * n as u32));
    }
    w
}

fn native_with(body: &[u32], wave64: bool, threads: u32, lds: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(0xBF81_0000);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
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
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1 | lds.div_ceil(512) << 15, // 5 user SGPRs: s0..s4
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn lds_fixture() -> NativeProgram {
    let mut w = vec![vop2(0x1A, 1, k(2), 0)]; // v_lshlrev_b32 v1, 2, v0: this lane's dword
    w.extend(vop3(0x169, 2, v(0), k(7))); // v_mul_lo_u32 v2, v0, 7
    w.extend([
        vop2(0x26, 5, k(63), 0), // v_sub_nc_u32 v5, 63, v0: the mirror lane
        vop2(0x1A, 5, k(2), 5),  // v_lshlrev_b32 v5, 2, v5: its dword
        vop2(0x1B, 3, k(1), 0),  // v_and_b32 v3, 1, v0
        vopc(0xC2, k(0), 3),     // v_cmp_eq_u32 vcc, 0, v3: the even lanes
        sop1(0x24, 12, VCC),     // s_and_saveexec_b64 s[12:13], vcc
        sopp(0x08, 4),           // s_cbranch_execz past the even lanes' arm
        vop2(0x25, 4, LIT, 2),   // v_add_nc_u32 v4, 100, v2
        100,
    ]);
    w.extend(ds(0x0D, 0, 1, 4, 0)); // ds_write_b32 v1, v4
    w.extend([
        sop2(0x15, EXEC, 12, EXEC), // s_andn2_b64 exec, s[12:13], exec: the odd lanes
        sopp(0x08, 6),              // s_cbranch_execz past their arm
        vop2(0x25, 4, LIT, 2),      // v_add_nc_u32 v4, 200, v2
        200,
    ]);
    w.extend(ds(0x0D, 0, 1, 4, 0)); // ds_write_b32 v1, v4
    w.extend(ds(0x36, 6, 5, 0, 0)); // ds_read_b32 v6, v5: an even lane's, from the other arm
    w.extend([
        sop1(0x04, EXEC, 12), // s_mov_b64 exec, s[12:13]
        sop1(0x24, 12, VCC),  // s_and_saveexec_b64 s[12:13], vcc: the even lanes again
    ]);
    w.extend(ds(0x36, 6, 5, 0, 0)); // ds_read_b32 v6, v5: an odd lane's
    w.push(sop1(0x04, EXEC, 12)); // s_mov_b64 exec, s[12:13]
    w.extend(vop3(0x360, 14, v(6), k(5))); // v_readlane_b32 s14, v6, 5
    w.extend(vop3(0x360, 15, v(6), k(40))); // v_readlane_b32 s15, v6, 40
    w.extend([
        sop2(0x00, 14, 14, 15), // s_add_u32 s14, s14, s15
        vopc(0xC1, LIT, 6),     // v_cmp_lt_u32 vcc, 250, v6
        250,
        sop1(0x10, 16, VCC), // s_bcnt1_i32_b64 s16, vcc
    ]);
    w.extend(vop3(0x365, 7, VCC, k(0))); // v_mbcnt_lo_u32_b32 v7, vcc_lo, 0
    w.extend(vop3(0x366, 7, VCC + 1, v(7))); // v_mbcnt_hi_u32_b32 v7, vcc_hi, v7
    w.extend([
        vop1(0x01, 9, 14),  // v_mov_b32 v9, s14
        vop1(0x01, 10, 16), // v_mov_b32 v10, s16
    ]);
    w.extend(stores([6, 9, 10, 7]));
    native_with(&w, true, 64, 256)
}

fn loop_fixture() -> NativeProgram {
    let mut w = vec![
        vop2(0x16, 1, k(6), 0),  // v_lshrrev_b32 v1, 6, v0: the console wave
        vop2(0x1B, 2, k(63), 0), // v_and_b32 v2, 63, v0: the lane in it
        vop2(0x1A, 11, k(2), 1), // v_lshlrev_b32 v11, 2, v1: the wave's dword
        vop1(0x01, 12, k(0)),    // v_mov_b32 v12, 0
        sop1(0x04, 22, EXEC),    // s_mov_b64 s[22:23], exec
        vopc(0xD2, k(0), 2),     // v_cmpx_eq_u32 exec, 0, v2: lane 0
    ];
    w.extend(ds(0x0D, 0, 11, 12, 0)); // ds_write_b32 v11, v12
    w.extend([
        sop1(0x04, EXEC, 22),   // s_mov_b64 exec, s[22:23]
        vop2(0x1B, 3, k(3), 2), // v_and_b32 v3, 3, v2
        vop2(0x25, 3, k(1), 3), // v_add_nc_u32 v3, 1, v3
        vop2(0x25, 3, v(1), 3), // v_add_nc_u32 v3, v1, v3: the rounds
        vop1(0x01, 4, k(0)),    // v_mov_b32 v4, 0
        vop1(0x01, 13, k(1)),   // v_mov_b32 v13, 1
        sop1(0x04, 20, EXEC),   // s_mov_b64 s[20:21], exec
        vop2(0x25, 4, v(2), 4), // loop: v_add_nc_u32 v4, v2, v4
    ]);
    w.extend(ds(0x00, 0, 11, 13, 0)); // ds_add_u32 v11, v13
    w.extend([
        vop2(0x25, 3, 193, 3),       // v_add_nc_u32 v3, -1, v3
        vopc(0xC5, k(0), 3),         // v_cmp_ne_u32 vcc, 0, v3
        sop2(0x0F, EXEC, EXEC, VCC), // s_and_b64 exec, exec, vcc
        sopp(0x09, -7),              // s_cbranch_execnz loop
        sop1(0x04, EXEC, 20),        // s_mov_b64 exec, s[20:21]
    ]);
    w.extend(ds(0x36, 5, 11, 0, 0)); // ds_read_b32 v5, v11: the wave's total
    w.extend([
        sop1(0x04, 24, EXEC),          // s_mov_b64 s[24:25], exec
        vopc(0xD1, k(39), 2),          // v_cmpx_lt_u32 exec, 39, v2: lanes 40-63
        vop1(0x02, 6, v(4)),           // v_readfirstlane_b32 s6, v4
        sop1(0x04, EXEC, 24),          // s_mov_b64 exec, s[24:25]
        vopc(0xD1, k(19), 2),          // v_cmpx_lt_u32 exec, 19, v2: lanes 20-63
        vop1(0x02, 7, v(4)),           // v_readfirstlane_b32 s7, v4
        sop1(0x04, EXEC, 24),          // s_mov_b64 exec, s[24:25]
        vop1(0x01, 6, 6),              // v_mov_b32 v6, s6
        vop1(0x01, 9, 7),              // v_mov_b32 v9, s7
        0xD400_0000 | 0x346 << 16 | 6, // v_lshl_add_u32 v6, v9, 16, v6
        v(9) | k(16) << 9 | v(6) << 18,
        sopp(0x0A, 0),            // s_barrier
        vop2(0x1D, 14, k(4), 11), // v_xor_b32 v14, 4, v11: the other wave's dword
    ]);
    w.extend(ds(0x36, 7, 14, 0, 0)); // ds_read_b32 v7, v14
    w.extend(stores([4, 5, 6, 7]));
    native_with(&w, true, 128, 512)
}

fn permute_fixture() -> NativeProgram {
    let mut w = vop3(0x169, 1, v(0), k(3)).to_vec(); // v_mul_lo_u32 v1, v0, 3
    w.extend([
        vop2(0x25, 1, k(1), 1),  // v_add_nc_u32 v1, 1, v1
        vop2(0x26, 2, k(63), 0), // v_sub_nc_u32 v2, 63, v0
        vop2(0x1A, 2, k(2), 2),  // v_lshlrev_b32 v2, 2, v2: the mirror lane's address
    ]);
    w.extend(ds(0xB3, 3, 2, 1, 0)); // ds_bpermute_b32 v3, v2, v1
    w.extend([
        vop1(0x01, 4, LIT), // v_mov_b32 v4, 1000
        1000,
        vop1(0x01, 4, 0xFA), // v_mov_b32 v4, v1 row_shr:1 row_mask:0x6 bank_mask:0xf
        0x6 << 28 | 0xF << 24 | 0x111 << 8 | 1,
    ]);
    w.extend(ds(0x35, 5, 1, 0, 0x401F)); // ds_swizzle_b32 v5, v1 offset:swizzle(BITMASK_PERM, xor 16)
    w.push(vop1(0x01, 6, v(0))); // v_mov_b32 v6, v0
    w.extend(vop3(0x361, 6, 0, k(45))); // v_writelane_b32 v6, s0, 45
    w.extend(stores([3, 4, 5, 6]));
    native_with(&w, true, 64, 0)
}

fn branch_fixture() -> NativeProgram {
    let mut w = vec![
        sop1(0x03, 7, LIT), // s_mov_b32 s7, 0x77
        0x77,
        vop1(0x01, 5, k(1)),    // v_mov_b32 v5, 1
        vop2(0x16, 6, k(1), 0), // v_lshrrev_b32 v6, 1, v0
        vop2(0x1A, 7, v(6), 5), // v_lshlrev_b32 v7, v6, v5
        vopc(0xC4, 0, 0),       // v_cmp_gt_u32 vcc, s0, v0
        sop1(0x24, 12, VCC),    // s_and_saveexec_b64 s[12:13], vcc
        sopp(0x08, 20),         // s_cbranch_execz past the OR
    ];
    w.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    w.extend([
        sop1(0x04, EXEC, 12), // s_mov_b64 exec, s[12:13]
        vop1(0x01, 8, 7),     // v_mov_b32 v8, s7
    ]);
    w.extend(stores([8, 7, 6, 0]));
    native_with(&w, true, 64, 0)
}

#[derive(Clone, Copy)]
enum Form {
    Paired,
    Original,
    Selected,
    Local,
}

fn emit(
    native: &NativeProgram,
    marks: bool,
    form: Form,
) -> Result<(HostShader, Option<u32>), ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, native)?;
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes)?;
    let options = Options { view_marks: marks };
    match form {
        Form::Paired => super::paired(&c, native, &p, &io, &res, options),
        Form::Original => super::emit_program(&c, native, &p, &st, &io, &res, options),
        Form::Selected => super::emit_with(&c, native, &p, &st, &io, &res, options).map(|s| {
            let (_, min, max) = runtime(&s);
            let wave = (min == max).then_some(min);
            (s, wave)
        }),
        Form::Local => {
            lower::with_local_waves(|| super::emit_program(&c, native, &p, &st, &io, &res, options))
        }
    }
}

// Decode the emitted binary contract, not strings in the bitcode payload.
fn runtime(shader: &HostShader) -> ([u32; 3], u32, u32) {
    let c = Container::parse(&shader.dxil).expect("emitted container");
    assert!(c.part(FourCC::DXIL).is_some(), "missing executable part");
    let psv = c.part(FourCC::PSV0).expect("PSV0").data;
    assert!(u32_at(psv, 0).unwrap() >= 48);
    assert_eq!(psv[4 + 24], 5, "compute stage");
    let word = |at| u32_at(psv, at).unwrap();
    ([word(40), word(44), word(48)], word(20), word(24))
}

fn has_binding(shader: &HostShader, space: u32, register: u32) -> bool {
    let c = Container::parse(&shader.dxil).unwrap();
    let psv = c.part(FourCC::PSV0).unwrap().data;
    let at = 4 + u32_at(psv, 0).unwrap() as usize;
    let count = u32_at(psv, at).unwrap() as usize;
    if count == 0 {
        return false;
    }
    let stride = u32_at(psv, at + 4).unwrap() as usize;
    assert!(stride >= 16);
    (0..count).any(|n| {
        let b = at + 8 + n * stride;
        u32_at(psv, b + 4).unwrap() == space && u32_at(psv, b + 8).unwrap() == register
    })
}

#[test]
fn paired_synthetic_bodies_require_wave32_and_halve_the_group() {
    let _scope = Overrides::new(32, true);
    for native in [
        lds_fixture(),
        loop_fixture(),
        permute_fixture(),
        branch_fixture(),
    ] {
        let StageIo::Cs(cs) = &native.io else {
            unreachable!()
        };
        for marks in [false, true] {
            let (shader, wave) = emit(&native, marks, Form::Paired).expect("paired synthetic body");
            assert_eq!(wave, Some(32));
            assert_eq!(
                runtime(&shader),
                ([cs.threads.iter().product::<u32>() / 2, 1, 1], 32, 32)
            );
            assert_eq!(shader.view_marks, marks);
            for register in [super::VIEW_MARKS, super::VIEW_WRITES] {
                assert_eq!(
                    has_binding(&shader, super::MEMORY_SPACE, register),
                    marks,
                    "view-mark binding {register}"
                );
            }
        }
    }
}

#[test]
fn paired_group_guards_use_total_threads_including_2d_groups() {
    let _scope = Overrides::new(32, true);
    for threads in [[32, 1, 1], [64, 1, 1], [96, 1, 1], [128, 1, 1], [8, 8, 1]] {
        let mut native = permute_fixture();
        let StageIo::Cs(cs) = &mut native.io else {
            unreachable!()
        };
        cs.threads = threads;
        let count = threads.iter().product::<u32>();
        let result = emit(&native, false, Form::Paired);
        if count % 64 == 0 {
            let (shader, wave) = result.expect("complete console waves");
            assert_eq!(wave, Some(32));
            assert_eq!(runtime(&shader), ([count / 2, 1, 1], 32, 32));
        } else {
            assert!(matches!(
                result,
                Err(ShaderError::Unsupported {
                    offset: 0,
                    what: "paired lanes in a thread group that does not fill its console waves"
                })
            ));
        }
    }
}

#[test]
fn paired_refuses_noncompute_before_lowering() {
    let _scope = Overrides::new(32, true);
    let mut native = permute_fixture();
    let p = ir::build(&native.code, true, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    native.stage = HwStage::Vs;
    native.io = StageIo::None;
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).unwrap();
    assert!(matches!(
        super::paired(&c, &native, &p, &io, &res, Options::default()),
        Err(ShaderError::Unsupported {
            offset: 0,
            what: "paired lanes outside compute"
        })
    ));
}

#[test]
fn paired_retains_all_host_annotations_and_options() {
    let native = permute_fixture();
    for marks in [false, true] {
        let (mut original, wave) = {
            let _scope = Overrides::new(64, false);
            emit(&native, marks, Form::Original).expect("original wave64")
        };
        assert_eq!(wave, Some(64));
        assert_eq!(runtime(&original), ([64, 1, 1], 64, 64));
        let (mut paired, _) = {
            let _scope = Overrides::new(32, true);
            emit(&native, marks, Form::Paired).expect("paired wave32")
        };
        // All HostShader fields besides the executable remain the host's same contract.
        original.dxil.clear();
        paired.dxil.clear();
        assert_eq!(paired, original);
        assert_eq!(paired.view_marks, marks);
    }
}

#[test]
fn exact_originals_stay_byte_identical_with_pairs_enabled_or_disabled() {
    for (wave64, host_wave) in [(false, 32), (true, 64)] {
        let mut native = permute_fixture();
        if !wave64 {
            native = native_with(&stores([0, 0, 0, 0]), false, 32, 0);
        }
        for marks in [false, true] {
            let (original, required) = {
                let _scope = Overrides::new(host_wave, false);
                emit(&native, marks, Form::Original).expect("exact original")
            };
            for pairs in [false, true] {
                let _scope = Overrides::new(host_wave, pairs);
                let (selected, selected_wave) =
                    emit(&native, marks, Form::Selected).expect("exact selection");
                assert_eq!(selected_wave, required);
                assert_eq!(selected, original);
            }
        }
    }
}

#[test]
fn paired_is_selected_before_a_successful_approximate_half_wave() {
    let native = branch_fixture();
    let _scope = Overrides::new(32, true);
    for marks in [false, true] {
        let (paired, wave) = emit(&native, marks, Form::Paired).expect("paired branch");
        assert_eq!(wave, Some(32));
        let (approximate, wave) =
            emit(&native, marks, Form::Local).expect("approximate branch also lowers");
        assert_ne!(wave, Some(64));
        assert_ne!(approximate.dxil, paired.dxil);
        let (selected, _) = emit(&native, marks, Form::Selected).expect("paired retry");
        assert_eq!(
            selected, paired,
            "selection must prefer the exact paired body"
        );
        let _off = Overrides::new(32, false);
        // Respect developer opt-outs of approximate retries without changing process env.
        let approximate_enabled =
            std::env::var_os("VICTORY_WAVE_LOCAL").is_none_or(|v| v != "0" && v != "idiom");
        if approximate_enabled && std::env::var_os("VICTORY_WAVE_WIDE").is_none_or(|v| v != "1") {
            let (baseline, _) =
                emit(&native, marks, Form::Selected).expect("disabled-pairs baseline");
            assert_eq!(baseline, approximate);
        }
    }
}

#[test]
fn paired_choices_and_host_wave_are_in_the_translation_fingerprint() {
    let fingerprint = {
        let _scope = Overrides::new(32, true);
        super::translation_fingerprint()
    };
    assert!(fingerprint.starts_with(crate::SOURCE_HASH.as_bytes()));
    for name in ["VICTORY_WAVE_PAIRS", "VICTORY_WAVE_RAW"] {
        assert!(
            env!("VICTORY_SHADER_SWITCHES")
                .split(',')
                .any(|s| s == name)
        );
        let mut entry = format!("{name}=").into_bytes();
        if let Some(value) = std::env::var_os(name) {
            entry.extend_from_slice(value.as_encoded_bytes());
        }
        entry.push(0);
        assert!(
            fingerprint.windows(entry.len()).any(|w| w == entry),
            "{name}"
        );
    }
    let _scope = Overrides::new(64, true);
    assert_ne!(super::translation_fingerprint(), fingerprint);
}

#[test]
fn overrides_restore_nested_choices() {
    let before = (lower::TEST_HOST_WAVE.get(), super::TEST_PAIRS.get());
    {
        let _outer = Overrides::new(32, true);
        assert!(super::pairs_on());
        {
            let _inner = Overrides::new(64, false);
            assert!(!super::pairs_on());
            assert_eq!(lower::TEST_HOST_WAVE.get(), Some(64));
        }
        assert!(super::pairs_on());
        assert_eq!(lower::TEST_HOST_WAVE.get(), Some(32));
    }
    assert_eq!(
        (lower::TEST_HOST_WAVE.get(), super::TEST_PAIRS.get()),
        before
    );
}

/// Opt in with a private directory containing only the desired .cdxil inputs.
/// Never writes files, embeds private paths, or requires game data in the repository.
#[test]
fn optional_external_wave_shader_directory_translates_on_wave32() {
    let Some(dir) = std::env::var_os("VICTORY_TEST_WAVE_SHADER_DIR") else {
        eprintln!("VICTORY_TEST_WAVE_SHADER_DIR unset; external fixture check skipped");
        return;
    };
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("external fixture directory")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "cdxil"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "explicit fixture directory has no .cdxil files"
    );
    let _scope = Overrides::new(32, true);
    let mut failed = Vec::new();
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy();
        let input = std::fs::read(&path).expect("read external fixture");
        match super::translate_with(&input, Options { view_marks: true }) {
            Ok(shader) => {
                let (threads, min, max) = runtime(&shader);
                eprintln!(
                    "{name}: translated, group={threads:?}, wave={min}..{max}, view_marks={}",
                    shader.view_marks
                );
                // Unconstrained output (0..MAX) is also legal on a wave32 host.
                if max == 64 || min > 32 || max < 32 {
                    failed.push(name.into_owned());
                }
            }
            Err(error) => {
                eprintln!("{name}: translation failed: {error}");
                failed.push(name.into_owned());
            }
        }
    }
    assert!(failed.is_empty(), "wave32 translation failed: {failed:?}");
}

#[cfg(windows)]
mod gpu {
    use super::super::host::{Dispatch, Host, Root, View};
    use super::*;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
    };

    fn run(host: &Host, shader: &HostShader, s0: u32) -> Vec<u32> {
        let (root, pso) = host
            .compute_pipeline(shader)
            .expect("paired pipeline on wave32-capable device");
        let mut ud = vec![0; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = match (u.root, u.slot, u.dword) {
                (RootType::Constants, 0, 0) => s0,
                (RootType::Constants, 0, 1..=3) | (RootType::Table, 1, 0) => 0,
                other => panic!("unexpected synthetic root input {other:?}"),
            };
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(256)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("paired dispatch");
        assert_eq!(out.len(), 1);
        out.into_iter().next().unwrap()
    }

    #[test]
    fn paired_cross_half_permutation_and_branch_readbacks() {
        let _one = super::super::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM6.6 hardware device; paired readback skipped");
            return;
        };
        let mut caps = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS1,
                (&mut caps as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS1).cast(),
                std::mem::size_of_val(&caps) as u32,
            )
        }
        .expect("wave capabilities");
        if !caps.WaveOps.as_bool() || caps.WaveLaneCountMin > 32 || caps.WaveLaneCountMax < 32 {
            eprintln!("device cannot run wave32; paired readback skipped");
            return;
        }
        let _scope = Overrides::new(32, true);
        let (permutation, _) = emit(&permute_fixture(), false, Form::Paired).unwrap();
        let (branch, _) = emit(&branch_fixture(), false, Form::Selected).unwrap();
        for shader in [&permutation, &branch] {
            if let Ok(mut validator) = super::super::validator::Validator::load(
                super::super::validator::Validator::default_path().as_deref(),
            ) {
                validator
                    .validate(&mut shader.dxil.clone())
                    .expect("paired DXIL validation");
            }
        }
        let s0 = 0xABCD_1234;
        let x = |n: u32| 3 * n + 1;
        let want: Vec<_> = (0..64)
            .flat_map(|n| {
                let shifted = if matches!(n >> 4, 1 | 2) && n & 15 != 0 {
                    x(n - 1)
                } else {
                    1000
                };
                [
                    x(63 - n),
                    shifted,
                    x((n & 32) | ((n & 31) ^ 16)),
                    if n == 45 { s0 } else { n },
                ]
            })
            .collect();
        assert_eq!(run(&host, &permutation, s0), want);
        for below in [0, 1, 10, 32, 33, 40, 64] {
            let reduced = if below == 0 {
                0x77
            } else {
                (0..below).fold(0u32, |mask, lane| mask | (1 << (lane >> 1)))
            };
            let want: Vec<_> = (0..64)
                .flat_map(|lane| [reduced, 1 << (lane >> 1), lane >> 1, lane])
                .collect();
            assert_eq!(run(&host, &branch, below), want, "branch threshold {below}");
        }
    }
}
