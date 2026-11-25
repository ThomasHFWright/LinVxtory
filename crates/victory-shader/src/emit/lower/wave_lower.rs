//! Linux paired-wave lowering retained from the tested integration.
use super::*;
thread_local! {
    static NEEDS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    pub(super) static LOCAL_WAVES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static LOCAL_ANY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn branches_around(st: &Structured, needs: &[usize]) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for (d, region) in lane_regions(st)? {
        if needs
            .iter()
            .any(|&b| region.get(b).copied().unwrap_or(false))
        {
            st.wave_any.get(d).copied().flatten()?;
            out.push(d);
        }
    }
    Some(out)
}

pub(crate) fn lane_uniform(p: &Program, st: &Structured, all: &[bool]) -> HashSet<Value> {
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    let mut same: HashSet<Value> = HashSet::new();
    for b in crate::ir::reverse_postorder(&p.blocks) {
        let block = &p.blocks[b];
        let whole = block
            .preds
            .iter()
            .all(|&q| all.get(q).copied().unwrap_or(false));
        for phi in &block.phis {
            if matches!(phi.reg, Reg::V(_)) && whole && phi.args.iter().all(|a| same.contains(a)) {
                same.insert(phi.value);
            }
        }
        for (i, inst) in block.insts.iter().enumerate() {
            let name = inst.name();
            let ops: Vec<Operand> = match &inst.fields {
                Fields::Valu(v)
                    if v.dpp.is_none()
                        && v.sdwa.is_none()
                        && !name.starts_with("v_mbcnt")
                        && !name.starts_with("v_cmpx")
                        && !name.contains("lane") =>
                {
                    v.src.clone()
                }
                _ => continue,
            };
            if st.guards[b][i].is_some() {
                continue; // lanes outside EXEC keep another value
            }
            let alike = ops.iter().all(|op| match *op {
                Operand::Vector { reg, .. } => {
                    // every dword of the operand the instruction reads from that register on
                    let read: Vec<Value> = inst
                        .reads
                        .iter()
                        .filter(|r| matches!(r.0, Reg::V(x) if x >= reg))
                        .map(|r| r.1)
                        .collect();
                    !read.is_empty() && read.iter().all(|x| same.contains(x))
                }
                // a scalar is one value; a lane mask is not, unless a compare of such values made it
                Operand::Scalar { code, .. } if code != NULL => inst
                    .read(Reg::S(code))
                    .is_none_or(|x| !is_mask(x) || same.contains(&x)),
                _ => true,
            });
            if alike {
                same.extend(inst.writes.iter().filter(|w| w.0 != Reg::Scc).map(|w| w.1));
            }
        }
    }
    same
}

pub(crate) fn lds_meetings(
    p: &crate::ir::Program,
    st: &Structured,
    all: &[bool],
) -> Option<Vec<(bool, bool)>> {
    let n = st.exits.len();
    let succ = |b: usize| -> Vec<usize> {
        match st.exits[b] {
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch {
                taken, not_taken, ..
            } => vec![taken, not_taken],
            LaneExit::End | LaneExit::Indirect => Vec::new(),
        }
    };
    // regions: blocks not every lane runs, joined by the edges between them
    let mut region: Vec<usize> = (0..n).collect();
    fn find(region: &mut [usize], mut b: usize) -> usize {
        while region[b] != b {
            region[b] = region[region[b]];
            b = region[b];
        }
        b
    }
    for b in 0..n {
        for t in succ(b) {
            if !all[b] && !all[t] {
                let (x, y) = (find(&mut region, b), find(&mut region, t));
                region[x] = y;
            }
        }
    }
    // per region: LDS reads, LDS writes
    let mut reads = vec![false; n];
    let mut writes = vec![false; n];
    for (b, block) in p.blocks.iter().enumerate().take(n) {
        if all[b] {
            continue;
        }
        let r = find(&mut region, b);
        for i in &block.insts {
            let name = i.name();
            if !matches!(i.fields, Fields::Ds { .. })
                || name.starts_with("ds_swizzle")
                || name.contains("permute")
            {
                continue;
            }
            if name.starts_with("ds_read") {
                reads[r] = true;
            } else {
                writes[r] = true;
            }
        }
    }
    if (0..n).any(|r| reads[r] && writes[r]) {
        return None;
    }
    let mut meet = vec![(false, false); n];
    for b in 0..n {
        for t in succ(b) {
            if all[b] && !all[t] {
                let r = find(&mut region, t);
                meet[b].1 |= reads[r] || writes[r];
            } else if !all[b] && all[t] {
                let r = find(&mut region, b);
                meet[t].0 |= reads[r] || writes[r];
            }
        }
    }
    Some(meet)
}

pub(crate) fn need(block: usize) {
    NEEDS.with_borrow_mut(|n| {
        if !n.contains(&block) {
            n.push(block);
        }
    });
}

pub(crate) fn needs_every_lane(what: &str) -> bool {
    what.ends_with("where not every lane runs") || what == MASK_ELSEWHERE
}

pub(crate) fn own_bits_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WAVE_OWN_BITS").is_none_or(|v| v != "0"))
}

pub(crate) fn take_needs() -> Vec<usize> {
    NEEDS.take()
}

/// What a register value holds, for [`wave_local`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wave {
    /// Nothing that depends on how the threads are divided into waves.
    Plain,
    /// The number of the wave's first lane in a mask (`s_ff1`).
    First,
    /// A reduction over the wave's lanes: a count, a sum, a minimum...
    Total(super::super::wave::Op),
    /// A count or sum over the lanes below this one.
    Prefix,
    /// Derived from what a first-lane atomic returned: the slots the wave reserved.
    Base,
    /// A mix the idiom does not make.
    Mixed,
}

pub(crate) fn wave_local(p: &Program, st: &Structured) -> Result<(), (usize, &'static str)> {
    use super::super::wave::{Op, Shape};
    const FIRST: Why = "a wave's first-lane number used as data";
    const TOTAL: Why = "a count or sum over the wave used as data";
    const PREFIX: Why = "a count or sum over the lanes below used without a reserved base";
    const MIXED: Why = "a value that depends on the wave on some paths only";
    const EFFECT: Why = "a write by the wave's first lane other than its reservation";
    fn of(kind: &HashMap<Value, Wave>, v: Option<Value>) -> Wave {
        v.and_then(|v| kind.get(&v).copied()).unwrap_or(Wave::Plain)
    }
    fn put(kind: &mut HashMap<Value, Wave>, v: Value, k: Wave) {
        if k == Wave::Plain {
            kind.remove(&v);
        } else {
            kind.insert(v, k);
        }
    }
    let wild = |k: Wave| !matches!(k, Wave::Plain | Wave::Base);
    let why = |k: Wave| match k {
        Wave::First => FIRST,
        Wave::Total(_) => TOTAL,
        Wave::Prefix => PREFIX,
        _ => MIXED,
    };
    // a mask (by its low dword) under which only a wave's first lane runs
    let first_mask = |firsts: &HashSet<Value>, m: Option<Value>| {
        m.and_then(|m| st.masks.get(&m)).is_some_and(|&b| {
            st.bools
                .support(b)
                .iter()
                .any(|a| matches!(a, Atom::Lane(v) if firsts.contains(v)))
        })
    };
    // One instruction outside a section: the kind of what it writes, and whether it is a
    // first lane's reservation.
    let step = |inst: &Inst,
                kind: &HashMap<Value, Wave>,
                firsts: &mut HashSet<Value>|
     -> Result<(Wave, bool), Why> {
        let name = inst.name();
        let val = |op: &Operand| match *op {
            Operand::Vector { reg, .. } => inst.read(Reg::V(reg)),
            Operand::Scalar { code, .. } if code != NULL => inst.read(Reg::S(code)),
            _ => None,
        };
        let ops: Vec<Operand> = match &inst.fields {
            Fields::Valu(v) => v.src.clone(),
            Fields::Sop { dst, src, .. } => {
                let mut o = src.clone();
                if matches!(name, "s_addk_i32" | "s_mulk_i32") {
                    o.extend(dst.iter().copied());
                }
                o
            }
            Fields::Buffer { data, .. }
                if name.contains("_atomic_") || name.contains("_store_") =>
            {
                data.iter().copied().collect()
            }
            _ => Vec::new(),
        };
        let kinds: Vec<Wave> = ops.iter().map(|o| of(kind, val(o))).collect();
        // Everything else it reads (addresses, descriptors, SCC, the high dwords of wide
        // operands) must not depend on the wave. Not EXEC, and not the old value of a VGPR it
        // writes, which only the lanes outside EXEC keep.
        let named: Vec<Value> = ops.iter().filter_map(&val).collect();
        for &(r, v) in &inst.reads {
            let kept = matches!(r, Reg::V(_)) && inst.writes.iter().any(|w| w.0 == r);
            if matches!(r, Reg::S(c) if c == EXEC || c == EXEC + 1) || kept || named.contains(&v) {
                continue;
            }
            let k = of(kind, Some(v));
            if wild(k) {
                return Err(why(k));
            }
        }
        let w = kinds
            .iter()
            .copied()
            .find(|&k| wild(k))
            .unwrap_or(Wave::Mixed);
        let sum = |k: Wave| k == Wave::Total(Op::AddU);
        let is_mask = |o: &Operand| {
            val(o).is_some_and(|v| st.masks.contains_key(&v) || st.highs.contains_key(&v))
        };
        let mut reserved = false;
        let out = match (name, &kinds[..]) {
            ("s_ff1_i32_b64" | "s_ff1_i32_b32", _) if ops.first().is_some_and(is_mask) => {
                Wave::First
            }
            ("s_bcnt1_i32_b64" | "s_bcnt1_i32_b32", _) if ops.first().is_some_and(is_mask) => {
                Wave::Total(Op::AddU)
            }
            // bits of a mask below the lane; with -1 for the mask, the lane's number
            ("v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32", [_, b]) => {
                match (matches!(ops[0], Operand::Scalar { .. }), *b) {
                    (_, Wave::Prefix) | (true, Wave::Plain | Wave::Base) => Wave::Prefix,
                    (false, Wave::Plain | Wave::Base) => Wave::Plain,
                    (_, k) => return Err(why(k)),
                }
            }
            // nothing wave-dependent goes in; what derives from a reserved base stays marked
            _ if !kinds.iter().any(|&k| wild(k)) => {
                let derives = matches!(inst.fields, Fields::Valu(_) | Fields::Sop { .. });
                if derives && kinds.contains(&Wave::Base) {
                    Wave::Base
                } else {
                    Wave::Plain
                }
            }
            ("v_mov_b32" | "s_mov_b32", [k]) => *k,
            ("v_cmp_eq_i32" | "v_cmp_eq_u32" | "v_cmpx_eq_i32" | "v_cmpx_eq_u32", [a, b]) => {
                // the lane whose own number is the first lane's
                let lane = match (*a, *b) {
                    (Wave::First, k) if !wild(k) => &ops[1],
                    (k, Wave::First) if !wild(k) => &ops[0],
                    _ => return Err(why(w)),
                };
                if !matches!(lane, Operand::Vector { .. }) {
                    return Err(FIRST);
                }
                let mask = match &inst.fields {
                    _ if name.starts_with("v_cmpx") => inst.write(Reg::S(EXEC)),
                    Fields::Valu(v) => match v.dst {
                        Some(Operand::Scalar { code, .. }) => inst.write(Reg::S(code)),
                        _ => None,
                    },
                    _ => None,
                };
                firsts.extend(mask);
                Wave::Plain
            }
            ("v_add_nc_u32" | "s_add_i32", [a, b]) => match (*a, *b) {
                (Wave::Prefix, Wave::Base) | (Wave::Base, Wave::Prefix) => Wave::Base,
                (Wave::Prefix, Wave::Plain | Wave::Prefix) | (Wave::Plain, Wave::Prefix) => {
                    Wave::Prefix
                }
                (x, y) if sum(x) && sum(y) => x,
                _ => return Err(why(w)),
            },
            ("v_sub_nc_u32" | "s_sub_i32", [a, b]) => match (*a, *b) {
                (Wave::Base, Wave::Prefix) => Wave::Base,
                (Wave::Prefix, Wave::Plain) => Wave::Prefix,
                (Wave::Plain, y) if sum(y) && matches!(ops[0], Operand::Const(128)) => y,
                _ => return Err(why(w)),
            },
            ("s_mul_i32" | "v_mul_lo_u32" | "v_mul_u32_u24", [a, b]) => match (*a, *b) {
                (x, Wave::Plain) | (Wave::Plain, x) if sum(x) || x == Wave::Prefix => x,
                _ => return Err(why(w)),
            },
            ("s_lshl_b32", [x, Wave::Plain]) | ("v_lshlrev_b32", [Wave::Plain, x])
                if sum(*x) || *x == Wave::Prefix =>
            {
                *x
            }
            ("v_lshl_add_u32", [Wave::Prefix, Wave::Plain, c]) if !wild(*c) => {
                if *c == Wave::Base {
                    Wave::Base
                } else {
                    Wave::Prefix
                }
            }
            // the reservation: the first lane adds its wave's total to a shared counter
            (_, [Wave::Total(op)]) if name.contains("_atomic_") => {
                let want = match op {
                    Op::AddU => "buffer_atomic_add",
                    Op::MinU => "buffer_atomic_umin",
                    Op::MaxU => "buffer_atomic_umax",
                    Op::MinI => "buffer_atomic_smin",
                    Op::MaxI => "buffer_atomic_smax",
                    Op::Or => "buffer_atomic_or",
                    Op::And => "buffer_atomic_and",
                    Op::Xor => "buffer_atomic_xor",
                    _ => "",
                };
                if name != want || !first_mask(firsts, inst.read(Reg::S(EXEC))) {
                    return Err(TOTAL);
                }
                reserved = true;
                Wave::Base
            }
            _ => return Err(why(w)),
        };
        Ok((out, reserved))
    };
    let sections: Vec<_> = (0..p.blocks.len())
        .map(|b| super::super::wave::sections(p, b))
        .collect();
    let order = crate::ir::reverse_postorder(&p.blocks);
    let pass = |kind: &mut HashMap<Value, Wave>,
                firsts: &mut HashSet<Value>,
                check: bool|
     -> Result<(), (usize, Why)> {
        for &b in &order {
            let block = &p.blocks[b];
            for phi in &block.phis {
                let mut k = Wave::Plain;
                for (n, &a) in phi.args.iter().enumerate() {
                    let x = of(kind, Some(a));
                    k = match (k, x) {
                        _ if n == 0 => x,
                        (k, x) if k == x => k,
                        (Wave::Plain, m @ (Wave::Base | Wave::Prefix))
                        | (m @ (Wave::Base | Wave::Prefix), Wave::Plain) => m,
                        _ => Wave::Mixed,
                    };
                }
                put(kind, phi.value, k);
            }
            for (i, inst) in block.insts.iter().enumerate() {
                // inside a whole-wave section: what the simulation found each value to be
                if let Some((_, sec)) = sections[b].iter().find(|(s, sec)| i > **s && i < sec.end) {
                    for &(_, w) in &inst.writes {
                        let k = match sec.shapes.get(&w) {
                            None => Wave::Mixed,
                            Some(&Shape::K(_)) => Wave::Plain,
                            Some(&Shape::Value(v)) => of(kind, Some(v)),
                            Some(&Shape::Masked(src, _, _)) => of(kind, Some(src)),
                            Some(&Shape::Active(op, src, mask, _)) => match of(kind, Some(src)) {
                                Wave::Plain | Wave::Base => Wave::Total(op),
                                // the first lane's own sum of a total is that total
                                Wave::Total(o) if o == op && first_mask(firsts, Some(mask)) => {
                                    Wave::Total(op)
                                }
                                _ => Wave::Mixed,
                            },
                            Some(&Shape::Prefix(Op::AddU, src, mask, _, _)) => {
                                match of(kind, Some(src)) {
                                    Wave::Plain | Wave::Base => Wave::Prefix,
                                    Wave::Total(Op::AddU) if first_mask(firsts, Some(mask)) => {
                                        Wave::Prefix
                                    }
                                    _ => Wave::Mixed,
                                }
                            }
                            Some(_) => Wave::Mixed,
                        };
                        put(kind, w, k);
                    }
                    continue;
                }
                let name = inst.name();
                if name.starts_with("s_cbranch") {
                    continue; // with the block's exit, below
                }
                let (k, reserved) = match step(inst, kind, firsts) {
                    Ok(r) => r,
                    Err(why) if check => return Err((inst.machine.offset, why)),
                    Err(_) => (Wave::Mixed, false),
                };
                // the same test as `Lower::inst`'s `effect`
                let effect = matches!(inst.fields, Fields::Exp { .. })
                    || matches!(inst.fields, Fields::Flat { .. })
                        && !name.starts_with("global_load_")
                    || name.contains("store")
                    || name.contains("atomic")
                    || matches!(inst.fields, Fields::Ds { .. })
                        && !(name.starts_with("ds_read")
                            || name.starts_with("ds_swizzle")
                            || name.contains("permute"));
                if check && effect && !reserved && first_mask(firsts, inst.read(Reg::S(EXEC))) {
                    return Err((inst.machine.offset, EFFECT));
                }
                let guarded = st.guards[b][i].is_some();
                for &(r, w) in &inst.writes {
                    let mut kw = match r {
                        // "some lane is counted" keeps the count's kind; any other SCC is one bit of data
                        Reg::Scc if !name.starts_with("s_bcnt1") => {
                            if wild(k) {
                                Wave::Mixed
                            } else {
                                Wave::Plain
                            }
                        }
                        _ => k,
                    };
                    // a guarded write leaves the old value in the lanes outside EXEC
                    if guarded
                        && matches!(r, Reg::V(_))
                        && kw == Wave::Plain
                        && matches!(
                            of(kind, inst.read(r)),
                            Wave::First | Wave::Total(_) | Wave::Prefix
                        )
                    {
                        kw = Wave::Mixed;
                    }
                    put(kind, w, kw);
                }
            }
            // a branch on "some lane is counted": fine where the structure made it the lane's
            // own bit (the mask is EXEC under a scalar condition)
            if check
                && let Some(last) = block.insts.last()
                && last.name().starts_with("s_cbranch_scc")
                && let Some(scc) = last.read(Reg::Scc)
                && wild(of(kind, Some(scc)))
            {
                let own = matches!(st.exits[b], LaneExit::Branch { when, .. }
                    if !st.bools.support(when).iter().any(|a| matches!(a, Atom::Uniform(v) if *v == scc)));
                if !own {
                    return Err((last.machine.offset, TOTAL));
                }
            }
        }
        Ok(())
    };
    let mut kind: HashMap<Value, Wave> = HashMap::new();
    let mut firsts: HashSet<Value> = HashSet::new();
    // to a fixpoint (values carried around loops), then once more to say what is wrong
    let mut settled = false;
    for _ in 0..16 {
        let before = (kind.clone(), firsts.len());
        pass(&mut kind, &mut firsts, false)?;
        if (&before.0, before.1) == (&kind, firsts.len()) {
            settled = true;
            break;
        }
    }
    if !settled {
        return Err((0, MIXED));
    }
    pass(&mut kind, &mut firsts, true)
}

pub(crate) fn with_local_waves<R>(f: impl FnOnce() -> R) -> R {
    let was = LOCAL_WAVES.replace(true);
    let r = f();
    LOCAL_WAVES.set(was);
    r
}

pub(crate) fn with_local_waves_any<R>(f: impl FnOnce() -> R) -> R {
    let was = LOCAL_ANY.replace(true);
    let r = with_local_waves(f);
    LOCAL_ANY.set(was);
    r
}

impl<'a> Lower<'a> {
    /// An instruction inside a whole-wave section: the values that leave
    /// the section are made from what the simulation found them to be.
    pub(super) fn section_inst(&mut self, b: usize, i: usize, start: usize) {
        const UNKNOWN: Why = "a whole-wave section step the simulation does not follow";
        let inst = &self.x.p.blocks[b].insts[i];
        for &(_, w) in &inst.writes {
            let val = if self.section_out.contains(&w) {
                let shape = self.sections[&b][&start].shapes.get(&w).copied();
                let own = self.own_bits.totals.contains(&w);
                match shape.ok_or(UNKNOWN).and_then(|sh| self.shape(sh, own)) {
                    Ok(x) => Val::I32(x),
                    Err(why) => Val::Missing(why),
                }
            } else {
                Val::Missing(UNKNOWN)
            };
            self.vals.insert(w, val);
        }
    }

    /// A value a whole-wave section leaves, as DXIL wave operations over the
    /// host's active lanes: the console's result with console lanes; on
    /// another wave size, what the HLSL wave operation the console compiler
    /// expanded gives on that wave.
    pub(super) fn shape(&mut self, shape: super::super::wave::Shape, own: bool) -> Result<V, Why> {
        use super::super::wave::{Op, Shape};
        let (op, src, mask, ident, prefix) = match shape {
            Shape::K(k) => return Ok(self.s.m.i32(k)),
            Shape::Value(v) => return self.i32_of(v),
            Shape::Active(op, src, mask, ident) => (op, src, mask, ident, None),
            Shape::Half(_, src, mask, ident, hi) => {
                // The high half is a console lane's: compute only.
                if self.mask_data != MaskData::Console {
                    return Err("a 16-bit wave reduction outside compute");
                }
                let c = self.cur;
                let &m = self
                    .x
                    .st
                    .masks
                    .get(&mask)
                    .ok_or("a wave reduction's untracked mask")?;
                let on = self.boolean(m, c)?;
                let x = self.i32_of(src)?;
                let id = self.s.m.i32(ident);
                let x = self.s.m.select(c, on, x, id);
                self.s.flags |= flags::WAVE_OPS;
                let low = self.s.m.i32(0xFFFF);
                let r = if self.x.native.wave64 {
                    self.half_sum(x)?
                } else {
                    // shortcut: upstream wave32 sums in f32 and rounds once.
                    let h = self.s.m.bin(c, Bin::And, x, low);
                    let f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, self.s.t.f32, 131, &[h]);
                    let (add, signed) = (self.s.m.i8(0), self.s.m.i8(0));
                    let sum = self.s.op(
                        c,
                        "waveActiveOp.f32",
                        Attr::NoUnwind,
                        self.s.t.f32,
                        119,
                        &[f, add, signed],
                    );
                    self.s.op(
                        c,
                        "legacyF32ToF16",
                        Attr::ReadNone,
                        self.s.t.i32,
                        130,
                        &[sum],
                    )
                };
                let r = self.s.m.bin(c, Bin::And, r, low);
                let paired = self.exchange.as_ref().is_some_and(|e| !e.1);
                // Lane `hi` runs the section on the console even where it is
                // elsewhere on the host; there it holds the identity.
                let i32t = self.s.t.i32;
                let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
                let b = self
                    .s
                    .op(c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[on]);
                let word = self.s.m.extract(c, b, if paired { 0 } else { hi / 32 });
                let (sh, one, z) = (self.s.m.i32(hi % 32), self.s.m.i32(1), self.s.m.i32(0));
                let bit = self.s.m.bin(c, Bin::LShr, word, sh);
                let bit = self.s.m.bin(c, Bin::And, bit, one);
                let there = self.s.m.cmp(c, pred::NE, bit, z);
                let lane = self.s.m.i32(if paired { hi % 32 } else { hi });
                let at = self.read_lane(x, lane);
                let top = self.s.m.select(c, there, at, id);
                let top = if paired {
                    let (lo, upper) = self.exchange_halves(c, top);
                    if hi < 32 { lo } else { upper }
                } else {
                    top
                };
                let high = self.s.m.i32(0xFFFF_0000);
                let top = self.s.m.bin(c, Bin::And, top, high);
                return Ok(self.s.m.bin(c, Bin::Or, top, r));
            }
            Shape::ActiveF64(lo, hi, mask, high) => {
                return self.active_f64_sum(lo, hi, mask, high);
            }
            Shape::Masked(src, mask, ident) => {
                let c = self.cur;
                let &m = self
                    .x
                    .st
                    .masks
                    .get(&mask)
                    .ok_or("a wave reduction's untracked mask")?;
                let on = self.boolean(m, c)?;
                let x = self.i32_of(src)?;
                let id = self.s.m.i32(ident);
                return Ok(self.s.m.select(c, on, x, id));
            }
            Shape::Prefix(op, src, mask, ident, inclusive) => {
                (op, src, mask, ident, Some(inclusive))
            }
        };
        // Mesh: no host wave size can be declared, so a reduction is over this host wave
        // only. A minimum or maximum is taken that way: the compiler's "next distinct
        // value" loop (the wave's minimum, then the lanes that hold it) serves every
        // lane however the lanes are divided into waves. A sum, a bit reduction or a
        // prefix would be of part of a console wave: still refused.
        if self.mask_data == MaskData::None
            && (prefix.is_some()
                || !matches!(
                    op,
                    Op::MinU | Op::MaxU | Op::MinI | Op::MaxI | Op::MinF | Op::MaxF
                ))
        {
            return Err("a wave reduction in a mesh shader");
        }
        let c = self.cur;
        let &m = self
            .x
            .st
            .masks
            .get(&mask)
            .ok_or("a wave reduction's untracked mask")?;
        let on = self.boolean(m, c)?;
        let x = self.i32_of(src)?;
        let id = self.s.m.i32(ident);
        let x = self.s.m.select(c, on, x, id);
        self.s.flags |= flags::WAVE_OPS;
        if op == Op::AddH {
            return match prefix {
                Some(_) => Err("a wave prefix of an operation other than a sum"),
                None => self.half_sum(x),
            };
        }
        let float = matches!(op, Op::AddF | Op::MinF | Op::MaxF);
        let (i32t, f32t) = (self.s.t.i32, self.s.t.f32);
        let arg = if float { self.as_float(x) } else { x };
        let ty = if float { f32t } else { i32t };
        let sfx = if float { "f32" } else { "i32" };
        let r = match (op, prefix) {
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
        };
        // two host waves of 32 hold the console's 64 lanes: their results combine
        let r = if self.local || self.exchange.as_ref().is_some_and(|e| e.1) {
            // this host wave's own result (and prefix): each wave on its own, or a group of
            // 32 threads or fewer, which one host wave holds whole (combining with the 0 a
            // missing half "gives" broke AND, MIN and signed MAX)
            r
        } else if own && prefix.is_none() && op == Op::Or && !self.all_lanes(self.cur_ir) {
            // each thread walks the bits of this value and works only at its own, which the
            // host wave's OR holds: the same run as with the console wave's OR
            r
        } else if self.exchange.is_some() {
            if !self.all_lanes(self.cur_ir) {
                need(self.cur_ir);
                return Err("a wave reduction where not every lane runs");
            }
            let bits = |l: &mut Self, v: V| if float { l.as_int(v) } else { v };
            let back = |l: &mut Self, v: V| if float { l.as_float(v) } else { v };
            match prefix {
                Some(_) => {
                    // the upper half adds the lower half's total to its prefix
                    let (k0, ku) = (self.s.m.i8(0), self.s.m.i8(u8::from(!float)));
                    let total = self.s.op(
                        c,
                        &format!("waveActiveOp.{sfx}"),
                        Attr::NoUnwind,
                        ty,
                        119,
                        &[arg, k0, ku],
                    );
                    let tb = bits(self, total);
                    let c = self.cur;
                    let (lo, _) = self.exchange_halves(c, tb);
                    let lo = back(self, lo);
                    let cl = self.console_lane();
                    let k32 = self.s.m.i32(32);
                    let upper = self.s.m.cmp(c, pred::IUGE, cl, k32);
                    let zero = if float {
                        self.s.m.f32(0)
                    } else {
                        self.s.m.i32(0)
                    };
                    let add = self.s.m.select(c, upper, lo, zero);
                    self.s.m.bin(c, Bin::Add, r, add)
                }
                None => {
                    let rb = bits(self, r);
                    let c = self.cur;
                    let (lo, hi) = self.exchange_halves(c, rb);
                    let (lo, hi) = (back(self, lo), back(self, hi));
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
            }
        } else {
            r
        };
        Ok(if float { self.as_int(r) } else { r })
    }

    pub(super) fn active_f64_sum(
        &mut self,
        lo: Value,
        hi: Value,
        mask: Value,
        high: bool,
    ) -> Result<V, Why> {
        if self.mask_data == MaskData::None {
            return Err("a wave reduction in a mesh shader");
        }
        let c = self.cur;
        let &m = self
            .x
            .st
            .masks
            .get(&mask)
            .ok_or("a wave reduction's untracked mask")?;
        let on = self.boolean(m, c)?;
        let lo = self.i32_of(lo)?;
        let hi = self.i32_of(hi)?;
        let z = self.s.m.i32(0);
        let lo = self.s.m.select(c, on, lo, z);
        let hi = self.s.m.select(c, on, hi, z);
        self.s.flags |= flags::WAVE_OPS | flags::DOUBLES;
        let f64t = self.s.m.double();
        let i32t = self.s.t.i32;
        let split_ty = self
            .s
            .m
            .named_struct("dx.types.splitdouble", vec![i32t, i32t]);
        let arg = self
            .s
            .op(c, "makeDouble.f64", Attr::ReadNone, f64t, 101, &[lo, hi]);
        let (k0, k0b) = (self.s.m.i8(0), self.s.m.i8(0));
        let sum = self.s.op(
            c,
            "waveActiveOp.f64",
            Attr::NoUnwind,
            f64t,
            119,
            &[arg, k0, k0b],
        );
        let split = self
            .s
            .op(c, "splitDouble.f64", Attr::ReadNone, split_ty, 102, &[sum]);
        let mut r_lo = self.s.m.extract(c, split, 0);
        let mut r_hi = self.s.m.extract(c, split, 1);
        if self.exchange.is_some() {
            if !self.all_lanes(self.cur_ir) {
                need(self.cur_ir);
                return Err("a wave reduction where not every lane runs");
            }
            let (lo0, lo1) = self.exchange_halves(self.cur, r_lo);
            let (hi0, hi1) = self.exchange_halves(self.cur, r_hi);
            let c = self.cur;
            let d0 = self
                .s
                .op(c, "makeDouble.f64", Attr::ReadNone, f64t, 101, &[lo0, hi0]);
            let d1 = self
                .s
                .op(c, "makeDouble.f64", Attr::ReadNone, f64t, 101, &[lo1, hi1]);
            let total = self.s.m.bin(c, Bin::Add, d0, d1);
            let split = self.s.op(
                c,
                "splitDouble.f64",
                Attr::ReadNone,
                split_ty,
                102,
                &[total],
            );
            r_lo = self.s.m.extract(c, split, 0);
            r_hi = self.s.m.extract(c, split, 1);
        }
        Ok(if high { r_hi } else { r_lo })
    }

    pub(super) fn half_sum(&mut self, x: V) -> Result<V, Why> {
        const WHERE: Why = "a wave sum of 16-bit floats where not every lane runs";
        let threads = self
            .s
            .threads
            .ok_or("a wave sum of 16-bit floats outside compute")?;
        if threads.iter().product::<u32>() % 64 != 0 {
            return Err("a wave sum of 16-bit floats in a wave that is not full");
        }
        if self.local {
            return Err("a wave sum of 16-bit floats in a half-wave");
        }
        // two host waves of 32 that exchange, or one host wave of 64 lanes
        let halves = match (&self.exchange, self.s.wave_size) {
            (Some((_, false, _)), _) => true,
            (None, Some(64)) if self.x.p.wave64 => false,
            _ => return Err("a wave sum of 16-bit floats on these waves"),
        };
        let every = if halves {
            self.all_lanes(self.cur_ir)
        } else {
            all_lane_blocks(self.x.st)
                .is_some_and(|all| all.get(self.cur_ir).copied().unwrap_or(false))
        };
        if !every {
            if halves {
                need(self.cur_ir);
                return Err(WHERE);
            }
            return Ok(self.half_sum_present(x));
        }
        let c = self.cur;
        let i32t = self.s.t.i32;
        let lane = self
            .s
            .op(c, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let mut sum = x;
        for step in 0..if halves { 5 } else { 6 } {
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
        let mine = self.s.m.bin(c, Bin::Or, first, sum);
        if !halves {
            return Ok(mine);
        }
        let (lo, hi) = self.exchange_halves(c, mine);
        let c = self.cur;
        let total = self.add_halves(lo, hi);
        let total = self.s.m.bin(c, Bin::And, total, low);
        let first = self.s.m.bin(c, Bin::And, lo, high);
        Ok(self.s.m.bin(c, Bin::Or, first, total))
    }

    pub(super) fn half_sum_present(&mut self, x: V) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        self.s.flags |= flags::WAVE_OPS;
        let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
        let here = self.s.m.bool(true);
        let ballot = self
            .s
            .op(c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[here]);
        let (blo, bhi) = (
            self.s.m.extract(c, ballot, 0),
            self.s.m.extract(c, ballot, 1),
        );
        let lane = self
            .s
            .op(c, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let [zero, k31, k32] = [0, 31, 32].map(|k| self.s.m.i32(k));
        // The lanes of the run of `size` lanes at `base` that are here, from bit 0.
        let present = |l: &mut Self, base: V, size: u32| -> V {
            let upper = l.s.m.cmp(c, pred::IUGE, base, k32);
            let word = l.s.m.select(c, upper, bhi, blo);
            let at = l.s.m.bin(c, Bin::And, base, k31);
            let bits = l.s.m.bin(c, Bin::LShr, word, at);
            if size >= 32 {
                bits
            } else {
                let m = l.s.m.i32((1u32 << size) - 1);
                l.s.m.bin(c, Bin::And, bits, m)
            }
        };
        let mut sum = x;
        for step in 0..6u32 {
            let size = 1u32 << step;
            let (bit, run) = (self.s.m.i32(size), self.s.m.i32(!(size - 1)));
            let other = self.s.m.bin(c, Bin::Xor, lane, bit);
            let base = self.s.m.bin(c, Bin::And, other, run);
            let bits = present(self, base, size);
            // FirstbitLo (dx.op 32): the run's first lane that is here
            let first = self
                .s
                .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[bits]);
            let any = self.s.m.cmp(c, pred::NE, bits, zero);
            let first = self.s.m.select(c, any, first, zero);
            let from = self.s.m.bin(c, Bin::Add, base, first);
            let theirs = self.read_lane(sum, from);
            let theirs = self.s.m.select(c, any, theirs, zero);
            sum = self.add_halves(sum, theirs);
        }
        let (high, low, one) = (
            self.s.m.i32(0xFFFF_0000),
            self.s.m.i32(0xFFFF),
            self.s.m.i32(1),
        );
        let lane0 = self.s.m.bin(c, Bin::And, blo, one);
        let lane0 = self.s.m.cmp(c, pred::NE, lane0, zero);
        let first = self.read_lane(x, zero);
        let first = self.s.m.select(c, lane0, first, zero);
        let first = self.s.m.bin(c, Bin::And, first, high);
        let sum = self.s.m.bin(c, Bin::And, sum, low);
        self.s.m.bin(c, Bin::Or, first, sum)
    }

    pub(super) fn mask_dword(&mut self, v: Value) -> Result<V, Why> {
        let (lo, high) = match self.x.st.highs.get(&v) {
            Some(&lo) => (lo, true),
            None => (v, false),
        };
        let &m = self.x.st.masks.get(&lo).ok_or(LANE_MASK)?;
        // A mask that holds lanes outside EXEC where it is written (the complement of one,
        // say) is the ballot of the lanes that are there: all of them, where two exchanging
        // host waves know every lane runs that block.
        let made = self.def_block.get(&lo).copied();
        let whole =
            self.exchange.is_some() && !self.local && made.is_none_or(|b| self.all_lanes(b));
        if !self.x.st.active.contains(&lo) && !whole && !self.ballots.contains_key(&lo) {
            // Upstream reconstructs masks outside EXEC from their scalar
            // operands. Keep that exact path before asking to widen a region.
            match self.mask_number(lo) {
                Ok(xy) => {
                    self.ballots.insert(lo, xy);
                }
                Err(why) => {
                    if self.exchange.is_some() && !self.local {
                        need(made.unwrap_or(0));
                    }
                    return Err(why);
                }
            }
        }
        let (x, y) = match self.ballots.get(&lo) {
            Some(&xy) => xy,
            None => {
                let at = match self.def_block.get(&lo) {
                    // Program inputs (EXEC on entry).
                    None => self.s.entry,
                    Some(&b) if b == self.cur_ir => self.cur,
                    Some(&b) => self.tail[b].ok_or("a lane mask written after its use")?,
                };
                if !self.balloting.insert(lo) {
                    return Err("a lane mask whose lanes depend on its own value");
                }
                let bit = self.boolean(m, at);
                self.balloting.remove(&lo);
                let bit = bit?;
                let i32t = self.s.t.i32;
                let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
                let r = self
                    .s
                    .op(at, "waveActiveBallot", Attr::NoUnwind, four, 116, &[bit]);
                self.s.flags |= flags::WAVE_OPS;
                let mut xy = (self.s.m.extract(at, r, 0), self.s.m.extract(at, r, 1));
                if self.exchange.is_some() {
                    // a host wave's ballot is its half's 32 bits: the halves exchange them
                    let b = self.def_block.get(&lo).copied();
                    if self.local {
                        if !self.local_ok && !self.local_any {
                            return Err("a lane mask read as data in a half-wave");
                        }
                    } else if !self.exchange.as_ref().is_some_and(|e| e.1)
                        && !b.is_none_or(|b| self.all_lanes(b))
                    {
                        // (a group of 32 or fewer has no other half to exchange with)
                        need(b.unwrap_or(0));
                        return Err("a lane mask exchanged where not every lane runs");
                    }
                    xy = self.exchange_halves(at, xy.0);
                }
                self.ballots.insert(lo, xy);
                xy
            }
        };
        Ok(if high { y } else { x })
    }

    pub(super) fn boolean(&mut self, b: Bool, at: usize) -> Result<V, Why> {
        if b == Bool::TRUE || b == Bool::FALSE {
            return Ok(self.s.m.bool(b == Bool::TRUE));
        }
        if let Some(&v) = self.bools.get(&(at, b)) {
            return Ok(v);
        }
        let (atom, lo, hi) = self.x.st.bools.node(b).expect("not a constant");
        let a = self.atom(atom, at)?;
        let m = &mut self.s.m;
        let v = match (lo, hi) {
            (Bool::FALSE, Bool::TRUE) => a,
            (Bool::TRUE, Bool::FALSE) => {
                let t = m.bool(true);
                m.bin(at, Bin::Xor, a, t)
            }
            (Bool::FALSE, h) => {
                let h = self.boolean(h, at)?;
                self.s.m.bin(at, Bin::And, a, h)
            }
            (l, Bool::TRUE) => {
                let l = self.boolean(l, at)?;
                self.s.m.bin(at, Bin::Or, a, l)
            }
            (l, h) => {
                let l = self.boolean(l, at)?;
                let h = self.boolean(h, at)?;
                self.s.m.select(at, a, h, l)
            }
        };
        self.bools.insert((at, b), v);
        Ok(v)
    }

    pub(super) fn wave_base(&mut self) -> Option<V> {
        let slot = self.halves?;
        if let Some(v) = self.wave_base {
            return Some(v);
        }
        let (e, i32t) = (self.s.entry, self.s.t.i32);
        let lds = self.s.lds();
        let k = self.s.m.i32(slot);
        let ptr = self.s.m.element(e, lds, k);
        let zero = self.s.m.i32(0);
        self.s.m.store(e, ptr, zero);
        self.group_sync(e);
        let lane = self
            .s
            .op(e, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let first = self.s.m.cmp(e, pred::EQ, lane, zero);
        let k32 = self.s.m.i32(32);
        let add = self.s.m.select(e, first, k32, zero);
        let ptr = self.s.m.element(e, lds, k);
        let old = self.s.m.atomic(e, Rmw::Add, ptr, add);
        let base = self.s.op(
            e,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[old],
        );
        self.s.flags |= flags::WAVE_OPS;
        self.wave_base = Some(base);
        Some(base)
    }

    pub(super) fn console_thread(&mut self) -> V {
        let (e, i32t) = (self.s.entry, self.s.t.i32);
        let lane = self
            .s
            .op(e, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        if let Some(base) = self.wave_base() {
            return self.s.m.bin(e, Bin::Add, base, lane);
        }
        let flat = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            i32t,
            96,
            &[],
        );
        let first = self.s.op(
            e,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[flat],
        );
        let wave = self.s.wave_size.unwrap_or(32);
        let mask = self.s.m.i32(!(wave - 1));
        let base = self.s.m.bin(e, Bin::And, first, mask);
        self.s.m.bin(e, Bin::Add, base, lane)
    }

    pub(super) fn exchange_halves(&mut self, at: usize, mine: V) -> (V, V) {
        let Some((slot, small, _)) = self.exchange.as_ref().map(|(a, b, _)| (*a, *b, ())) else {
            let z = self.s.m.i32(0);
            return (mine, z);
        };
        if small {
            let z = self.s.m.i32(0);
            return (mine, z);
        }
        if self.local {
            // each host wave on its own: its half holds `mine`, the other nothing
            let cl = self.console_lane();
            let (k32, z) = (self.s.m.i32(32), self.s.m.i32(0));
            let upper = self.s.m.cmp(at, pred::IUGE, cl, k32);
            let lo = self.s.m.select(at, upper, z, mine);
            let hi = self.s.m.select(at, upper, mine, z);
            return (lo, hi);
        }
        // the pair of dwords of this thread's console wave: its half's, then both
        let pair = self.exchange_pair(slot);
        let cl = self.console_lane();
        let five = self.s.m.i32(5);
        let half = self.s.m.bin(at, Bin::LShr, cl, five);
        let index = self.s.m.bin(at, Bin::Add, pair, half);
        let lds = self.s.lds();
        let ptr = self.s.m.element(at, lds, index);
        self.s.m.store(at, ptr, mine);
        self.group_sync(at);
        let one = self.s.m.i32(1);
        let k1 = self.s.m.bin(at, Bin::Add, pair, one);
        let p0 = self.s.m.element(at, lds, pair);
        let p1 = self.s.m.element(at, lds, k1);
        let lo = self.s.m.load(at, p0);
        let hi = self.s.m.load(at, p1);
        // the slots are written again only after every thread read them
        self.group_sync(at);
        (lo, hi)
    }

    pub(super) fn exchange_pair(&mut self, slot: u32) -> V {
        if let Some(v) = self.pair {
            return v;
        }
        let e = self.s.entry;
        let thread = self.console_thread();
        let six = self.s.m.i32(6);
        let wave = self.s.m.bin(e, Bin::LShr, thread, six);
        let one = self.s.m.i32(1);
        let twice = self.s.m.bin(e, Bin::Shl, wave, one);
        let base = self.s.m.i32(slot);
        let v = self.s.m.bin(e, Bin::Add, base, twice);
        self.pair = Some(v);
        v
    }

    pub(super) fn scc_some(&mut self, scc: Value) -> Result<V, Why> {
        let &m = self.x.st.scc_some.get(&scc).ok_or(LANE_MASK)?;
        // (not for a console wave cut into host waves that do not exchange)
        if !self.console_lanes || self.local || self.exchange.is_none() && self.halves.is_some() {
            return Err(LANE_MASK);
        }
        let made = self.def_block.get(&scc).copied();
        let at = match made {
            None => self.s.entry,
            Some(b) if b == self.cur_ir => self.cur,
            Some(b) => self.tail[b].ok_or("a lane mask written after its use")?,
        };
        let bit = self.boolean(m, at)?;
        let (i1t, i32t) = (self.s.t.i1, self.s.t.i32);
        let some = self
            .s
            .op(at, "waveAnyTrue", Attr::NoUnwind, i1t, 113, &[bit]);
        self.s.flags |= flags::WAVE_OPS;
        if !self.exchange.as_ref().is_some_and(|e| !e.1) {
            return Ok(some);
        }
        if !made.is_none_or(|b| self.all_lanes(b)) {
            need(made.unwrap_or(0));
            return Err("a lane mask exchanged where not every lane runs");
        }
        let some = self.s.m.cast(at, Cast::ZExt, some, i32t);
        let (lo, hi) = self.exchange_halves(at, some);
        let both = self.s.m.bin(at, Bin::Or, lo, hi);
        let z = self.s.m.i32(0);
        Ok(self.s.m.cmp(at, pred::NE, both, z))
    }

    pub(super) fn wave_wide(&mut self, b: usize, when: Bool) -> Result<V, Why> {
        const WHY: Why = "a wave-wide branch where not every lane runs";
        let any = self
            .x
            .st
            .wave_any
            .get(b)
            .copied()
            .flatten()
            .ok_or("a wave-wide branch on a scalar condition")?;
        if self.exchange.is_none()
            && self.halves.is_none()
            && self.console_lanes
            && self.mask_data == MaskData::Console
        {
            // The host's waves are the console's: the wave's own answer. The lanes that are
            // elsewhere have no bit set, as on the console, so "some lane" needs no one but
            // the lanes here, and "every lane's bit is clear" is true of the others anyway.
            let c = self.cur;
            let mine = self.boolean(when, c)?;
            let i1t = self.s.t.i1;
            self.s.flags |= flags::WAVE_OPS;
            let (name, code) = if any {
                ("waveAnyTrue", 113)
            } else {
                ("waveAllTrue", 114)
            };
            return Ok(self.s.op(c, name, Attr::NoUnwind, i1t, code, &[mine]));
        }
        if self.local || !self.exchange.as_ref().is_some_and(|e| !e.1) {
            return Err(WHY);
        }
        if !self.all_lanes(b) {
            need(b);
            return Err(WHY);
        }
        let c = self.cur;
        let mine = self.boolean(when, c)?;
        // what the halves exchange is "some lane speaks against" either way (for `any`:
        // some lane's `when` holds; else: some lane's does not), so a half with no lane says
        // nothing
        let set = if any {
            mine
        } else {
            let t = self.s.m.bool(true);
            self.s.m.bin(c, Bin::Xor, mine, t)
        };
        let (i1t, i32t) = (self.s.t.i1, self.s.t.i32);
        let some = self
            .s
            .op(c, "waveAnyTrue", Attr::NoUnwind, i1t, 113, &[set]);
        self.s.flags |= flags::WAVE_OPS;
        let some = self.s.m.cast(c, Cast::ZExt, some, i32t);
        let (lo, hi) = self.exchange_halves(c, some);
        let c = self.cur;
        let both = self.s.m.bin(c, Bin::Or, lo, hi);
        let z = self.s.m.i32(0);
        Ok(self
            .s
            .m
            .cmp(c, if any { pred::NE } else { pred::EQ }, both, z))
    }

    pub(super) fn all_lanes(&self, b: usize) -> bool {
        self.exchange
            .as_ref()
            .is_none_or(|(_, _, all)| all.get(b).copied().unwrap_or(false))
    }

    pub(super) fn console_lane(&mut self) -> V {
        if self.exchange.is_none() {
            return self.lane_index();
        }
        if let Some(v) = self.clane {
            return v;
        }
        let (e, i32t) = (self.s.entry, self.s.t.i32);
        let f = self.console_thread();
        let _ = i32t;
        let k63 = self.s.m.i32(63);
        let cl = self.s.m.bin(e, Bin::And, f, k63);
        self.s.flags |= flags::WAVE_OPS;
        self.clane = Some(cl);
        cl
    }

    pub(super) fn lane_index(&mut self) -> V {
        if self.s.mesh.is_some() {
            // The console's lane: mesh code reads no other lanes, so host
            // waves need not match the console's.
            let f = self.mesh_thread();
            let m = self.s.m.i32(self.wave() - 1);
            return self.s.m.bin(self.s.entry, Bin::And, f, m);
        }
        self.s.op(
            self.cur,
            "waveGetLaneIndex",
            Attr::ReadOnly,
            self.s.t.i32,
            111,
            &[],
        )
    }

    pub(super) fn add_halves(&mut self, a: V, b: V) -> V {
        let c = self.cur;
        let f32t = self.s.t.f32;
        let low = self.s.m.i32(0xFFFF);
        let wide = |l: &mut Self, x: V| {
            let h = l.s.m.bin(c, Bin::And, x, low);
            l.s.op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h])
        };
        let (fa, fb) = (wide(self, a), wide(self, b));
        let f = self.s.m.bin(c, Bin::Add, fa, fb);
        self.f16_nearest(f)
    }

    /// Code that depends on the lane layout: wave operations, and in compute
    /// the console's wave size (lanes are thread ids in group order there).
    /// Pixel lanes match only within a quad.
    pub(super) fn cross_lane(&mut self, quad: bool) -> Result<(), Why> {
        if self.s.mesh.is_some() {
            // shortcut: no wave size for mesh shaders in SM 6.6, so host
            // lanes cannot follow the console's; emulate through group-shared
            // memory if mesh programs turn out to need these.
            return Err("a cross-lane operation in a mesh shader");
        }
        if self.s.threads.is_none() && !(quad && self.s.pixel) {
            return Err("a cross-lane operation outside compute");
        }
        debug_assert!(self.s.threads.is_none() || self.console_lanes);
        self.s.flags |= flags::WAVE_OPS;
        Ok(())
    }

    /// Component `c` of the thread id in the group. With console lanes, the
    /// thread is relabelled so that its host lane is its console lane: the
    /// flat id is the wave's first lane's, rounded down to the wave size,
    /// plus the lane index. (A bijection while each host wave holds a
    /// wave-aligned run of flat ids, as the console's waves do.)
    pub(super) fn thread_id(&mut self, c: u8) -> Val {
        if !self.console_lanes {
            return self.compute_id("threadIdInGroup.i32", 95, c);
        }
        let [x, y, _] = self.s.threads.expect("compute");
        let e = self.s.entry;
        let i32t = self.s.t.i32;
        if self.halves.is_some() {
            let f = self.console_thread();
            self.s.flags |= flags::WAVE_OPS;
            let (xk, yk, xy) = (self.s.m.i32(x), self.s.m.i32(y), self.s.m.i32(x * y));
            return Val::I32(match c {
                0 => self.s.m.bin(e, Bin::URem, f, xk),
                1 => {
                    let t = self.s.m.bin(e, Bin::UDiv, f, xk);
                    self.s.m.bin(e, Bin::URem, t, yk)
                }
                _ => self.s.m.bin(e, Bin::UDiv, f, xy),
            });
        }
        let flat = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            i32t,
            96,
            &[],
        );
        let first = self.s.op(
            e,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[flat],
        );
        let wave = self.s.wave_size.expect("set with console lanes");
        let mask = self.s.m.i32(!(wave - 1));
        let base = self.s.m.bin(e, Bin::And, first, mask);
        let lane = self.lane_index();
        let f = self.s.m.bin(e, Bin::Add, base, lane);
        self.s.flags |= flags::WAVE_OPS;
        let (xk, yk, xy) = (self.s.m.i32(x), self.s.m.i32(y), self.s.m.i32(x * y));
        Val::I32(match c {
            0 => self.s.m.bin(e, Bin::URem, f, xk),
            1 => {
                let t = self.s.m.bin(e, Bin::UDiv, f, xk);
                self.s.m.bin(e, Bin::URem, t, yk)
            }
            _ => self.s.m.bin(e, Bin::UDiv, f, xy),
        })
    }

    pub(super) fn first_lane(&mut self, x: V, guard: Option<V>) -> (V, V) {
        let c = self.cur;
        let i32t = self.s.t.i32;
        self.s.flags |= flags::WAVE_OPS;
        let first = self
            .s
            .op(c, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[x]);
        let Some(g) = guard else {
            return (first, self.s.m.bool(true));
        };
        let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
        let r = self
            .s
            .op(c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[g]);
        let (lo, hi) = (self.s.m.extract(c, r, 0), self.s.m.extract(c, r, 1));
        let (z, k32) = (self.s.m.i32(0), self.s.m.i32(32));
        // FirstbitLo (dx.op 32): the lowest set bit
        let flo = self
            .s
            .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[lo]);
        let fhi = self
            .s
            .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[hi]);
        let fhi = self.s.m.bin(c, Bin::Add, fhi, k32);
        let in_lo = self.s.m.cmp(c, pred::NE, lo, z);
        let in_hi = self.s.m.cmp(c, pred::NE, hi, z);
        let any = self.s.m.bin(c, Bin::Or, in_lo, in_hi);
        let lane = self.s.m.select(c, in_lo, flo, fhi);
        // no lane in EXEC: read this lane (one that is here); the read is not used
        let me = self
            .s
            .op(c, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let lane = self.s.m.select(c, any, lane, me);
        let at = self.read_lane(x, lane);
        (self.s.m.select(c, any, at, first), any)
    }

    pub(super) fn lds_meeting(&mut self) {
        let mode = self.s.m.i32(meeting_mode());
        let void = self.s.t.void;
        self.s
            .op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    pub(super) fn uniform_test(&self, inst: &Inst) -> bool {
        let p = self.x.p;
        let writer = |v: Value| -> Option<&'a Inst> {
            let b = *self.def_block.get(&v)?;
            p.blocks[b]
                .insts
                .iter()
                .find(|i| i.writes.iter().any(|w| w.1 == v))
        };
        let scalar = |i: &Inst, op: &Operand| match *op {
            Operand::Scalar { code, .. } if code != NULL && code != EXEC => i.read(Reg::S(code)),
            _ => None,
        };
        if !inst.name().starts_with("s_bcnt1_i32_") {
            return false;
        }
        let Fields::Sop { src, .. } = &inst.fields else {
            return false;
        };
        let Some(xor) = src.first().and_then(|o| scalar(inst, o)).and_then(writer) else {
            return false;
        };
        let Fields::Sop { src, .. } = &xor.fields else {
            return false;
        };
        if !matches!(xor.name(), "s_xor_b64" | "s_xor_b32")
            || !src
                .iter()
                .any(|o| matches!(o, Operand::Scalar { code: EXEC, .. }))
        {
            return false;
        }
        let Some(cmp) = src.iter().find_map(|o| scalar(xor, o)).and_then(writer) else {
            return false;
        };
        let Fields::Valu(f) = &cmp.fields else {
            return false;
        };
        if !matches!(cmp.name(), "v_cmp_eq_u32" | "v_cmp_eq_i32")
            || f.sdwa.is_some()
            || f.dpp.is_some()
        {
            return false;
        }
        let lanes = f.src.iter().find_map(|o| match *o {
            Operand::Vector { reg, .. } => cmp.read(Reg::V(reg)),
            _ => None,
        });
        let first = f.src.iter().find_map(|o| scalar(cmp, o)).and_then(writer);
        lanes.is_some()
            && first.is_some_and(|fl| {
                fl.name() == "v_readfirstlane_b32"
                    && matches!(&fl.fields, Fields::Valu(g)
                        if matches!(g.src.first(), Some(&Operand::Vector { reg, .. })
                            if fl.read(Reg::V(reg)) == lanes))
            })
    }

    pub(super) fn group_sync(&mut self, block: usize) {
        let mode = self.s.m.i32(meeting_mode());
        let void = self.s.t.void;
        self.s
            .op(block, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    /// This lane's bit of the mask `lo`, `hi` (wave64).
    pub(super) fn lane_bit(&mut self, at: usize, lo: V, hi: Option<V>) -> V {
        self.s.flags |= flags::WAVE_OPS;
        let lane = if self.exchange.is_some() {
            self.console_lane()
        } else {
            self.s.op(
                at,
                "waveGetLaneIndex",
                Attr::ReadOnly,
                self.s.t.i32,
                111,
                &[],
            )
        };
        let (k31, k32, one, z) = (
            self.s.m.i32(31),
            self.s.m.i32(32),
            self.s.m.i32(1),
            self.s.m.i32(0),
        );
        let word = match hi {
            Some(hi) => {
                let low = self.s.m.cmp(at, pred::IULT, lane, k32);
                self.s.m.select(at, low, lo, hi)
            }
            None => lo,
        };
        let sh = self.s.m.bin(at, Bin::And, lane, k31);
        let t = self.s.m.bin(at, Bin::LShr, word, sh);
        let t = self.s.m.bin(at, Bin::And, t, one);
        self.s.m.cmp(at, pred::NE, t, z)
    }

    pub(super) fn f16_nearest(&mut self, f: V) -> V {
        let c = self.cur;
        let f32t = self.s.t.f32;
        let lo = self.f16_toward_zero(f);
        let (one, zero) = (self.s.m.i32(1), self.s.m.i32(0));
        let hi = self.s.m.bin(c, Bin::Add, lo, one);
        let size = |l: &mut Self, h: V| {
            let w = l.s.op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
            l.unary(6, w)
        };
        let (below, above) = (size(self, lo), size(self, hi));
        let (magnitude, infinity) = (self.s.m.i32(0x7FFF), self.s.m.i32(0x7C00));
        let m = self.s.m.bin(c, Bin::And, hi, magnitude);
        let top = self.s.m.cmp(c, pred::EQ, m, infinity);
        let bound = self.s.m.f32(65536.0f32.to_bits());
        let above = self.s.m.select(c, top, bound, above);
        let fa = self.unary(6, f);
        // (both differences are exact: the three values lie within one half's step)
        let under = self.s.m.bin(c, Bin::Sub, fa, below);
        let over = self.s.m.bin(c, Bin::Sub, above, fa);
        // Ordered: never for NaN, which stays what the conversion made of it.
        let nearer = self.s.m.cmp(c, pred::OLT, over, under);
        let tie = self.s.m.cmp(c, pred::OEQ, over, under);
        let odd = self.s.m.bin(c, Bin::And, lo, one);
        let odd = self.s.m.cmp(c, pred::NE, odd, zero);
        let tie = self.s.m.bin(c, Bin::And, tie, odd);
        let up = self.s.m.bin(c, Bin::Or, nearer, tie);
        self.s.m.select(c, up, hi, lo)
    }

    pub(super) fn f64_pair(&mut self, inst: &Inst, k: usize) -> Result<(V, V), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        if v.sdwa.is_some() || v.dpp.is_some() {
            return Err("64-bit operand modifiers");
        }
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let pair = |l: &mut Self, r: Reg, r1: Reg| -> Result<(V, V), Why> {
            let lo = l.i32_of(inst.read(r).ok_or("an unread operand")?)?;
            let hi = l.i32_of(inst.read(r1).ok_or("an unread operand")?)?;
            Ok((lo, hi))
        };
        let (lo, mut hi) = match op {
            Operand::Vector { reg, .. } => pair(self, Reg::V(reg), Reg::V(reg + 1))?,
            Operand::Scalar { code, .. } if code != NULL => {
                pair(self, Reg::S(code), Reg::S(code + 1))?
            }
            Operand::Const(128) => (self.s.m.i32(0), self.s.m.i32(0)),
            Operand::Const(c @ 240..=248) => {
                const INLINE_F64: [f64; 9] = [
                    0.5,
                    -0.5,
                    1.0,
                    -1.0,
                    2.0,
                    -2.0,
                    4.0,
                    -4.0,
                    1.0 / std::f64::consts::TAU,
                ];
                let bits = INLINE_F64[(c - 240) as usize].to_bits();
                (self.s.m.i32(bits as u32), self.s.m.i32((bits >> 32) as u32))
            }
            // GFX10 places a 32-bit literal in the high 32 bits of a 64-bit float operand.
            Operand::Literal(x) => (self.s.m.i32(0), self.s.m.i32(x)),
            _ => return Err("a 64-bit float operand"),
        };
        let c = self.cur;
        if v.abs >> k & 1 != 0 {
            let mask = self.s.m.i32(0x7FFF_FFFF);
            hi = self.s.m.bin(c, Bin::And, hi, mask);
        }
        if v.neg >> k & 1 != 0 {
            let sign = self.s.m.i32(0x8000_0000);
            hi = self.s.m.bin(c, Bin::Xor, hi, sign);
        }
        Ok((lo, hi))
    }

    pub(super) fn join64(&mut self, lo: V, hi: V) -> V {
        let c = self.cur;
        let ty = self.s.m.int(64);
        let lo = self.s.m.cast(c, Cast::ZExt, lo, ty);
        let hi = self.s.m.cast(c, Cast::ZExt, hi, ty);
        let shift = self.s.m.int_const(ty, 32);
        let hi = self.s.m.bin(c, Bin::Shl, hi, shift);
        self.s.m.bin(c, Bin::Or, lo, hi)
    }

    pub(super) fn f64_to_f32_bits(&mut self, lo: V, hi: V) -> V {
        let c = self.cur;
        let k0 = self.s.m.i32(0);
        let k1 = self.s.m.i32(1);
        let sign_mask = self.s.m.i32(0x8000_0000);
        let sign = self.s.m.bin(c, Bin::And, hi, sign_mask);

        let k20 = self.s.m.i32(20);
        let k7ff = self.s.m.i32(0x7FF);
        let exp64 = self.s.m.bin(c, Bin::LShr, hi, k20);
        let exp64 = self.s.m.bin(c, Bin::And, exp64, k7ff);

        let frac_mask = self.s.m.i32(0x000F_FFFF);
        let frac_hi = self.s.m.bin(c, Bin::And, hi, frac_mask);
        let frac_any = self.s.m.bin(c, Bin::Or, frac_hi, lo);
        let frac_nz = self.s.m.cmp(c, pred::NE, frac_any, k0);

        let k3 = self.s.m.i32(3);
        let k29 = self.s.m.i32(29);
        let m23_hi = self.s.m.bin(c, Bin::Shl, frac_hi, k3);
        let m23_lo = self.s.m.bin(c, Bin::LShr, lo, k29);
        let m23 = self.s.m.bin(c, Bin::Or, m23_hi, m23_lo);

        let rem_mask = self.s.m.i32(0x1FFF_FFFF);
        let rem = self.s.m.bin(c, Bin::And, lo, rem_mask);
        let half_rem = self.s.m.i32(0x1000_0000);
        let gt_half = self.s.m.cmp(c, pred::IUGT, rem, half_rem);
        let eq_half = self.s.m.cmp(c, pred::EQ, rem, half_rem);
        let m23_odd_bit = self.s.m.bin(c, Bin::And, m23, k1);
        let m23_odd = self.s.m.cmp(c, pred::NE, m23_odd_bit, k0);
        let tie_up = self.s.m.bin(c, Bin::And, eq_half, m23_odd);
        let norm_round = self.s.m.bin(c, Bin::Or, gt_half, tie_up);
        let norm_inc = self.s.m.select(c, norm_round, k1, k0);

        // Normal range: exp64 in [897, 1150] -> exp32 in [1, 254].
        let k896 = self.s.m.i32(896);
        let k23 = self.s.m.i32(23);
        let exp32 = self.s.m.bin(c, Bin::Sub, exp64, k896);
        let exp32_sh = self.s.m.bin(c, Bin::Shl, exp32, k23);
        let norm_body = self.s.m.bin(c, Bin::Add, exp32_sh, m23);
        let norm_body = self.s.m.bin(c, Bin::Add, norm_body, norm_inc);
        let norm_res = self.s.m.bin(c, Bin::Or, sign, norm_body);

        // Subnormal range: exp64 in [874, 896] -> shift in [1, 23].
        let k897 = self.s.m.i32(897);
        let k32 = self.s.m.i32(32);
        let k31 = self.s.m.i32(31);
        let imp = self.s.m.i32(0x0080_0000);
        let sig24 = self.s.m.bin(c, Bin::Or, imp, m23);
        let raw_shift = self.s.m.bin(c, Bin::Sub, k897, exp64);
        // Clamp shift to [1, 23] so shifts never produce poison outside the subnormal branch.
        let lt1 = self.s.m.cmp(c, pred::IULT, raw_shift, k1);
        let shift = self.s.m.select(c, lt1, k1, raw_shift);
        let gt23 = self.s.m.cmp(c, pred::IUGT, shift, k23);
        let shift = self.s.m.select(c, gt23, k23, shift);
        let sub_m = self.s.m.bin(c, Bin::LShr, sig24, shift);
        let inv_shift = self.s.m.bin(c, Bin::Sub, k32, shift);
        let lost = self.s.m.bin(c, Bin::Shl, sig24, inv_shift);
        let round_bit = self.s.m.bin(c, Bin::LShr, lost, k31);
        let round_on = self.s.m.cmp(c, pred::NE, round_bit, k0);
        let low31 = self.s.m.i32(0x7FFF_FFFF);
        let lost_lo = self.s.m.bin(c, Bin::And, lost, low31);
        let sticky = self.s.m.bin(c, Bin::Or, lost_lo, rem);
        let sticky_on = self.s.m.cmp(c, pred::NE, sticky, k0);
        let sub_odd_bit = self.s.m.bin(c, Bin::And, sub_m, k1);
        let sub_odd = self.s.m.cmp(c, pred::NE, sub_odd_bit, k0);
        let sub_tie = self.s.m.bin(c, Bin::Or, sticky_on, sub_odd);
        let sub_round = self.s.m.bin(c, Bin::And, round_on, sub_tie);
        let sub_inc = self.s.m.select(c, sub_round, k1, k0);
        let sub_body = self.s.m.bin(c, Bin::Add, sub_m, sub_inc);
        let sub_res = self.s.m.bin(c, Bin::Or, sign, sub_body);

        // Underflow range: exp64 < 874. Only exp64 == 873 with non-zero fraction rounds up to min subnormal (1).
        let k873 = self.s.m.i32(873);
        let eq873 = self.s.m.cmp(c, pred::EQ, exp64, k873);
        let round_min = self.s.m.bin(c, Bin::And, eq873, frac_nz);
        let min_bit = self.s.m.select(c, round_min, k1, k0);
        let under_res = self.s.m.bin(c, Bin::Or, sign, min_bit);

        // Overflow / Inf / NaN: exp64 >= 1151.
        let is_exp_all = self.s.m.cmp(c, pred::EQ, exp64, k7ff);
        let is_nan = self.s.m.bin(c, Bin::And, is_exp_all, frac_nz);
        let inf_bits = self.s.m.i32(0x7F80_0000);
        let qnan_bits = self.s.m.i32(0x7FC0_0000);
        let nan_body = self.s.m.bin(c, Bin::Or, qnan_bits, m23);
        let over_body = self.s.m.select(c, is_nan, nan_body, inf_bits);
        let over_res = self.s.m.bin(c, Bin::Or, sign, over_body);

        // Select final result by exponent range.
        let k874 = self.s.m.i32(874);
        let k1151 = self.s.m.i32(1151);
        let is_under = self.s.m.cmp(c, pred::IULT, exp64, k874);
        let is_sub = self.s.m.cmp(c, pred::IULT, exp64, k897);
        let is_over = self.s.m.cmp(c, pred::IUGE, exp64, k1151);
        let r = self.s.m.select(c, is_over, over_res, norm_res);
        let r = self.s.m.select(c, is_sub, sub_res, r);
        self.s.m.select(c, is_under, under_res, r)
    }

    pub(super) fn is_nan_f32(&mut self, f: V) -> V {
        let c = self.cur;
        let bits = self.as_int(f);
        let mask = self.s.m.i32(0x7FFF_FFFF);
        let abs = self.s.m.bin(c, Bin::And, bits, mask);
        let inf = self.s.m.i32(0x7F80_0000);
        self.s.m.cmp(c, pred::IUGT, abs, inf)
    }
}
