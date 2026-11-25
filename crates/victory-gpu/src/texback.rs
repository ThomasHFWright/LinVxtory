//! Textures shaders write that the title's own code then reads on the CPU.
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read. Here it stays
//! in the texture's host copy. The title's ocean is one: its wave passes write
//! a 256x256 height map (R32_FLOAT) and a displacement map (R32G32_FLOAT), and
//! its CPU code samples both. Adapted from victory-fork PR999, including
//! the allocation tickets added in 04df435.
//!
//! For textures of the kinds in `VICTORY_TEXTURE_BACK` (comma-separated
//! `<width>x<height>f<format>`; `1` for the four water formats found by
//! the upstream trace), a list in which shaders write one copies it
//! into a read-back buffer of ours when it closes, and once the list has run
//! the texels are tiled into the texture's title memory in the console's
//! layout, as the console's GPU would have left them.
//!
//! shortcut: by kind, not by texture: textures of those kinds the title never
//! reads are copied back too (about a megabyte per such list). The copy shows
//! what the list's last write left; a title read in between sees the frame
//! before, as it would before the console's GPU has run.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// The kinds copied back: (width, height, format).
fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        let kinds = parse_kinds(std::env::var("VICTORY_TEXTURE_BACK").ok().as_deref());
        if !kinds.is_empty() {
            tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (VICTORY_TEXTURE_BACK)");
        }
        kinds
    })
}

fn parse_kinds(spec: Option<&str>) -> Vec<(u64, u32, u32)> {
    let list = match spec {
        Some("1") => "256x256f41,256x256f16,64x64f16,128x128f36",
        Some(spec) => spec,
        None => "",
    };
    list.split(',')
        .filter_map(|k| {
            let (size, format) = k.trim().split_once('f')?;
            let (w, h) = size.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
        })
        .collect()
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty() && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

/// An outstanding write to a specific allocation. Mapping and resource
/// changes invalidate only the writes that could touch the replaced memory.
struct Ticket {
    owner: usize,
    at: usize,
    end: usize,
    valid: AtomicBool,
}

static TICKETS: Mutex<Vec<Weak<Ticket>>> = Mutex::new(Vec::new());
static TRACKING: AtomicBool = AtomicBool::new(false);

fn track(tickets: &mut Vec<Weak<Ticket>>, owner: usize, at: usize, end: usize) -> Arc<Ticket> {
    let ticket = Arc::new(Ticket {
        owner,
        at,
        end,
        valid: AtomicBool::new(true),
    });
    tickets.retain(|weak| weak.strong_count() != 0);
    tickets.push(Arc::downgrade(&ticket));
    ticket
}

fn invalidate(matches: impl Fn(&Ticket) -> bool) {
    if !TRACKING.load(Ordering::Acquire) {
        return;
    }
    TICKETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|weak| {
            let Some(ticket) = weak.upgrade() else {
                return false;
            };
            if matches(&ticket) {
                ticket.valid.store(false, Ordering::Release);
            }
            true
        });
}

pub(crate) fn released(owner: usize) {
    invalidate(|ticket| ticket.owner == owner);
}

pub(crate) fn replaced(at: usize) {
    invalidate(|ticket| ticket.at == at);
}

pub(crate) fn remapped(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    // Called before watch removes the old alias links, without GUARDS held.
    // A copy landed through another view before its unmap must not be overwritten later.
    let first = at / crate::watch::PAGE * crate::watch::PAGE;
    let end = at
        .saturating_add(len)
        .checked_next_multiple_of(crate::watch::PAGE)
        .unwrap_or(usize::MAX);
    invalidate(|ticket| {
        crate::watch::overlaps_buffer_writes(
            ticket.at,
            ticket.end - ticket.at,
            &[(first, end - first)],
        )
    });
}

pub(crate) struct Target {
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
}

impl Target {
    pub(crate) fn new(resource: This) -> Option<Self> {
        // Register before acquiring the host copy: replacement during that
        // acquisition must invalidate this ticket, not precede a fresh one.
        let (layout, ticket) = Self::allocation(resource)?;
        // No mapping or ticket lock across host/resource-cache locks.
        let copy = crate::resource::host_copy(resource)?;
        ticket.valid.load(Ordering::Acquire).then_some(Self {
            copy,
            layout,
            ticket,
        })
    }

    fn allocation(resource: This) -> Option<(victory_formats::texture::Layout, Arc<Ticket>)> {
        TRACKING.store(true, Ordering::Release);
        let _mapping = crate::watch::title_memory_reading();
        let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
        let (layout, at, size) = crate::resource::title_texture(resource)?;
        let end = at
            .checked_add(size)
            .filter(|&end| at != 0 && end > at && size <= isize::MAX as usize)?;
        let ticket = track(&mut tickets, resource as usize, at, end);
        Some((layout, ticket))
    }

    pub(crate) fn owner(&self) -> usize {
        self.ticket.owner
    }

    pub(crate) fn valid(&self) -> bool {
        self.ticket.valid.load(Ordering::Acquire)
    }

    pub(crate) fn range(&self) -> (usize, usize) {
        (self.ticket.at, self.ticket.end)
    }
}

/// The readback resources and the allocation the pixels belong to.
pub(crate) struct Back {
    buffer: ID3D12Resource,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
    bytes: usize,
}

impl Back {
    pub(crate) fn range(&self) -> (usize, usize) {
        (self.ticket.at, self.ticket.end)
    }
}

/// Records a host copy from a texture already in COPY_SOURCE.
pub(crate) fn record(list: &crate::host::HostList, target: Target) -> Option<Back> {
    if !target.valid() {
        return None;
    }
    let host = crate::host::host()?;
    let texture = &target.copy.texture;
    // SAFETY: a live texture kept through the command list's state tracking.
    let desc = unsafe { texture.GetDesc() };
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.SampleDesc.Count != 1 {
        return None;
    }
    let mips = u32::from(desc.MipLevels.max(1));
    let subresources = mips.checked_mul(u32::from(desc.DepthOrArraySize.max(1)))?;
    if subresources != target.copy.subresource_count()
        || mips as usize != target.layout.mips.len()
        || u32::from(desc.DepthOrArraySize) != target.layout.slices
    {
        return None;
    }
    let n = subresources as usize;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut total = 0u64;
    // SAFETY: out arrays of `n` entries.
    unsafe {
        host.device.GetCopyableFootprints(
            &desc,
            0,
            subresources,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        )
    };
    // The last footprint's advertised extent can omit row-end padding.
    // The tiler reads a pitched slice, so the mapped buffer must include it.
    total = total.max(footprints_size(&target.layout, &footprints, &rows, mips)? as u64);
    let bytes = usize::try_from(total)
        .ok()
        .filter(|&n| n <= isize::MAX as usize)?;
    let (at, end) = target.range();
    if usize::try_from(target.layout.size).ok()? > end - at {
        return None;
    }
    let (buffer, _) = list.readback(host, total)?;
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    Some(Back {
        buffer,
        footprints,
        rows,
        mips,
        layout: target.layout,
        ticket: target.ticket,
        bytes,
    })
}

/// Validate every subresource before recording copies or constructing a mapped slice.
fn footprints_size(
    layout: &victory_formats::texture::Layout,
    footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
    rows: &[u32],
    mips: u32,
) -> Option<usize> {
    if layout.desc.dimension != victory_formats::texture::Dimension::Tex2d
        || layout.desc.samples != 1
        || !layout.addressable()
        || mips == 0
        || mips as usize != layout.mips.len()
        || footprints.len() != mips.checked_mul(layout.slices)? as usize
        || footprints.len() != rows.len()
    {
        return None;
    }
    let mut total = 0;
    for (i, (footprint, &rows)) in footprints.iter().zip(rows).enumerate() {
        let mip = &layout.mips[i % mips as usize];
        let f = footprint.Footprint;
        let (element, block) = crate::resource::element(f.Format.0 as u32)?;
        if element != u64::from(layout.desc.element_bytes)
            || block != u64::from(layout.desc.block)
            || f.Width.div_ceil(block as u32) != mip.width
            || f.Height.div_ceil(block as u32) != mip.height
            || f.Depth != mip.depth
            || rows != mip.height
            || u64::from(f.RowPitch) < u64::from(mip.width).checked_mul(element)?
        {
            return None;
        }
        let pitched = u64::from(f.RowPitch)
            .checked_mul(u64::from(rows))?
            .checked_mul(u64::from(f.Depth))?;
        let end = usize::try_from(footprint.Offset.checked_add(pitched)?).ok()?;
        if end > isize::MAX as usize {
            return None;
        }
        total = total.max(end);
    }
    Some(total)
}

/// Once the list has run, tile its texels back into the same title allocation.
pub(crate) fn land(back: &Back, buffer_writes: &[(usize, usize)]) {
    static COPIED: AtomicU64 = AtomicU64::new(0);
    let mut data = std::ptr::null_mut();
    let range = D3D12_RANGE {
        Begin: 0,
        End: back.bytes,
    };
    // SAFETY: the submission completed before this callback; bytes is the checked buffer size.
    unsafe {
        if back.buffer.Map(0, Some(&range), Some(&mut data)).is_err() || data.is_null() {
            return;
        }
    }
    // SAFETY: mapped above, sized before allocation and all copy commands.
    let linear = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), back.bytes) };
    let copied = land_bytes(
        &back.layout,
        &back.ticket,
        &back.footprints,
        &back.rows,
        back.mips,
        linear,
        buffer_writes,
    );
    // SAFETY: this map read only; the list retains the original persistent mapping.
    unsafe {
        back.buffer
            .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }))
    };
    if copied {
        let copied = COPIED.fetch_add(1, Ordering::Relaxed) + 1;
        if copied == 1 || copied.is_multiple_of(128) {
            let (at, end) = back.range();
            tracing::info!(copied, bytes = end - at, "texture readback landed");
        }
    }
}

/// CPU half of a completed readback, shared with the synthetic data tests.
fn land_bytes(
    layout: &victory_formats::texture::Layout,
    ticket: &Ticket,
    footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
    rows: &[u32],
    mips: u32,
    linear: &[u8],
    buffer_writes: &[(usize, usize)],
) -> bool {
    static STALE: AtomicU64 = AtomicU64::new(0);
    let _mapping = crate::watch::title_memory_reading();
    let _ticket_guard = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    if !ticket.valid.load(Ordering::Acquire) {
        let skipped = STALE.fetch_add(1, Ordering::Relaxed) + 1;
        if skipped.is_power_of_two() {
            tracing::info!(
                skipped,
                "skipping texture readbacks whose title allocation changed"
            );
        }
        return false;
    }
    let (at, end) = (ticket.at, ticket.end);
    let size = end - at;
    // Keep both the alias query and the write under the same reading gate.
    if crate::watch::overlaps_buffer_writes(at, size, buffer_writes) {
        static OVERLAPS: AtomicU64 = AtomicU64::new(0);
        let skipped = OVERLAPS.fetch_add(1, Ordering::Relaxed) + 1;
        if skipped.is_power_of_two() {
            tracing::info!(
                skipped,
                "texture readback overlaps buffer writes or their aliases: withheld"
            );
        }
        return false;
    }
    if size < layout.size as usize
        || footprints_size(layout, footprints, rows, mips).is_none_or(|n| n > linear.len())
    {
        return false;
    }
    let mut tiled = vec![0u8; size];
    // Keep title bytes the subresources do not cover.
    if !crate::host::read_into(at as *const u8, &mut tiled) {
        return false;
    }
    for (i, (footprint, &rows)) in footprints.iter().zip(rows).enumerate() {
        let (mip, slice) = (i as u32 % mips, i as u32 / mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let len = pitch * rows as usize * f.Depth as usize;
        // Bounds and products were checked for all subresources before copying any bytes.
        let at = footprint.Offset as usize;
        let linear = &linear[at..at + len];
        if let Err(e) = layout.tile(&mut tiled, mip, slice, linear, pitch, pitch * rows as usize) {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(?e, mip, slice, "a texture copied back could not be tiled")
            });
            return false;
        }
    }
    crate::host::copy_mapped(at as *mut u8, tiled.as_ptr(), size, true);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_FLOAT;

    fn track(owner: usize, at: usize, end: usize) -> Arc<Ticket> {
        TRACKING.store(true, Ordering::Release);
        super::track(
            &mut TICKETS.lock().unwrap_or_else(|e| e.into_inner()),
            owner,
            at,
            end,
        )
    }

    #[test]
    fn texture_readback_is_opt_in() {
        assert!(parse_kinds(None).is_empty());
        assert!(parse_kinds(Some("0")).is_empty());
        assert_eq!(
            parse_kinds(Some("1")),
            [(256, 256, 41), (256, 256, 16), (64, 64, 16), (128, 128, 36)]
        );
        assert_eq!(parse_kinds(Some("13x7f41,invalid")), [(13, 7, 41)]);
        fn send<T: Send>() {}
        send::<Back>();
    }

    struct Pool(usize);

    impl Pool {
        fn new() -> Self {
            Self(crate::watch::tests::pool(2))
        }

        fn fill(&self, value: u8) {
            let _writing = crate::watch::writing(self.0, 2 * crate::watch::PAGE);
            // SAFETY: two committed pages owned by this test.
            unsafe { (self.0 as *mut u8).write_bytes(value, 2 * crate::watch::PAGE) };
        }

        fn bytes(&self) -> Vec<u8> {
            // SAFETY: this test owns the allocation and reads after its writes finish.
            unsafe {
                std::slice::from_raw_parts(self.0 as *const u8, 2 * crate::watch::PAGE).to_vec()
            }
        }
    }

    impl Drop for Pool {
        fn drop(&mut self) {
            let _mapping = crate::watch::title_memory_remapping();
            crate::watch::title_memory_remapped(self.0, 2 * crate::watch::PAGE);
            crate::watch::tests::free(self.0);
        }
    }

    struct Views(usize, usize);

    impl Views {
        fn new() -> Self {
            let (a, b) = crate::watch::tests::twin_views(2);
            let _writing = crate::watch::writing(a, 2 * crate::watch::PAGE);
            // SAFETY: two committed pages of our section, seen through both mappings.
            unsafe { (a as *mut u8).write_bytes(0xa5, 2 * crate::watch::PAGE) };
            Self(a, b)
        }

        fn bytes(&self) -> Vec<u8> {
            // SAFETY: the fixture keeps both views mapped until it is dropped.
            unsafe {
                std::slice::from_raw_parts(self.0 as *const u8, 2 * crate::watch::PAGE).to_vec()
            }
        }

        fn write_alias(&self) {
            let _writing = crate::watch::writing(self.1 + 32, 4);
            // SAFETY: a word in our second mapping; this changes the first mapping too.
            unsafe { ((self.1 + 32) as *mut u32).write_unaligned(0x1122_3344) };
        }
    }

    impl Drop for Views {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            let _mapping = crate::watch::title_memory_remapping();
            for at in [self.0, self.1] {
                crate::watch::title_memory_remapped(at, 2 * crate::watch::PAGE);
                // SAFETY: no reads of these test views remain.
                unsafe {
                    UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: at as *mut _,
                    })
                };
            }
        }
    }

    fn layout() -> Layout {
        Layout::new(&Desc {
            dimension: Dimension::Tex2d,
            width: 13,
            height: 7,
            depth: 2,
            mips: 3,
            element_bytes: 4,
            block: 1,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        })
        .unwrap()
    }

    /// Synthetic pitched data for three mips in two array slices; padding differs from texels.
    fn pixels(
        layout: &Layout,
    ) -> (
        Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
        Vec<u32>,
        Vec<u8>,
        Vec<u8>,
    ) {
        let mut footprints = Vec::new();
        let mut rows = Vec::new();
        let mut linear = Vec::new();
        let mut expected = vec![0xa5; layout.size as usize];
        for slice in 0..layout.slices {
            for (mip, shape) in layout.mips.iter().enumerate() {
                let offset = linear.len().div_ceil(512) * 512;
                let pitch = 256usize;
                linear.resize(offset + pitch * shape.height as usize, 0xcd);
                footprints.push(D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: offset as u64,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT_R32_FLOAT,
                        Width: shape.width,
                        Height: shape.height,
                        Depth: 1,
                        RowPitch: pitch as u32,
                    },
                });
                rows.push(shape.height);
                for y in 0..shape.height {
                    for x in 0..shape.width {
                        let value = (1 + slice * 10_000 + mip as u32 * 1_000 + y * 20 + x) as f32;
                        let bytes = value.to_le_bytes();
                        let at = offset + y as usize * pitch + x as usize * 4;
                        linear[at..at + 4].copy_from_slice(&bytes);
                        let tiled = layout.offset(mip as u32, x, y, slice).unwrap() as usize;
                        expected[tiled..tiled + 4].copy_from_slice(&bytes);
                    }
                }
            }
        }
        (footprints, rows, linear, expected)
    }

    #[test]
    fn completed_readback_tiles_every_mip_and_slice_and_preserves_other_bytes() {
        let layout = layout();
        let (footprints, rows, linear, expected) = pixels(&layout);
        let pool = Pool::new();
        pool.fill(0xa5);
        let at = pool.0 + 32;
        let end = at + layout.size as usize;
        assert!(end + 32 < pool.0 + 2 * crate::watch::PAGE);
        let ticket = track(at, at, end);
        let tracked = crate::watch::track(pool.0, crate::watch::PAGE, true).unwrap();
        tracked.take(pool.0, pool.0 + crate::watch::PAGE);
        assert!(tracked.clean());
        assert!(land_bytes(
            &layout,
            &ticket,
            &footprints,
            &rows,
            3,
            &linear,
            &[]
        ));
        let actual = pool.bytes();
        assert_eq!(&actual[32..32 + expected.len()], expected);
        assert!(
            actual[..32]
                .iter()
                .chain(&actual[32 + expected.len()..])
                .all(|&v| v == 0xa5)
        );
        assert!(
            !tracked.clean(),
            "CPU readback must notify other copies of these pages"
        );
    }

    #[test]
    fn buffer_writes_through_another_view_withhold_texture_readback() {
        let layout = layout();
        let (footprints, rows, linear, expected) = pixels(&layout);
        let views = Views::new();
        let at = views.0 + 32;
        assert!(32 + (layout.size as usize) < crate::watch::PAGE);
        let ticket = track(at, at, at + layout.size as usize);
        views.write_alias();
        let newer = views.bytes();
        assert_eq!(&newer[32..36], &0x1122_3344u32.to_le_bytes());
        assert!(!land_bytes(
            &layout,
            &ticket,
            &footprints,
            &rows,
            3,
            &linear,
            &[(views.1 + 32, 4)]
        ));
        assert_eq!(
            views.bytes(),
            newer,
            "an older texture must not overwrite the aliased buffer"
        );
        assert!(
            ticket.valid.load(Ordering::Acquire),
            "overlap only withholds this submission"
        );
        // Reset the first case's padding, then really write through the
        // second physical page's alias for the nonoverlapping control.
        {
            let _writing = crate::watch::writing(at, layout.size as usize);
            // SAFETY: the first view's live texture allocation.
            unsafe { (at as *mut u8).write_bytes(0xa5, layout.size as usize) };
        }
        let outside = views.1 + crate::watch::PAGE;
        {
            let _writing = crate::watch::writing(outside, 4);
            // SAFETY: a word in the second page of our alias mapping.
            unsafe { (outside as *mut u32).write_unaligned(0x5566_7788) };
        }
        assert!(land_bytes(
            &layout,
            &ticket,
            &footprints,
            &rows,
            3,
            &linear,
            &[(outside, 4)]
        ));
        let actual = views.bytes();
        assert_eq!(&actual[32..32 + expected.len()], expected);
        assert_eq!(
            &actual[crate::watch::PAGE..crate::watch::PAGE + 4],
            &0x5566_7788u32.to_le_bytes(),
            "the nonoverlapping buffer write survives"
        );
    }

    #[test]
    fn remapping_a_buffer_alias_invalidates_tickets_before_the_link_is_removed() {
        let layout = layout();
        let (footprints, rows, linear, _) = pixels(&layout);
        let views = Views::new();
        let at = views.0 + 32;
        let ticket = track(at, at, at + layout.size as usize);
        views.write_alias();
        let newer = views.bytes();
        {
            let _mapping = crate::watch::title_memory_remapping();
            // Any remap in this physical page forgets its alias links. The
            // host unmap follows notification; fixture cleanup performs it.
            crate::watch::title_memory_remapped(views.1 + crate::watch::PAGE - 1, 1);
        }
        assert!(!ticket.valid.load(Ordering::Acquire));
        {
            let _mapping = crate::watch::title_memory_reading();
            assert!(
                !crate::watch::overlaps_buffer_writes(
                    at,
                    layout.size as usize,
                    &[(views.1 + 32, 4)]
                ),
                "the historical relationship is no longer queryable"
            );
        }
        assert!(!land_bytes(
            &layout,
            &ticket,
            &footprints,
            &rows,
            3,
            &linear,
            &[(views.1 + 32, 4)]
        ));
        assert_eq!(
            views.bytes(),
            newer,
            "invalidation preserves the newer physical-page data"
        );
    }

    #[test]
    fn stale_readbacks_never_overwrite_reused_memory_but_new_tickets_do() {
        let layout = layout();
        let (footprints, rows, linear, expected) = pixels(&layout);
        let pool = Pool::new();
        let at = pool.0;
        for how in 0..3 {
            pool.fill(0xa5);
            let ticket = track(at, at, at + layout.size as usize);
            let untouched = track(
                at + crate::watch::PAGE,
                at + crate::watch::PAGE,
                at + 2 * crate::watch::PAGE,
            );
            match how {
                0 => {
                    let _mapping = crate::watch::title_memory_remapping();
                    crate::watch::title_memory_remapped(at + 1, 1);
                }
                1 => replaced(at),
                _ => released(at),
            }
            assert!(
                untouched.valid.load(Ordering::Acquire),
                "unrelated allocations stay live"
            );
            assert!(!land_bytes(
                &layout,
                &ticket,
                &footprints,
                &rows,
                3,
                &linear,
                &[],
            ));
            assert!(pool.bytes().iter().all(|&v| v == 0xa5));
            let fresh = track(at, at, at + layout.size as usize);
            assert!(land_bytes(
                &layout,
                &fresh,
                &footprints,
                &rows,
                3,
                &linear,
                &[]
            ));
            assert_eq!(&pool.bytes()[..expected.len()], expected);
        }
    }

    #[test]
    fn replacement_during_host_copy_acquisition_invalidates_the_registered_ticket() {
        use crate::object::{guid, slot};
        let pool = Pool::new();
        let make = || {
            // Synthetic 13x7 R32_FLOAT, two slices, three mips, 4 KiB swizzle.
            let words: [u32; 14] = [3, 0, 0, 0, 13, 0, 7, 3 << 16 | 2, 41, 1, 0, 0x105, 4, 0];
            // SAFETY: ResourceDesc is a 56-byte C structure of integer fields.
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let iid = guid(
                0x696442be,
                0xa72e,
                0x4059,
                [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
            );
            let mut resource = std::ptr::null();
            assert_eq!(
                crate::resource::create_at_address(
                    std::ptr::null(),
                    pool.0 as u64,
                    &desc,
                    0,
                    std::ptr::null(),
                    &iid,
                    &mut resource,
                ),
                crate::object::S_OK
            );
            resource
        };
        let first = make();
        let (layout, old) = Target::allocation(first).unwrap();
        assert_eq!(layout, self::layout());
        // The allocation is replaced after registration but before host_copy.
        let next = make();
        assert!(!old.valid.load(Ordering::Acquire));
        let (_, fresh) = Target::allocation(next).unwrap();
        assert!(fresh.valid.load(Ordering::Acquire));
        type Release = extern "system" fn(This) -> u32;
        assert_eq!(slot::<Release>(first, 0x10)(first), 0);
        assert!(
            fresh.valid.load(Ordering::Acquire),
            "release only invalidates its own resource"
        );
        assert_eq!(slot::<Release>(next, 0x10)(next), 0);
        assert!(
            !fresh.valid.load(Ordering::Acquire),
            "actual resource release invalidates pending readbacks"
        );
    }

    #[test]
    fn gpu_texture_readback_lands_real_footprints_before_completion() {
        gpu_texture_readback(false);
    }

    #[test]
    fn gpu_texture_readback_never_overwrites_a_later_overlapping_buffer_write() {
        gpu_texture_readback(true);
    }

    fn gpu_texture_readback(overlapping_buffer: bool) {
        use crate::object::{guid, slot};
        let host = crate::host::host().expect("GPU required for texture readback regression");
        let pool = Pool::new();
        pool.fill(0xa5);
        // Synthetic R32_FLOAT texture, two slices and three mips, in tiled title memory.
        let words: [u32; 14] = [3, 0, 0, 0, 13, 0, 7, 3 << 16 | 2, 41, 1, 0, 0x105, 4, 0];
        // SAFETY: ResourceDesc is a 56-byte C structure of integer fields.
        let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
        let iid = guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let mut resource = std::ptr::null();
        assert_eq!(
            crate::resource::create_at_address(
                std::ptr::null(),
                pool.0 as u64,
                &desc,
                0,
                std::ptr::null(),
                &iid,
                &mut resource,
            ),
            crate::object::S_OK
        );
        let target = Target::new(resource).expect("live title allocation and GPU texture");
        let (footprints, _, linear, mut expected) = pixels(&target.layout);
        let list = host.new_list().unwrap();
        let later = host.new_list().unwrap();
        let shadow = overlapping_buffer.then(|| {
            crate::shadow::register(pool.0, pool.0 as u64, crate::watch::PAGE as u64);
            let copy = crate::shadow::find(pool.0 as u64, crate::watch::PAGE as u64).unwrap();
            let address = pool.0 as u64;
            copy.refresh(&later, address, 4);
            copy.gpu_writes(&later, address, 4, false);
            let written = copy.clone();
            // Simulate the later shader's result after its snapshot, as in host's withholding tests.
            later.defer_host(move || written.write_host(address, &0x1122_3344u32.to_le_bytes()));
            expected.fill(0xa5);
            expected[..4].copy_from_slice(&0x1122_3344u32.to_le_bytes());
            copy
        });
        let (upload, mapped) = list.upload(host, linear.len() as u64).unwrap();
        // SAFETY: the list owns a mapped upload buffer of exactly this size.
        unsafe {
            std::ptr::copy_nonoverlapping(linear.as_ptr(), mapped.cast::<u8>(), linear.len())
        };
        let barriers = list.track_state(
            &target.copy,
            crate::host::subresource::ALL,
            D3D12_RESOURCE_STATE_COPY_DEST.0 as u32,
        );
        // SAFETY: first use is handled by the submission prelude; subsequent barriers are tracked.
        unsafe {
            if !barriers.is_empty() {
                list.list.ResourceBarrier(&barriers);
            }
        }
        for (index, footprint) in footprints.iter().enumerate() {
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: borrowed COM pointer, retained in the list's uploads.
                pResource: unsafe { std::mem::transmute_copy(&upload) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: *footprint,
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: the list's state tracking keeps the texture alive.
                pResource: unsafe { std::mem::transmute_copy(&target.copy.texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: index as u32,
                },
            };
            // SAFETY: live resources, all six destination subresources are in COPY_DEST.
            unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
        let barriers = list.track_state(
            &target.copy,
            crate::host::subresource::ALL,
            D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32,
        );
        // SAFETY: transition the actual uploaded subresources before the production readback.
        unsafe {
            if !barriers.is_empty() {
                list.list.ResourceBarrier(&barriers);
            }
        }
        let back = record(&list, target).expect("real device footprints and readback buffer");
        assert_eq!(back.footprints.len(), 6);
        assert_eq!(back.mips, 3);
        list.defer_texture_back(back);
        let mut completion = Box::new(0u64);
        let completion_at = (&raw mut *completion) as usize;
        list.defer_write(completion_at as *mut u8, crate::queue::Written::Value(1), 8);
        let finished = Arc::new(AtomicBool::new(false));
        let callback_finished = finished.clone();
        let title_at = pool.0;
        let callback_expected = expected.clone();
        list.defer_after(move || {
            // SAFETY: the title allocation and completion marker outlive synchronous execute.
            unsafe {
                assert_eq!(
                    std::slice::from_raw_parts(title_at as *const u8, callback_expected.len()),
                    callback_expected.as_slice(),
                    "texture readbacks land only when no buffer write overlaps"
                );
                assert_eq!(
                    (completion_at as *const u64).read_volatile(),
                    0,
                    "title completion must follow texture readback"
                );
            }
            callback_finished.store(true, Ordering::Release);
        });
        list.close();
        later.close();
        host.execute(&[&list, &later]);
        assert!(finished.load(Ordering::Acquire));
        assert_eq!(*completion, 1);
        let actual = pool.bytes();
        assert_eq!(&actual[..expected.len()], expected);
        assert!(actual[expected.len()..].iter().all(|&v| v == 0xa5));
        type Release = extern "system" fn(This) -> u32;
        assert_eq!(slot::<Release>(resource, 0x10)(resource), 0);
        if shadow.is_some() {
            crate::shadow::unregister(pool.0, pool.0 as u64);
        }
    }

    #[test]
    fn invalid_or_truncated_footprints_leave_all_title_bytes_untouched() {
        let layout = layout();
        let (footprints, rows, linear, _) = pixels(&layout);
        let pool = Pool::new();
        pool.fill(0xa5);
        let ticket = track(pool.0, pool.0, pool.0 + layout.size as usize);
        let required = footprints_size(&layout, &footprints, &rows, 3).unwrap();
        assert_eq!(
            required,
            linear.len(),
            "include last-row padding in the allocation"
        );
        assert!(!land_bytes(
            &layout,
            &ticket,
            &footprints,
            &rows,
            3,
            &linear[..required - 1],
            &[],
        ));
        assert!(!land_bytes(
            &layout,
            &ticket,
            &footprints[..footprints.len() - 1],
            &rows,
            3,
            &linear,
            &[],
        ));
        for bad in 0..4 {
            let mut wrong = footprints.clone();
            let last = wrong.last_mut().unwrap();
            match bad {
                0 => last.Offset = u64::MAX - 1,
                1 => last.Footprint.RowPitch = 1,
                2 => last.Footprint.Depth = 2,
                _ => last.Footprint.Width += 1,
            }
            assert!(!land_bytes(
                &layout,
                &ticket,
                &wrong,
                &rows,
                3,
                &linear,
                &[]
            ));
            assert!(
                pool.bytes().iter().all(|&v| v == 0xa5),
                "do not land even the valid first mip"
            );
        }
    }

    #[test]
    fn only_readbacks_for_changed_allocations_are_invalidated() {
        use crate::watch::PAGE;
        let first = track(91, PAGE, 2 * PAGE);
        let next = track(92, 2 * PAGE, 3 * PAGE);
        let far = track(93, 4 * PAGE, 5 * PAGE);

        remapped(PAGE - 0x100, 0x200);
        assert!(!first.valid.load(Ordering::Acquire));
        assert!(next.valid.load(Ordering::Acquire));
        remapped(2 * PAGE, 0);
        assert!(
            next.valid.load(Ordering::Acquire),
            "empty remap at the boundary changes nothing"
        );
        replaced(2 * PAGE);
        assert!(!next.valid.load(Ordering::Acquire));
        assert!(far.valid.load(Ordering::Acquire));

        released(93);
        assert!(!far.valid.load(Ordering::Acquire));
        assert!(track(94, 2 * PAGE, 3 * PAGE).valid.load(Ordering::Acquire));
    }
}
