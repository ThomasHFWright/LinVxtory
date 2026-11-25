//! Merge GPU-written bytes with concurrent CPU writes, preserving the scalar rule.

pub(crate) fn merge(title: &mut [u8], host: &mut [u8], before: &[u8]) -> bool {
    static WIDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *WIDE.get_or_init(|| crate::switch::on("VICTORY_COPYBACK_SIMD")) {
        wide(title, host, before)
    } else {
        scalar(title, host, before)
    }
}

fn scalar(title: &mut [u8], host: &mut [u8], before: &[u8]) -> bool {
    assert_eq!(title.len(), host.len());
    assert_eq!(title.len(), before.len());
    let mut changed = false;
    for ((t, h), b) in title.iter_mut().zip(host).zip(before) {
        if *h != *b && *t != *h {
            *t = *h;
            changed = true;
        }
        *h = *t;
    }
    changed
}

#[cfg(target_arch = "x86_64")]
fn wide(title: &mut [u8], host: &mut [u8], before: &[u8]) -> bool {
    use core::arch::x86_64::*;
    assert_eq!(title.len(), host.len());
    assert_eq!(title.len(), before.len());
    let end = title.len() / 16 * 16;
    let mut changed = false;
    for at in (0..end).step_by(16) {
        // SSE2 is baseline on x86-64; each unaligned access is inside its disjoint slice.
        unsafe {
            let t = _mm_loadu_si128(title.as_ptr().add(at).cast());
            let h = _mm_loadu_si128(host.as_ptr().add(at).cast());
            let b = _mm_loadu_si128(before.as_ptr().add(at).cast());
            let unchanged = _mm_cmpeq_epi8(h, b);
            let merged = _mm_or_si128(_mm_and_si128(unchanged, t), _mm_andnot_si128(unchanged, h));
            changed |= _mm_movemask_epi8(_mm_cmpeq_epi8(t, merged)) != 0xffff;
            _mm_storeu_si128(title.as_mut_ptr().add(at).cast(), merged);
            _mm_storeu_si128(host.as_mut_ptr().add(at).cast(), merged);
        }
    }
    changed | scalar(&mut title[end..], &mut host[end..], &before[end..])
}

#[cfg(not(target_arch = "x86_64"))]
fn wide(title: &mut [u8], host: &mut [u8], before: &[u8]) -> bool {
    scalar(title, host, before)
}

#[cfg(test)]
mod tests;
