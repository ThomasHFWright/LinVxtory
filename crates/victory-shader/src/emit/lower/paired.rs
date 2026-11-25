//! A wave64 compute wave in one host wave32: each thread holds lanes n and n+32.
use super::*;

thread_local! {
    pub(super) static PAIRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The caller must widen lane branches and require full console waves.
pub(crate) fn with_paired_lanes<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            PAIRED.set(self.0);
        }
    }
    let _restore = Restore(PAIRED.replace(true));
    f()
}

#[derive(Default)]
pub(super) struct LaneState {
    pub(super) raw: guarded::Raws,
    pub(super) read_phis: HashSet<Value>,
    pub(super) join_handles: HashMap<(String, bool, (u32, u32)), V>,
    pub(super) vals: HashMap<Value, Val>,
    pub(super) low16: HashMap<Value, V>,
    pub(super) floats: HashMap<Value, V>,
    pub(super) bools: HashMap<(usize, Bool), V>,
    pub(super) lane_bits: HashMap<Value, V>,
    pub(super) lane_fail: HashMap<Value, Why>,
    pub(super) mask_phis: HashMap<Value, V>,
    pub(super) pending_masks: Vec<(V, usize, usize)>,
    pub(super) data_phis: Vec<(V, usize, usize)>,
    pub(super) handles: HashMap<(usize, Site, u8, bool, (u32, u32)), V>,
    pub(super) strides: HashMap<(usize, Site, u8), V>,
    pub(super) clane: Option<V>,
}

pub(super) fn lane_scalars(p: &Program, res: &Resources) -> HashSet<Value> {
    let mut own: HashSet<Value> = res.waterfall.reads.iter().copied().collect();
    if own.is_empty() {
        return own;
    }
    let order = crate::ir::reverse_postorder(&p.blocks);
    loop {
        let before = own.len();
        for &b in &order {
            let block = &p.blocks[b];
            for phi in &block.phis {
                if !matches!(phi.reg, Reg::V(_)) && phi.args.iter().any(|a| own.contains(a)) {
                    own.insert(phi.value);
                }
            }
            for inst in &block.insts {
                if matches!(inst.fields, Fields::Sop { .. } | Fields::Smem { .. })
                    && inst.reads.iter().any(|r| own.contains(&r.1))
                {
                    own.extend(inst.writes.iter().map(|w| w.1));
                }
            }
        }
        if own.len() == before {
            return own;
        }
    }
}

impl<'a> Lower<'a> {
    pub(super) fn swap_lanes(&mut self) {
        let Some(t) = self.twin.as_deref_mut() else {
            return;
        };
        std::mem::swap(&mut self.vals, &mut t.vals);
        std::mem::swap(&mut self.floats, &mut t.floats);
        std::mem::swap(&mut self.bools, &mut t.bools);
        std::mem::swap(&mut self.lane_bits, &mut t.lane_bits);
        std::mem::swap(&mut self.lane_fail, &mut t.lane_fail);
        std::mem::swap(&mut self.mask_phis, &mut t.mask_phis);
        std::mem::swap(&mut self.pending_masks, &mut t.pending_masks);
        std::mem::swap(&mut self.data_phis, &mut t.data_phis);
        std::mem::swap(&mut self.handles, &mut t.handles);
        std::mem::swap(&mut self.strides, &mut t.strides);
        std::mem::swap(&mut self.clane, &mut t.clane);
        std::mem::swap(&mut self.low16, &mut t.low16);
        std::mem::swap(&mut self.raw, &mut t.raw);
        std::mem::swap(&mut self.read_phis, &mut t.read_phis);
        std::mem::swap(&mut self.join_handles, &mut t.join_handles);
        self.upper = !self.upper;
    }

    pub(super) fn both<T>(
        &mut self,
        mut f: impl FnMut(&mut Self) -> Result<T, Why>,
    ) -> Result<[T; 2], Why> {
        let was = self.upper;
        if was {
            self.swap_lanes();
        }
        let lo = f(self);
        self.swap_lanes();
        let hi = f(self);
        if !was {
            self.swap_lanes();
        }
        Ok([lo?, hi?])
    }

    pub(super) fn put_lanes(&mut self, v: Value, lo: Val, hi: Val) {
        let (mine, other) = if self.upper { (hi, lo) } else { (lo, hi) };
        self.vals.insert(v, mine);
        if let Some(t) = self.twin.as_deref_mut() {
            t.vals.insert(v, other);
        }
    }

    pub(super) fn block_phis(&mut self, b: usize) {
        let p = self.x.p;
        for (i, phi) in p.blocks[b].phis.iter().enumerate() {
            let val = match phi.reg {
                Reg::S(c) if c == EXEC || c == EXEC + 1 => Val::Missing("a lane mask"),
                _ if self.bad.contains_key(&phi.value) => Val::Missing(self.bad[&phi.value]),
                Reg::Scc => {
                    let v = self.s.m.phi(self.cur, self.s.t.i1);
                    self.data_phis.push((v, b, i));
                    Val::Bit(v)
                }
                _ => {
                    let v = self.s.m.phi(self.cur, self.s.t.i32);
                    self.data_phis.push((v, b, i));
                    Val::I32(v)
                }
            };
            self.vals.insert(phi.value, val);
        }
    }

    pub(super) fn paired_section_inst(&mut self, b: usize, i: usize, start: usize) {
        use super::super::wave::Shape;
        const UNKNOWN: Why = "a whole-wave section step the simulation does not follow";
        let inst = &self.x.p.blocks[b].insts[i];
        for &(_, w) in &inst.writes {
            let shape = match self.section_out.contains(&w) {
                true => self.sections[&b][&start].shapes.get(&w).copied(),
                false => None,
            };
            let made = match shape {
                None => Err(UNKNOWN),
                Some(
                    sh @ (Shape::Active(..)
                    | Shape::Prefix(..)
                    | Shape::ActiveF64(..)
                    | Shape::Half(..)),
                ) => self.paired_shape(sh),
                Some(sh) => self.both(|l| l.shape(sh, false)),
            };
            match made {
                Ok([lo, hi]) => self.put_lanes(w, Val::I32(lo), Val::I32(hi)),
                Err(why) => self.put_lanes(w, Val::Missing(why), Val::Missing(why)),
            }
        }
    }

    pub(super) fn paired_shape(&mut self, shape: super::super::wave::Shape) -> Result<[V; 2], Why> {
        use super::super::wave::{Op, Shape};
        if let Shape::ActiveF64(lo, hi, mask, high) = shape {
            return self.paired_sum64(high, lo, hi, mask);
        }
        if let Shape::Half(op, src, mask, ident, hi) = shape {
            let [sum, _] = self.paired_shape(Shape::Active(op, src, mask, ident))?;
            let &m = self
                .x
                .st
                .masks
                .get(&mask)
                .ok_or("a wave reduction's untracked mask")?;
            let c = self.cur;
            let [lo, upper] = self.both(|l| {
                let on = l.boolean(m, c)?;
                let x = l.i32_of(src)?;
                let id = l.s.m.i32(ident);
                Ok(l.s.m.select(c, on, x, id))
            })?;
            let lane = self.s.m.i32(hi % 32);
            let top = self.read_lane(if hi < 32 { lo } else { upper }, lane);
            let (low_mask, high_mask) = (self.s.m.i32(0xFFFF), self.s.m.i32(0xFFFF_0000));
            let low = self.s.m.bin(c, Bin::And, sum, low_mask);
            let high = self.s.m.bin(c, Bin::And, top, high_mask);
            let r = self.s.m.bin(c, Bin::Or, low, high);
            return Ok([r, r]);
        }
        let (op, src, mask, ident, prefix) = match shape {
            Shape::Active(op, src, mask, ident) => (op, src, mask, ident, None),
            Shape::Prefix(op, src, mask, ident, inclusive) => {
                (op, src, mask, ident, Some(inclusive))
            }
            _ => return Err("a whole-wave section value of another shape"),
        };
        let &m = self
            .x
            .st
            .masks
            .get(&mask)
            .ok_or("a wave reduction's untracked mask")?;
        let c = self.cur;
        let [lo, hi] = self.both(|l| {
            let on = l.boolean(m, c)?;
            let x = l.i32_of(src)?;
            let id = l.s.m.i32(ident);
            Ok(l.s.m.select(c, on, x, id))
        })?;
        self.s.flags |= flags::WAVE_OPS;
        if op == Op::AddH {
            if prefix.is_some() {
                return Err("a wave prefix of an operation other than a sum");
            }
            let (lo, hi) = (self.half_tree(lo, 5), self.half_tree(hi, 5));
            let r = self.join_half_sums(lo, hi);
            return Ok([r, r]);
        }
        let float = matches!(op, Op::AddF | Op::MinF | Op::MaxF);
        let (lo, hi) = if float {
            (self.as_float(lo), self.as_float(hi))
        } else {
            (lo, hi)
        };
        let r = match prefix {
            Some(inclusive) => {
                let mine = self.host_wave_op(op, lo, float, Some(inclusive))?;
                let theirs = self.host_wave_op(op, hi, float, Some(inclusive))?;
                let below = self.host_wave_op(op, lo, float, None)?;
                [mine, self.s.m.bin(c, Bin::Add, theirs, below)]
            }
            None => {
                let lo = self.host_wave_op(op, lo, float, None)?;
                let hi = self.host_wave_op(op, hi, float, None)?;
                let r = self.join_halves(op, lo, hi, float);
                [r, r]
            }
        };
        Ok(if float { r.map(|x| self.as_int(x)) } else { r })
    }

    pub(super) fn host_wave_op(
        &mut self,
        op: super::super::wave::Op,
        arg: V,
        float: bool,
        prefix: Option<bool>,
    ) -> Result<V, Why> {
        use super::super::wave::Op;
        let c = self.cur;
        let (i32t, f32t) = (self.s.t.i32, self.s.t.f32);
        let ty = if float { f32t } else { i32t };
        let sfx = if float { "f32" } else { "i32" };
        Ok(match (op, prefix) {
            (Op::AddU | Op::AddF, Some(inclusive)) => {
                let (k0, k1) = (self.s.m.i8(0), self.s.m.i8(1));
                let r = self.s.op(
                    c,
                    &format!("wavePrefixOp.{sfx}"),
                    Attr::NoUnwind,
                    ty,
                    121,
                    &[arg, k0, k1],
                );
                if inclusive {
                    self.s.m.bin(c, Bin::Add, r, arg)
                } else {
                    r
                }
            }
            (_, Some(_)) => return Err("a wave prefix of an operation other than a sum"),
            (Op::Or | Op::And | Op::Xor, None) => {
                let k = self.s.m.i8(match op {
                    Op::And => 0,
                    Op::Or => 1,
                    _ => 2,
                });
                self.s
                    .op(c, "waveActiveBit.i32", Attr::NoUnwind, i32t, 120, &[arg, k])
            }
            (_, None) => {
                let (code, unsigned) = match op {
                    Op::AddU => (0, 1),
                    Op::AddF => (0, 0),
                    Op::MinU => (2, 1),
                    Op::MaxU => (3, 1),
                    Op::MinI | Op::MinF => (2, 0),
                    _ => (3, 0),
                };
                let (k, u) = (self.s.m.i8(code), self.s.m.i8(unsigned));
                self.s.op(
                    c,
                    &format!("waveActiveOp.{sfx}"),
                    Attr::NoUnwind,
                    ty,
                    119,
                    &[arg, k, u],
                )
            }
        })
    }

    pub(super) fn join_halves(
        &mut self,
        op: super::super::wave::Op,
        lo: V,
        hi: V,
        float: bool,
    ) -> V {
        use super::super::wave::Op;
        let c = self.cur;
        let ty = if float { self.s.t.f32 } else { self.s.t.i32 };
        let sfx = if float { "f32" } else { "i32" };
        match op {
            Op::AddU | Op::AddF => self.s.m.bin(c, Bin::Add, lo, hi),
            Op::Or => self.s.m.bin(c, Bin::Or, lo, hi),
            Op::And => self.s.m.bin(c, Bin::And, lo, hi),
            Op::Xor => self.s.m.bin(c, Bin::Xor, lo, hi),
            _ => {
                // FMax 35, FMin 36, IMax 37, IMin 38, UMax 39, UMin 40
                let code = match op {
                    Op::MaxF => 35,
                    Op::MinF => 36,
                    Op::MinU => 40,
                    Op::MaxU => 39,
                    Op::MinI => 38,
                    _ => 37,
                };
                self.s.op(
                    c,
                    &format!("binary.{sfx}"),
                    Attr::ReadNone,
                    ty,
                    code,
                    &[lo, hi],
                )
            }
        }
    }

    pub(super) fn half_tree(&mut self, x: V, steps: u32) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let lane = self
            .s
            .op(c, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let mut sum = x;
        for step in 0..steps {
            let bit = self.s.m.i32(1 << step);
            let partner = self.s.m.bin(c, Bin::Xor, lane, bit);
            let theirs = self.read_lane(sum, partner);
            sum = self.add_halves(sum, theirs);
        }
        let (high, low, zero) = (
            self.s.m.i32(0xFFFF_0000),
            self.s.m.i32(0xFFFF),
            self.s.m.i32(0),
        );
        let first = self.read_lane(x, zero);
        let first = self.s.m.bin(c, Bin::And, first, high);
        let sum = self.s.m.bin(c, Bin::And, sum, low);
        self.s.m.bin(c, Bin::Or, first, sum)
    }

    pub(super) fn join_half_sums(&mut self, lo: V, hi: V) -> V {
        let c = self.cur;
        let total = self.add_halves(lo, hi);
        let (high, low) = (self.s.m.i32(0xFFFF_0000), self.s.m.i32(0xFFFF));
        let total = self.s.m.bin(c, Bin::And, total, low);
        let first = self.s.m.bin(c, Bin::And, lo, high);
        self.s.m.bin(c, Bin::Or, first, total)
    }

    pub(super) fn paired_inst(&mut self, b: usize, i: usize) -> Result<(), Stop> {
        let inst: &'a Inst = &self.x.p.blocks[b].insts[i];
        let name = inst.name();
        let once = matches!(inst.fields, Fields::Sopp { .. })
            || matches!(inst.fields, Fields::Smem { .. })
                && (name.contains("store") || name.contains("atomic"));
        if once {
            self.inst(b, i)?;
            self.mirror(inst);
            return Ok(());
        }
        let cross = match name {
            "v_readlane_b32" | "ds_bpermute_b32" => true,
            // (not a waterfall loop's read, the lane's own dword: `Self::valu`)
            "v_readfirstlane_b32" => !inst
                .writes
                .first()
                .is_some_and(|w| self.x.res.waterfall.reads.contains(&w.1)),
            _ => false,
        };
        if cross {
            if let Err(why) = self.paired_cross(inst, b, i) {
                for &(_, w) in &inst.writes {
                    self.put_lanes(w, Val::Missing(why), Val::Missing(why));
                }
            }
            return Ok(());
        }
        if matches!(inst.fields, Fields::Ds { .. })
            && !name.starts_with("ds_swizzle")
            && !name.contains("permute")
        {
            let write = !name.starts_with("ds_read");
            if write || self.lds_last != Some(false) {
                let (mode, void) = (self.s.m.i32(8), self.s.t.void);
                self.s
                    .op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
            }
            self.lds_last = Some(write);
        }
        self.inst(b, i)?;
        self.swap_lanes();
        let r = self.inst(b, i);
        self.swap_lanes();
        r
    }

    pub(super) fn mirror(&mut self, inst: &Inst) {
        let Some(t) = self.twin.as_deref_mut() else {
            return;
        };
        for &(_, w) in &inst.writes {
            if let Some(&v) = self.vals.get(&w) {
                t.vals.insert(w, v);
            }
        }
    }

    pub(super) fn paired_cross(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        self.cross_lane(false)?;
        let c = self.cur;
        let (k31, k32, z) = (self.s.m.i32(31), self.s.m.i32(32), self.s.m.i32(0));
        // a lane's value from the lower or upper value of the host lane `at` (of 0..63)
        let pick = |l: &mut Self, lo: V, hi: V, at: V| {
            let host = l.s.m.bin(c, Bin::And, at, k31);
            let upper = l.s.m.bin(c, Bin::And, at, k32);
            let upper = l.s.m.cmp(c, pred::NE, upper, z);
            let (lo, hi) = (l.read_lane(lo, host), l.read_lane(hi, host));
            l.s.m.select(c, upper, hi, lo)
        };
        match inst.name() {
            "v_readlane_b32" => {
                // the lane: a scalar or a constant
                let lane = self.src(inst, 1)?;
                let [lo, hi] = self.both(|l| l.src(inst, 0))?;
                let r = pick(self, lo, hi, lane);
                self.write_i(inst, r, None)?;
            }
            "v_readfirstlane_b32" => {
                let [(lo, in_lo), (hi, in_hi)] = self.both(|l| {
                    let x = l.src(inst, 0)?;
                    let guard = l.guard(b, i)?;
                    Ok(l.first_lane(x, guard))
                })?;
                // the first lane in EXEC is a lower one if there is one; with none, lane 0
                let hi = self.s.m.select(c, in_hi, hi, lo);
                let r = self.s.m.select(c, in_lo, lo, hi);
                self.write_i(inst, r, None)?;
            }
            _ => {
                // `ds_bpermute_b32`: each lane reads the lane its address names, which gives
                // its data, or 0 outside EXEC
                let Fields::Ds {
                    dst: Some(Operand::Vector { reg, .. }),
                    addr: Some(addr),
                    data,
                    offset0,
                    offset1,
                    ..
                } = &inst.fields
                else {
                    return Err("a permute's operands");
                };
                let data = data.first().ok_or("a permute source")?;
                let offset = u32::from(*offset0) | u32::from(*offset1) << 8;
                let [(from_lo, x_lo, g_lo), (from_hi, x_hi, g_hi)] = self.both(|l| {
                    let at = l.vgprs(inst, addr, 1)?[0];
                    let x = l.vgprs(inst, data, 1)?[0];
                    let guard = l.guard(b, i)?;
                    let (o, two) = (l.s.m.i32(offset), l.s.m.i32(2));
                    let at = l.s.m.bin(c, Bin::Add, at, o);
                    let from = l.s.m.bin(c, Bin::LShr, at, two);
                    let x = match guard {
                        Some(g) => l.s.m.select(c, g, x, z),
                        None => x,
                    };
                    Ok((from, x, guard))
                })?;
                let read = [
                    pick(self, x_lo, x_hi, from_lo),
                    pick(self, x_lo, x_hi, from_hi),
                ];
                let guards = [g_lo, g_hi];
                let reg = *reg;
                self.both(|l| {
                    let n = usize::from(l.upper);
                    l.write(inst, Reg::V(reg), read[n], guards[n]);
                    Ok(())
                })?;
                return Ok(());
            }
        }
        self.mirror(inst);
        Ok(())
    }

    pub(super) fn paired_sum64(
        &mut self,
        high: bool,
        lo: Value,
        hi: Value,
        mask: Value,
    ) -> Result<[V; 2], Why> {
        let &m = self
            .x
            .st
            .masks
            .get(&mask)
            .ok_or("a wave reduction's untracked mask")?;
        let c = self.cur;
        let [x0, x1] = self.both(|l| {
            let on = l.boolean(m, c)?;
            let (dl, dh) = (l.i32_of(lo)?, l.i32_of(hi)?);
            Ok(l.masked_double(on, dl, dh))
        })?;
        let (s0, s1) = (self.wave_sum_f64(x0), self.wave_sum_f64(x1));
        let total = self.s.m.bin(self.cur, Bin::Add, s0, s1);
        let (dl, dh) = self.split_double(total);
        let x = if high { dh } else { dl };
        Ok([x, x])
    }

    fn masked_double(&mut self, on: V, lo: V, hi: V) -> V {
        let x = self.make_double(lo, hi);
        let z = self.s.m.i32(0);
        let zero = self.make_double(z, z);
        self.s.m.select(self.cur, on, x, zero)
    }

    fn wave_sum_f64(&mut self, x: V) -> V {
        self.s.flags |= flags::WAVE_OPS;
        let c = self.cur;
        let f64t = self.s.m.double();
        let (sum, signed) = (self.s.m.i8(0), self.s.m.i8(0));
        self.s.op(
            c,
            "waveActiveOp.f64",
            Attr::NoUnwind,
            f64t,
            119,
            &[x, sum, signed],
        )
    }

    fn make_double(&mut self, lo: V, hi: V) -> V {
        let c = self.cur;
        let f64t = self.s.m.double();
        self.s.flags |= flags::DOUBLES;
        self.s
            .op(c, "makeDouble.f64", Attr::ReadNone, f64t, 101, &[lo, hi])
    }

    fn split_double(&mut self, d: V) -> (V, V) {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let pair = self
            .s
            .m
            .named_struct("dx.types.splitdouble", vec![i32t, i32t]);
        let r = self
            .s
            .op(c, "splitDouble.f64", Attr::ReadNone, pair, 102, &[d]);
        (self.s.m.extract(c, r, 0), self.s.m.extract(c, r, 1))
    }
}
