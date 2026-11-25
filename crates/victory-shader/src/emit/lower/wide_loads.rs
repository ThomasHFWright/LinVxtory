//! Opt-in raw-load batching, adapted from fxceless c36971b.
use super::*;

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_WIDE_LOADS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

impl Lower<'_> {
    pub(super) fn wide_loads() -> bool {
        #[cfg(test)]
        if let Some(wide) = TEST_WIDE_LOADS.get() {
            return wide;
        }
        crate::emit::shader_opt() & crate::emit::bitcode::opt::WIDE != 0
    }

    /// Read one console instruction's raw dwords. Separate instructions
    /// never combine, and callers retain descriptor-kind and page guards.
    pub(super) fn raw_dwords(&mut self, handle: V, addr: V, count: u32) -> Vec<V> {
        assert!((1..=16).contains(&count));
        let scalar = |l: &mut Self| {
            (0..count)
                .map(|k| {
                    let off = l.s.m.i32(4 * k);
                    let at = l.s.m.bin(l.cur, Bin::Add, addr, off);
                    l.raw_dword(handle, at)
                })
                .collect::<Vec<_>>()
        };
        if count == 1 || !Self::wide_loads() {
            return scalar(self);
        }

        // The existing emitter adds each offset in i32. A vector access
        // does not wrap its later components, so retain scalar reads for
        // the rare instruction whose offsets cross the 32-bit boundary.
        let last = self.s.m.i32(u32::MAX - 4 * (count - 1));
        let wraps = self.s.m.cmp(self.cur, pred::IUGT, addr, last);
        let (head, narrow, wide, join) = (
            self.cur,
            self.s.m.new_block(),
            self.s.m.new_block(),
            self.s.m.new_block(),
        );
        self.s.m.terminate(head, Term::CondBr(wraps, narrow, wide));
        self.cur = narrow;
        let narrow_values = scalar(self);
        self.s.m.terminate(self.cur, Term::Br(join));
        self.cur = wide;
        let mut wide_values = Vec::with_capacity(count as usize);
        for first in (0..count).step_by(4) {
            let off = self.s.m.i32(4 * first);
            let at = self.s.m.bin(self.cur, Bin::Add, addr, off);
            wide_values.extend(self.memory_load_words(handle, at, (count - first).min(4) as u8));
        }
        self.s.m.terminate(self.cur, Term::Br(join));
        self.cur = join;
        narrow_values
            .into_iter()
            .zip(wide_values)
            .map(|(a, b)| {
                let phi = self.s.m.phi(join, self.s.t.i32);
                self.s.m.set_phi(phi, vec![(a, narrow), (b, wide)]);
                phi
            })
            .collect()
    }
}
