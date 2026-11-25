//! Synthetic inspection of emitted mesh counts and reachable primitive writes.
//! No DXC, GPU, or game fixtures.

use super::*;
use crate::emit::bitcode::TestCall;
use crate::emit::shader::Mesh;
use crate::native::ComputeIo;

fn prologue(lane: u32, primitives: u32, output_rows: u32) -> Result<(Mesh, Vec<TestCall>), Why> {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [128, 1, 1],
        }),
        code: 0xBF81_0000u32.to_le_bytes().to_vec(), // s_endpgm
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
        outputs: (0..output_rows)
            .map(|row| SigElement {
                name: "TEXCOORD".into(),
                index: row,
                kind: 0,
                comp: 9,
                interp: 0,
                row: Some(row),
                col: 0,
                cols: 4,
                usage: 0xf,
                half: false,
            })
            .collect(),
        prims: vec![],
        mesh_stride: 0,
        view_marks: false,
    };
    let bad = HashMap::new();
    let mut lower = Lower::new(&parts, &bad);
    let mesh = Mesh {
        max_vertices: 256,
        max_primitives: primitives,
        topology: 2,
    };
    lower.s.mesh = Some(mesh);
    // Substitute only the lane id; exercise the actual count, budget, cull
    // and branch emission, including folding away out-of-range tail writes.
    lower.flat = Some(lower.s.m.i32(lane));
    lower.mesh_prologue(mesh)?;
    lower.s.m.terminate(lower.cur, Term::Ret);
    Ok((lower.s.mesh.unwrap(), lower.s.m.folded_calls()))
}

#[test]
fn all_declared_primitives_start_culled_even_past_the_thread_count() {
    for primitives in [1, 127, 128, 129, 193, 256] {
        let mut writes = vec![0; primitives as usize];
        for lane in 0..128 {
            let (mesh, calls) = prologue(lane, primitives, 1).unwrap();
            assert_eq!(mesh.max_primitives, primitives);
            for call in calls.iter().filter(|c| c.name == "dx.op.emitIndices") {
                assert_eq!(call.args.len(), 5);
                let index = call.args[1][0];
                assert!(
                    index < primitives,
                    "lane={lane}, count={primitives}, index={index}"
                );
                assert_eq!(&call.args[2..], &[vec![0], vec![0], vec![0]]);
                writes[index as usize] += 1;
            }
        }
        // Missing later passes leave zeros; an unguarded partial pass writes
        // out of range above. Each declared primitive must be initialized once.
        assert_eq!(writes, vec![1; primitives as usize], "count={primitives}");
    }
}

#[test]
fn mesh_budget_clamp_updates_metadata_and_emitted_counts_together() {
    // Nine float4 rows use 36 KiB at 256 vertices, but 18 KiB at 128.
    let (mesh, calls) = prologue(0, 256, 9).unwrap();
    assert_eq!((mesh.max_vertices, mesh.max_primitives), (128, 128));
    let counts: Vec<_> = calls
        .iter()
        .filter(|c| c.name == "dx.op.setMeshOutputCounts")
        .collect();
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].args, vec![vec![168], vec![128], vec![128]]);
}

#[test]
fn mesh_budget_remaining_over_limit_is_only_rejected_in_strict_mode() {
    // Seventeen float4 rows still need 34 KiB after clamping to 128 vertices.
    // Respect the process setting without mutating the environment of other tests.
    let result = prologue(0, 256, 17);
    if std::env::var_os("VICTORY_MESH_OUTPUT_STRICT").is_some() {
        assert!(matches!(
            result,
            Err("a mesh shader whose outputs pass the host's 32 KiB")
        ));
    } else {
        let (mesh, _) = result.unwrap();
        assert_eq!((mesh.max_vertices, mesh.max_primitives), (128, 128));
    }
}
