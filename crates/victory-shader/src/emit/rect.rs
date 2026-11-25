//! Rectangle lists completed by a geometry shader: find the right angle
//! opposite the longest side in x/y, then emit the original triangle and
//! its completion `(va, vb, va + vb - vc)`. Ties and NaN select corner 1.
//! Integer attributes at the fourth corner keep `va`'s value. The optional
//! pixel signature adds a primitive-ID output when the vertex stage has none.

use super::bitcode::{Attr, Bin, Term, V, pred};
use super::container::SigElement;
use super::shader::Shader;
use crate::ShaderError;
use crate::dxbc::{Container, FourCC};

/// Vertices emitted per rectangle: two triangles.
const VERTICES: u32 = 6;

/// The scalars a geometry shader may emit per invocation (vertices times
/// output components).
const MAX_SCALARS: u32 = 1024;

/// The geometry shader (a DXIL container) for rectangle lists drawn with
/// the translated vertex shader `vs` and, if the pipeline has one, the
/// translated pixel shader `ps` (both as [`super::HostShader::dxil`]). Its
/// inputs and outputs are the vertex shader's outputs, register for
/// register, so it links with both. A pixel shader that declares a
/// primitive id gets the rectangle's number, which the host leaves to the
/// geometry stage when there is one.
pub fn rect_list_gs(vs: &[u8], ps: Option<&[u8]>) -> Result<Vec<u8>, ShaderError> {
    let sig = |dxil: &[u8], fourcc| -> Result<Vec<SigElement>, ShaderError> {
        let c = Container::parse(dxil)?;
        match c.part(fourcc) {
            Some(part) => super::signature(part.data),
            None => Ok(Vec::new()),
        }
    };
    let outputs = sig(vs, FourCC::OSG1)?;
    let wanted = match ps {
        Some(ps) => sig(ps, FourCC::ISG1)?,
        None => Vec::new(),
    };
    build(outputs, &wanted)
}

/// [`rect_list_gs`] from the vertex shader's outputs and the pixel
/// shader's inputs.
fn build(mut outputs: Vec<SigElement>, ps_inputs: &[SigElement]) -> Result<Vec<u8>, ShaderError> {
    let unsupported = |what| ShaderError::Unsupported { offset: 0, what };
    if outputs.iter().any(|e| e.row.is_none()) {
        return Err(unsupported("a rectangle list output without a register"));
    }
    let Some(position) = outputs
        .iter()
        .position(|e| e.kind == 3 && e.comp == 9 && e.cols == 4)
    else {
        return Err(unsupported("a rectangle list without a four-component position"));
    };
    for e in &mut outputs {
        // Interpolation as `emit_program` declares a vertex shader's
        // outputs; a geometry shader's inputs carry the same modes.
        e.interp = match (e.kind, e.comp) {
            (3, _) => 4,
            (0 | 6 | 7, 9) => 2,
            (0, _) => 1,
            _ => 0,
        };
        e.usage = (1 << e.cols) - 1;
    }
    let mut s = Shader::new(false);
    s.gs = Some(VERTICES);
    s.inputs = outputs.clone();
    // The primitive id the pixel shader declares, as one more output.
    let wanted = ps_inputs
        .iter()
        .find(|e| e.kind == 10 && e.row.is_some())
        .filter(|_| !outputs.iter().any(|o| o.kind == 10));
    let primitive = match wanted {
        Some(e) => {
            // Alone in its columns, and in a row of constant elements.
            let clash = outputs.iter().any(|o| {
                let overlap = o.col < e.col + e.cols && e.col < o.col + o.cols;
                o.row == e.row && (o.interp != 1 || overlap)
            });
            if clash {
                return Err(unsupported(
                    "a rectangle list whose primitive id shares a register with a vertex output",
                ));
            }
            outputs.push(SigElement {
                interp: 1,
                usage: (1 << e.cols) - 1,
                ..e.clone()
            });
            Some(outputs.len() - 1)
        }
        None => None,
    };
    let scalars: u32 = outputs.iter().map(|e| u32::from(e.cols)).sum();
    if scalars * VERTICES > MAX_SCALARS {
        return Err(unsupported(
            "a rectangle list with more output components than a geometry shader emits",
        ));
    }
    s.outputs = outputs;
    let b = s.entry;

    // Every input of the three vertices: [element][column][vertex].
    let mut corners: Vec<Vec<[V; 3]>> = Vec::new();
    for id in 0..s.inputs.len() {
        let mut columns = Vec::new();
        for col in 0..s.inputs[id].cols {
            columns.push([0, 1, 2].map(|vertex| load(&mut s, id, col, vertex)));
        }
        corners.push(columns);
    }

    // Match the upstream right-angle rule, including ties and NaN.
    let side = |s: &mut Shader, i: usize, j: usize| {
        let mut sum = None;
        for values in corners[position].iter().take(2) {
            let d = s.m.bin(b, Bin::Sub, values[i], values[j]);
            let sq = s.m.bin(b, Bin::Mul, d, d);
            sum = Some(match sum {
                Some(x) => s.m.bin(b, Bin::Add, x, sq),
                None => sq,
            });
        }
        sum.expect("two columns")
    };
    let (d12, d20, d01) = (side(&mut s, 1, 2), side(&mut s, 2, 0), side(&mut s, 0, 1));
    let longest = |s: &mut Shader, d: V, others: [V; 2]| {
        let [x, y] = others.map(|o| s.m.cmp(b, pred::OGT, d, o));
        s.m.bin(b, Bin::And, x, y)
    };
    let (at0, at2) = (longest(&mut s, d12, [d20, d01]), longest(&mut s, d01, [d12, d20]));
    let pick = |s: &mut Shader, [x0, x1, x2]: [V; 3]| {
        let x = s.m.select(b, at2, x2, x1);
        s.m.select(b, at0, x0, x)
    };

    // Original triangle followed by the upstream completing triangle.
    let mut emitted: Vec<Vec<[V; 6]>> = Vec::new();
    for (id, columns) in corners.iter().enumerate() {
        let mut out = Vec::new();
        for &[p0, p1, p2] in columns {
            let va = pick(&mut s, [p2, p0, p1]);
            let vb = pick(&mut s, [p1, p2, p0]);
            let fourth = if s.inputs[id].comp == 9 {
                let vc = pick(&mut s, [p0, p1, p2]);
                let sum = s.m.bin(b, Bin::Add, va, vb);
                s.m.bin(b, Bin::Sub, sum, vc)
            } else {
                va
            };
            out.push([p0, p1, p2, va, vb, fourth]);
        }
        emitted.push(out);
    }

    // The rectangle's number: the triangle's, three vertices a rectangle.
    let mut number = None;
    if let Some(id) = primitive {
        let n = s.op(b, "primitiveID.i32", Attr::ReadNone, s.t.i32, 108, &[]);
        number = Some((id, n));
    }
    let stream = s.m.i8(0);
    for vertex in 0..VERTICES as usize {
        for (id, columns) in emitted.iter().enumerate() {
            let name = if s.outputs[id].comp == 9 {
                "storeOutput.f32"
            } else {
                "storeOutput.i32"
            };
            for (col, values) in columns.iter().enumerate() {
                let args = [
                    s.m.i32(id as u32),
                    s.m.i32(0),
                    s.m.i8(col as u8),
                    values[vertex],
                ];
                s.op(b, name, Attr::NoUnwind, s.t.void, 5, &args);
            }
        }
        if let Some((id, n)) = number {
            let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(0), n];
            s.op(b, "storeOutput.i32", Attr::NoUnwind, s.t.void, 5, &args);
        }
        s.op(b, "emitStream", Attr::NoUnwind, s.t.void, 97, &[stream]);
        // Each triangle is its own strip.
        if vertex == 2 || vertex == 5 {
            s.op(b, "cutStream", Attr::NoUnwind, s.t.void, 98, &[stream]);
        }
    }
    s.m.terminate(b, Term::Ret);
    Ok(s.finish())
}

/// Column `col` of input `id` at vertex `vertex` of the triangle.
fn load(s: &mut Shader, id: usize, col: u8, vertex: u32) -> V {
    let (name, ty) = if s.inputs[id].comp == 9 {
        ("loadInput.f32", s.t.f32)
    } else {
        ("loadInput.i32", s.t.i32)
    };
    let args = [
        s.m.i32(id as u32),
        s.m.i32(0),
        s.m.i8(col),
        s.m.i32(vertex),
    ];
    let entry = s.entry;
    s.op(entry, name, Attr::ReadNone, ty, 4, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str, index: u32, kind: u8, comp: u8, row: u32, cols: u8) -> SigElement {
        SigElement {
            name: name.into(),
            index,
            kind,
            comp,
            interp: 0,
            row: Some(row),
            col: 0,
            cols,
            usage: 0,
            half: false,
        }
    }

    /// A position, a float interpolant and an integer one.
    fn outputs() -> Vec<SigElement> {
        vec![
            element("SV_Position", 0, 3, 9, 0, 4),
            element("TEXCOORD", 0, 0, 9, 1, 4),
            element("TEXCOORD", 1, 0, 5, 2, 2),
        ]
    }

    /// The stage is a geometry shader of triangles in and six strip
    /// vertices out, with the vertex shader's outputs on both sides and
    /// the primitive id the pixel shader declares; the validator, where
    /// there is one, accepts it.
    #[test]
    fn the_rectangle_stage_is_a_geometry_shader() {
        let wanted = [element("SV_PrimitiveID", 0, 10, 5, 3, 1)];
        let vs = crate::dxbc::build(&[(FourCC::OSG1, &super::super::container::signature(&outputs(), false))]);
        let ps = crate::dxbc::build(&[(FourCC::ISG1, &super::super::container::signature(&wanted, true))]);
        let gs = rect_list_gs(&vs, Some(&ps)).unwrap();
        assert_eq!(rectangle_gs(&vs).unwrap(), rect_list_gs(&vs, None).unwrap());
        let c = Container::parse(&gs).unwrap();
        // Program version: geometry (2), shader model 6.6.
        assert_eq!(c.part(FourCC::DXIL).unwrap().data[..4], [0x66, 0, 2, 0]);
        let psv = c.part(FourCC::PSV0).unwrap().data;
        // Triangles in, triangle strips out, stream 0, a position.
        assert_eq!(psv[4..17], [3, 0, 0, 0, 5, 0, 0, 0, 1, 0, 0, 0, 1]);
        // Geometry stage, six vertices, three inputs, four outputs.
        assert_eq!(psv[28..34], [2, 0, 6, 0, 3, 4]);
        let sig = |fourcc| super::super::signature(c.part(fourcc).unwrap().data).unwrap();
        let (ins, outs) = (sig(FourCC::ISG1), sig(FourCC::OSG1));
        assert_eq!(ins.len(), 3);
        assert_eq!(outs.len(), 4);
        for (i, o) in ins.iter().zip(&outs) {
            assert_eq!((&i.name, i.index, i.row), (&o.name, o.index, o.row));
            assert_eq!((i.col, i.cols, i.comp), (o.col, o.cols, o.comp));
        }
        assert_eq!((outs[3].kind, outs[3].row), (10, Some(3)));
        #[cfg(windows)]
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut gs.clone()).unwrap();
            let plain = build(outputs(), &[]).unwrap();
            v.validate(&mut plain.clone()).unwrap();
        }
    }

    /// Refused with a reason: no position, a primitive id in a register a
    /// vertex output has, more components than six vertices may carry.
    #[test]
    fn rectangle_stages_that_cannot_be_made_are_refused() {
        assert!(build(outputs()[1..].to_vec(), &[]).is_err());
        let taken = [element("SV_PrimitiveID", 0, 10, 5, 1, 1)];
        assert!(build(outputs(), &taken).is_err());
        let mut many = vec![element("SV_Position", 0, 3, 9, 0, 4)];
        many.extend((1..=43).map(|row| element("TEXCOORD", row - 1, 0, 9, row, 4)));
        assert!(build(many, &[]).is_err());
    }

    /// A vertex shader that passes a position and a float4 through, and a
    /// pixel shader that writes the float4.
    #[cfg(windows)]
    fn pass_through() -> (super::super::HostShader, super::super::HostShader) {
        let position = SigElement {
            interp: 4,
            usage: 15,
            ..element("SV_Position", 0, 3, 9, 0, 4)
        };
        let coord = SigElement {
            interp: 2,
            usage: 15,
            ..element("TEXCOORD", 0, 0, 9, 1, 4)
        };
        let mut vs = Shader::new(false);
        vs.inputs = vec![
            SigElement {
                usage: 15,
                ..element("POSITION", 0, 0, 9, 0, 4)
            },
            SigElement {
                interp: 0,
                ..coord.clone()
            },
        ];
        vs.outputs = vec![position.clone(), coord.clone()];
        let mut ps = Shader::new(true);
        ps.inputs = vec![
            SigElement {
                usage: 0,
                ..position
            },
            coord,
        ];
        ps.outputs = vec![SigElement {
            usage: 15,
            ..element("SV_Target", 0, 16, 9, 0, 4)
        }];
        // Vertex: output id from input id. Pixel: target 0 from input 1.
        for (s, copies) in [(&mut vs, &[(0, 0), (1, 1)][..]), (&mut ps, &[(1, 0)][..])] {
            let b = s.entry;
            for &(from, to) in copies {
                for col in 0..4 {
                    let args = [s.m.i32(from), s.m.i32(0), s.m.i8(col), s.m.undef(s.t.i32)];
                    let v = s.op(b, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args);
                    let args = [s.m.i32(to), s.m.i32(0), s.m.i8(col), v];
                    s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
                }
            }
            s.m.terminate(b, Term::Ret);
        }
        let host = |stage, s: Shader| super::super::HostShader {
            stage,
            dxil: s.finish(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            root_indexed_views: Vec::new(),
            half_views: Vec::new(),
            memory: false,
            memory_writes: false,
            view_addresses: false,
            heap: false,
            heap_writes: false,
            view_marks: false,
        };
        (
            host(crate::native::HwStage::Vs, vs),
            host(crate::native::HwStage::Ps, ps),
        )
    }

    /// Three corners of the whole target, the corner between the other two
    /// first (as the tiled lighting sends its tiles), second (as
    /// full-screen rectangles come) or third: with the stage every texel
    /// is drawn and the interpolant continues into the fourth corner's
    /// half; without it part of the target is not drawn.
    #[test]
    #[cfg(windows)]
    fn rectangles_are_drawn_whole() {
        use super::super::host::{Draw, Host};
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 device; skipped");
            return;
        };
        let (vs, ps) = pass_through();
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut vs.dxil.clone()).unwrap();
            v.validate(&mut ps.dxil.clone()).unwrap();
        }
        let gs = rect_list_gs(&vs.dxil, Some(&ps.dxil[..])).unwrap();
        let root = host.root_signature(&vs, &ps).unwrap();
        // Position (z 0.5, w 1) and an interpolant that is the position.
        let vertex = |(x, y): (f32, f32)| [x, y, 0.5, 1.0, x, y, 0.0, 1.0];
        for (name, corners) in [
            ("corner first", [(-1.0, 1.0), (-1.0, -1.0), (1.0, 1.0)]),
            ("corner second", [(-1.0, 1.0), (-1.0, -1.0), (1.0, -1.0)]),
            ("corner third", [(-1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]),
        ] {
            let vertices = corners.map(vertex).concat();
            let draw = |gs: Option<&[u8]>| {
                let pso = host
                    .pipeline_gs(&root, &vs.dxil, gs.unwrap_or(&[]), &ps.dxil)
                    .unwrap();
                let d = Draw {
                    roots: Vec::new(),
                    textures: &[],
                    samplers: &[],
                    vertices: &vertices,
                    stride: 32,
                    size: (8, 8),
                    mesh_groups: 0,
                };
                host.draw(&root, &pso, &d).unwrap()
            };
            let whole = draw(Some(&gs[..]));
            for (i, texel) in whole.iter().enumerate() {
                let (x, y) = ((i % 8) as f32, (i / 8) as f32);
                let want = [(x + 0.5) / 4.0 - 1.0, 1.0 - (y + 0.5) / 4.0, 0.0, 1.0];
                assert!(
                    texel.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-4),
                    "{name}: texel {i} is {texel:?}, not {want:?}"
                );
            }
            let half = draw(None);
            let missing = half.iter().zip(&whole).filter(|(a, b)| a != b).count();
            assert!(missing >= 24, "{name}: only {missing} texels need the stage");
        }
    }
}


/// Which of a rectangle's three corners (x and y of each) holds the right
/// angle: the one opposite the longest side, else (ties, NaN) `1`. The
/// geometry shader of [`rectangle_gs`] makes the same choice on the GPU.
pub fn right_angle(xy: [[f32; 2]; 3]) -> usize {
    let side = |i: usize, j: usize| {
        let (dx, dy) = (xy[i][0] - xy[j][0], xy[i][1] - xy[j][1]);
        dx * dx + dy * dy
    };
    let (d12, d20, d01) = (side(1, 2), side(2, 0), side(0, 1));
    if d12 > d20 && d12 > d01 {
        0
    } else if d01 > d12 && d01 > d20 {
        2
    } else {
        1
    }
}

/// The geometry shader that completes the rectangles of the vertex shader
/// container `vs` (its output signature is the shader's input and output).
pub fn rectangle_gs(vs: &[u8]) -> Result<Vec<u8>, ShaderError> {
    rect_list_gs(vs, None)
}

#[cfg(test)]
mod rule_tests {
    use super::right_angle;

    /// The right angle is found wherever it is; triangles that are no
    /// rectangle's take the corner opposite their longest side (their
    /// widest angle), and ties and NaN keep the blits' corner, `v1`.
    #[test]
    fn right_angles_are_found_in_any_order() {
        let (tl, bl, br, tr) = ([0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]);
        assert_eq!(right_angle([tl, bl, br]), 1);
        assert_eq!(right_angle([tl, bl, tr]), 0);
        assert_eq!(right_angle([bl, tr, tl]), 2);
        assert_eq!(right_angle([[0.0, 0.0], [3.0, 1.0], [1.0, 0.0]]), 2, "obtuse at v2");
        assert_eq!(right_angle([tl, tl, tl]), 1, "a point");
        assert_eq!(right_angle([tl, [f32::NAN, 0.0], br]), 1);
    }
}

#[cfg(all(test, windows))]
mod upstream_tests {
    use super::super::bitcode::{Attr, Term};
    use super::super::container::SigElement;
    use super::super::host::{Draw, Host};
    use super::super::shader::Shader;

    fn element(name: &str, kind: u8, interp: u8, row: u32, cols: u8) -> SigElement {
        SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp: 9,
            interp,
            row: Some(row),
            col: 0,
            cols,
            usage: (1 << cols) - 1,
            half: false,
        }
    }

    /// A shader copying its inputs to its outputs (vertex), or writing
    /// target 0 from TEXCOORD (pixel).
    fn shader(pixel: bool) -> Vec<u8> {
        let mut s = Shader::new(pixel);
        let b = s.entry;
        let (inputs, outputs) = if pixel {
            (
                vec![element("SV_Position", 3, 4, 0, 4), element("TEXCOORD", 0, 2, 1, 2)],
                vec![element("SV_Target", 16, 0, 0, 4)],
            )
        } else {
            (
                vec![element("POSITION", 0, 0, 0, 4), element("TEXCOORD", 0, 0, 1, 2)],
                vec![element("SV_Position", 3, 0, 0, 4), element("TEXCOORD", 0, 0, 1, 2)],
            )
        };
        let load = |s: &mut Shader, id: u32, col: u8| {
            let args = [s.m.i32(id), s.m.i32(0), s.m.i8(col), s.m.undef(s.t.i32)];
            s.op(b, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args)
        };
        let values: Vec<_> = if pixel {
            let one = s.m.f32(1f32.to_bits());
            vec![load(&mut s, 1, 0), load(&mut s, 1, 1), one, one]
        } else {
            [(0, 0), (0, 1), (0, 2), (0, 3), (1, 0), (1, 1)]
                .into_iter()
                .map(|(id, c)| load(&mut s, id, c))
                .collect()
        };
        let mut k = 0;
        for (id, e) in outputs.iter().enumerate() {
            for col in 0..e.cols {
                let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(col), values[k]];
                s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
                k += 1;
            }
        }
        s.inputs = inputs;
        s.outputs = outputs;
        s.m.terminate(b, Term::Ret);
        s.finish()
    }

    /// One rectangle with a coordinate attribute, its right angle at each of
    /// the three corners in turn (the blits' top left, bottom left, bottom
    /// right; the tiled lighting's top left, bottom left, top right; and
    /// one with it last): every pixel of the square is drawn, the attribute
    /// interpolated over it, the missing half included. Three corners on one
    /// line draw nothing.
    #[test]
    fn rectangles_get_their_fourth_corner() {
        let (vs, ps) = (shader(false), shader(true));
        let gs = super::rect_list_gs(&vs, Some(&ps)).unwrap();
        if let Ok(mut v) =
            super::super::validator::Validator::load(super::super::validator::Validator::default_path().as_deref())
        {
            for c in [&vs, &gs, &ps] {
                v.validate(&mut c.clone()).unwrap();
            }
        }
        let corner = |x: f32, y: f32| [x, y, 0.5, 1.0, x, y];
        let (tl, bl, br, tr) = (corner(-0.5, 0.5), corner(-0.5, -0.5), corner(0.5, -0.5), corner(0.5, 0.5));
        let orders = [
            [tl, bl, br], [tl, bl, tr], [bl, tr, tl], [tl, corner(0.0, 0.0), br],
            [corner(0.0, 0.5), corner(-0.5, 0.0), corner(0.0, -0.5)],
        ];
        for warp in [false, true] {
            let Ok(host) = Host::new(warp) else { continue };
            let root = host.root_signature_for(&[]).unwrap();
            let pso = host.pipeline_gs(&root, &vs, &gs, &ps).unwrap();
            for (n, order) in orders.iter().enumerate() {
                let vertices = order.concat();
                let draw = Draw {
                    roots: Vec::new(),
                    textures: &[],
                    samplers: &[],
                    vertices: &vertices,
                    stride: 24,
                    size: (32, 32),
                    mesh_groups: 0,
                };
                let texels = host.draw(&root, &pso, &draw).unwrap();
                for (i, t) in texels.iter().enumerate() {
                    let (x, y) = ((i % 32) as f32, (i / 32) as f32);
                    let (u, v) = ((x + 0.5) / 16.0 - 1.0, 1.0 - (y + 0.5) / 16.0);
                    // Leave exact diamond edges to the rasterizer's fill rule.
                    if n == 4 && (u.abs() + v.abs() - 0.5).abs() < 1e-6 {
                        continue;
                    }
                    let inside = if n == 4 {
                        u.abs() + v.abs() < 0.5
                    } else {
                        n < 3 && u.abs() < 0.5 && v.abs() < 0.5
                    };
                    let want = if inside { [u, v, 1.0, 1.0] } else { [0.0; 4] };
                    let near = t.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-4);
                    assert!(near, "warp {warp} order {n} pixel ({x}, {y}): {t:?}, want {want:?}");
                }
            }
        }
    }
}
