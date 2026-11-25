//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;

#[cfg(feature = "diagnostics")]
thread_local! {
    static TRACE_VIEWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "diagnostics")]
pub(crate) struct TraceViews(bool);

#[cfg(feature = "diagnostics")]
impl Drop for TraceViews {
    fn drop(&mut self) {
        TRACE_VIEWS.set(self.0);
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn trace_views(enabled: bool) -> TraceViews {
    TraceViews(TRACE_VIEWS.replace(enabled))
}

#[cfg(feature = "diagnostics")]
pub(crate) fn tracing_views() -> bool {
    TRACE_VIEWS.get()
}

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count`: [`DYNAMIC`] in view heaps, none in
    /// sampler heaps. (Upstream falls back to 2^16 on a capped host heap;
    /// ours keeps 2^18 there: 2^16 and 2^17 both ran out in this title.)
    dynamic: u32,
    sampler: bool,
    /// The record (view index and generation) each slot was last made from.
    made: Mutex<Vec<u64>>,
    /// Fingerprints of records synchronized by the copied-record pass;
    /// 0 means invalid. Mutations invalidate and publish under `made`,
    /// taking `copied` second. Page scans release `copied` before mirroring.
    copied: Mutex<Vec<u64>>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    /// With it the view's own copy of title memory, if it has one.
    kept: Mutex<Vec<Kept>>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
    /// How many dynamic slots are free (changed under `next`'s lock): with
    /// fewer than a table needs, the search is not made. Once the world
    /// had taken them all, every later table of the frame searched every
    /// slot before it was refused.
    free: std::sync::atomic::AtomicU32,
    /// The shortest table that found no run of free slots since slots were
    /// last given back (changed under `next`'s lock; `u32::MAX`: none).
    /// Until something is given back the free runs only shrink, so a table
    /// of that length or longer is refused without the search. Enough
    /// slots were free, but in pieces, and every such table walked the
    /// whole ring before it was refused: 0.33 ms each with 131,072 slots,
    /// 1.0 s of the recording threads in a frame that refused 3,143 (run
    /// 45), under the lock all of them need. (A free run that lies across
    /// the place the search starts from is seen by neither.)
    too_long: std::sync::atomic::AtomicU32,
    /// View heaps: the bytes per element of the buffer view at each title
    /// slot, tagged with the descriptor kind, for shaders that pick a
    /// buffer by an index they compute
    /// (`victory_shader::emit::SLOT_STRIDES`): an upload buffer of
    /// `SLOT_STRIDES_COUNT` dwords, its GPU address and its mapping.
    strides: Option<(ID3D12Resource, u64, usize)>,
    /// The title memory each placed texture view of this heap describes,
    /// by slot and by address ([`Spans`]): which slots a newer view of
    /// other memory there makes stale. Uploads use `bindless::Viewed`.
    placed: Mutex<Spans>,
    /// Slot pairs kept for host copies' views (`crate::memory`), past the
    /// dynamic slots and below the null buffer views: the next never
    /// taken, those given back, and where they end.
    pairs: Mutex<(u32, Vec<u32>)>,
    pairs_end: u32,
    /// View heaps: the view-stride table translated shaders read
    /// (`victory_shader::emit::VIEW_STRIDES`), a dword per title slot: the
    /// byte stride of the buffer view there, 0 for none. Mapped at the
    /// address kept beside it.
    view_strides: Option<(ID3D12Resource, usize)>,
    /// View heaps: the view-mark table translated shaders read
    /// (`victory_shader::emit`, "Write marks"), two dwords per slot as the
    /// stride table has one: the mark bit and phase of the unordered buffer
    /// view there, `VIEW_MARK_NONE` for none. Mapped at the address kept
    /// beside it.
    marks: Option<(ID3D12Resource, usize)>,
    /// Per title slot, the console descriptor's dwords 0-3 as far as the
    /// host knows them (shaders reading a descriptor as data, `crate::memory`;
    /// [`write_address`]): the buffer, its mapping and host address, made on
    /// first use.
    addresses: std::sync::OnceLock<Option<(ID3D12Resource, usize, u64)>>,
    /// What the title slots view that title memory feeds (`crate::bindless`).
    pub(crate) viewed: std::sync::Arc<crate::bindless::Viewed>,
}

/// Title memory by the placed texture views that describe it: per span of
/// memory (its start, its end at full size) the title slots holding a
/// view of a texture that takes exactly that span. A console title need
/// not clear a descriptor when it streams a texture out, so a newer view
/// that cannot be there together with an older one is all that says the
/// older one is stale ([`Rule`] says when that is). Views of one span with
/// different descriptions (the formats of one family, say) are one
/// texture's.
///
/// Spans may overlap: a texture's smallest levels lie at the start of its
/// memory, and a title that holds only those puts the next texture where
/// the larger ones would be (the title: three 512x256 textures
/// of 0x2c000 bytes each whose starts are 0x2000 and 0x4000 apart, all
/// three read by the same frame's draws, run 188). What is there of a
/// texture ends where the next one starts.
/// No host heap is needed, so it is tested without one.
#[derive(Default)]
pub(crate) struct Spans {
    /// When a newer view makes an older one stale.
    rule: Rule,
    by_start: std::collections::BTreeMap<u64, (u64, std::collections::BTreeSet<u32>)>,
    by_slot: std::collections::HashMap<u32, u64>,
    /// The bytes a placed texture takes, by resource description: worked
    /// out once for each (a title writes the views of a few thousand
    /// descriptions, thousands of times a frame).
    sizes: std::collections::HashMap<[u32; 14], Option<u64>>,
}

/// When a view the title writes makes an older view of other memory stale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Rule {
    /// When it starts where the older one starts: at the same address
    /// with another length, or within the older one's first [`HEAD`]
    /// bytes (or the older within its own). No two textures share the
    /// block of their smallest levels. A view that only reaches into the
    /// address range of an older texture's larger levels says nothing.
    #[default]
    Start,
    /// Whenever their spans at full size overlap (the rule until run
    /// 188, where it made null the slots of textures packed as above:
    /// each view written invalidated its neighbours for good). `VICTORY_STALE_TEXTURES=overlap`.
    Overlap,
    /// Never; a view that would be stale under [`Rule::Overlap`] is left
    /// out of the spans instead. `VICTORY_STALE_TEXTURES=0`, as before.
    Never,
}

/// The first bytes of a placed texture that are its own under
/// [`Rule::Start`]: the 4 KiB block its smallest levels share. (Of 129
/// pairs of overlapping neighbours in the listings of runs 188 and 189,
/// none starts closer than 0x2000 to the one before it.)
const HEAD: u64 = 4096;

impl Spans {
    /// The bytes of title memory a placed texture with the resource
    /// description `desc` takes (`crate::resource::placed_size`).
    fn bytes(&mut self, desc: &[u32; 14]) -> Option<u64> {
        if let Some(&known) = self.sizes.get(desc) {
            return known;
        }
        let bytes = crate::resource::placed_size(desc);
        self.sizes.insert(*desc, bytes);
        bytes
    }

    /// The starts of the spans that overlap `[start, end)`, each taken to
    /// end where the next one starts: the last span that starts before
    /// `start`, if it reaches past it at full size, and every span that
    /// starts inside.
    fn overlapping(&self, start: u64, end: u64) -> Vec<u64> {
        let before = self
            .by_start
            .range(..start)
            .next_back()
            .filter(|(_, span)| span.0 > start)
            .map(|(at, _)| *at);
        before.into_iter().chain(self.by_start.range(start..end).map(|(at, _)| *at)).collect()
    }

    /// Slot `index` holds a view of a texture in `[start, end)`. `newer`:
    /// the title wrote it now, so the slots whose views cannot be there
    /// with it ([`Rule`]) are stale: they are returned, and forgotten.
    /// Otherwise (a draw made the slot from what it held), and under
    /// [`Rule::Never`], a view that clashes with other spans is left out.
    fn note(&mut self, index: u32, start: u64, end: u64, newer: bool) -> Vec<u32> {
        let same = |span: Option<&(u64, std::collections::BTreeSet<u32>)>| span.is_some_and(|s| s.0 == end);
        if self.by_slot.get(&index) == Some(&start) && same(self.by_start.get(&start)) {
            return Vec::new();
        }
        self.forget(index);
        if end <= start {
            return Vec::new();
        }
        let rule = self.rule;
        // The spans the view cannot be there with.
        let clashing: Vec<u64> = match rule {
            Rule::Start => self
                .by_start
                .range(start.saturating_sub(HEAD - 1)..start.saturating_add(HEAD))
                .map(|(at, _)| *at)
                .collect(),
            Rule::Overlap | Rule::Never => self.overlapping(start, end),
        };
        let mut stale = Vec::new();
        for at in clashing {
            if at == start && same(self.by_start.get(&at)) {
                continue;
            }
            if !newer || rule == Rule::Never {
                return stale;
            }
            if let Some((old_end, slots)) = self.by_start.remove(&at) {
                // The first of them are said with both spans, to tell a
                // replacement from a neighbour in one launch's log.
                static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 48 {
                    tracing::info!(
                        slot = index,
                        start = format_args!("{start:#x}"),
                        end = format_args!("{end:#x}"),
                        stale_start = format_args!("{at:#x}"),
                        stale_end = format_args!("{old_end:#x}"),
                        stale_slots = slots.len(),
                        rule = ?rule,
                        "a placed texture view makes older views of other memory stale"
                    );
                }
                for slot in slots {
                    self.by_slot.remove(&slot);
                    stale.push(slot);
                }
            }
        }
        self.by_start.entry(start).or_insert_with(|| (end, Default::default())).1.insert(index);
        self.by_slot.insert(index, start);
        stale
    }

    /// Slot `index` holds no placed texture view any more.
    fn forget(&mut self, index: u32) {
        let Some(start) = self.by_slot.remove(&index) else {
            return;
        };
        if let Some(span) = self.by_start.get_mut(&start) {
            span.1.remove(&index);
            if span.1.is_empty() {
                self.by_start.remove(&start);
            }
        }
    }

}

/// When a view the title writes makes older views of other memory stale
/// ([`Rule`]): where it starts in the block of their smallest levels, by
/// default; `VICTORY_STALE_TEXTURES=0`: never, as before;
/// `VICTORY_STALE_TEXTURES=overlap`: wherever their spans overlap, the rule
/// until run 188.
fn stale_rule() -> Rule {
    static RULE: std::sync::OnceLock<Rule> = std::sync::OnceLock::new();
    *RULE.get_or_init(|| match std::env::var("VICTORY_STALE_TEXTURES").as_deref() {
        Ok("0") => Rule::Never,
        Ok("overlap") => Rule::Overlap,
        _ => Rule::Start,
    })
}

/// Bytes per element of a buffer view, as the console's descriptor holds
/// them for indexed reads (structured: the stride; raw: 1, the index is the
/// byte; typed: the format's); `None` if `view` is not a buffer view.
fn legacy_view_stride(view: &View) -> Option<u32> {
    const BUFFER: u32 = 1;
    let (words, unordered): (&[u32; 10], bool) = match view {
        View::Shader(_, Some(d)) if d[1] == BUFFER => (d, false),
        View::Unordered(_, _, Some(d)) if d[1] == BUFFER => (d, true),
        View::Placed(unordered, resource, d) if resource[0] == BUFFER && d[1] == BUFFER => {
            (d[..10].try_into().ok()?, *unordered)
        }
        _ => return None,
    };
    // SAFETY: buffer views' union members.
    let (stride, raw, format) = unsafe {
        if unordered {
            let d = uav_desc(words);
            let b = d.Anonymous.Buffer;
            (b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0, d.Format)
        } else {
            let d = srv_desc(words);
            let b = d.Anonymous.Buffer;
            (b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0, d.Format)
        }
    };
    Some(if stride > 0 {
        stride
    } else if raw {
        1
    } else {
        crate::resource::element(format.0 as u32).map_or(0, |(bytes, _)| bytes as u32)
    })
}

/// Update the shader's stride/kind entry even when a slot stops being a buffer.
/// Returns the untagged stride for descriptor classification checks.
///
/// # Safety
/// If present and `index` is inside `SLOT_STRIDES_COUNT`, `data` must point
/// to a live, writable mapping with at least `index + 1` dwords.
unsafe fn note_slot_stride(data: Option<usize>, index: u32, view: Option<&View>) -> Option<u32> {
    use victory_shader::emit::{SLOT_STRIDE_CBV, SLOT_STRIDE_UAV, SLOT_STRIDES_COUNT};
    let stride = view.and_then(legacy_view_stride);
    // Reserved kind bits cannot come from a malformed structure stride.
    let bytes = stride.filter(|&s| s < SLOT_STRIDE_UAV - 1).unwrap_or(0);
    let entry = match view {
        Some(View::Constant(..)) => SLOT_STRIDE_CBV,
        Some(View::Unordered(..) | View::Placed(true, ..)) if bytes > 0 => {
            bytes | SLOT_STRIDE_UAV
        }
        _ => bytes,
    };
    if let Some(data) = data
        && index < SLOT_STRIDES_COUNT
    {
        // SAFETY: the caller supplies the live mapping. Synchronization with
        // in-flight shaders is the existing stride table's separate concern.
        unsafe { (data as *mut u32).add(index as usize).write_volatile(entry) };
    }
    stride
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!("{:#x} past its end (slot {})", va - start - bytes, (va - start) / step)
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// The size of every host view heap on a host that takes more than
/// [`PORTABLE_VIEWS`] (AMD): the title's heap (a little over 1,000,000
/// views in the retail game) plus [`DYNAMIC`] fits with room to spare, and
/// the last slot is the null slot every view index at or past it reads.
const UNCAPPED_VIEWS: u32 = 1 << 21;

/// Picks, once, the null slot at the end of every host view heap and tells
/// the translator now (`set_view_heap_limit`) instead of when the title's
/// heap is made: a shader keeps the limit it was translated with, and the
/// title makes 25 pipelines (their shaders translated at once) before it
/// makes its view heap. Called before a shader is translated and before a
/// view heap is made.
///
/// Every host clamps. A host that caps view heaps at [`PORTABLE_VIEWS`]
/// (NVIDIA) hung on a view index past the heap; one that does not (AMD
/// RDNA3, RX 7900 XT) took a page fault and removed the device on entering
/// the world: a compute program read view slot 16,777,088 (0xffff80) of a
/// heap of 1,507,328, past its end. Unclamped, that index is undefined on
/// any host.
pub(crate) fn probe_view_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(host) = host() else { return };
        let fits = |count: u32| {
            // SAFETY: plain heap creation; the heap is released at once.
            let made: windows::core::Result<ID3D12DescriptorHeap> = unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            };
            made.is_ok()
        };
        let views = if fits(UNCAPPED_VIEWS) {
            UNCAPPED_VIEWS
        } else {
            PORTABLE_VIEWS
        };
        tracing::info!(
            views,
            "host view heaps hold {views} views; view indices are clamped to the null slot at the end from the first shader on"
        );
        victory_shader::emit::set_view_heap_limit(views - 1);
    });
}

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out in the world ("no free dynamic descriptor slots", run 11;
/// upstream PR #336 takes 2^18). On a host heap capped at 1,000,000 views
/// (NVIDIA) these come out of the title's slots, whose highest seen is
/// about 720,000: 2^17 left 868,000 of them, and ran out in the opening
/// cutscene (a frame asks for up to 16,000 tables while the frame before
/// still holds its own; a draw refused its table read slots never made
/// for it, and the card stopped within frames). 2^18 leaves 737,000; a
/// table past them is made afresh in these slots like any other.
pub(crate) const DYNAMIC: u32 = 1 << 18;

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        let extra = if sampler { 0 } else { DYNAMIC };
        // A view heap ends at the null slot shaders clamp view indices to
        // (`probe_view_limit`), whatever the title's own count: the title's
        // slots, then the dynamic ones, then null up to and with that slot.
        if !sampler {
            probe_view_limit();
        }
        let null = if sampler {
            0
        } else {
            victory_shader::emit::view_heap_limit()
        };
        let (heap, count, size) = if null == 0 {
            match make(count.max(victory_shader::emit::HEAP_SAMPLERS) + extra) {
                Ok(heap) => (heap, count, count.max(victory_shader::emit::HEAP_SAMPLERS) + extra),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else {
            let size = null + 1;
            let fitted = if count + extra <= null.saturating_sub(2) {
                count
            } else {
                // shortcut: descriptors past the host's limit are left out; a title that reaches
                // them needs remapping. They read the null slot instead.
                tracing::warn!(count, "host view heap capped at {size}; views past it read a null slot");
                null - 2 - extra
            };
            match make(size) {
                Ok(heap) => (heap, fitted, size),
                Err(e) => {
                    tracing::warn!(error = %e, count, size, "no host view heap");
                    return None;
                }
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let (gpu, total) = unsafe {
            (heap.GetGPUDescriptorHandleForHeapStart().ptr, heap.GetDesc().NumDescriptors)
        };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(size) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let strides = (!sampler)
            .then(|| {
                let bytes = u64::from(victory_shader::emit::SLOT_STRIDES_COUNT) * 4;
                let (buffer, data) = host.upload_buffer(bytes)?;
                // SAFETY: the buffer's whole mapping; a live buffer.
                let gpu = unsafe {
                    std::ptr::write_bytes(data, 0, bytes as usize);
                    buffer.GetGPUVirtualAddress()
                };
                crate::host::name(&buffer, || "element strides by view heap slot".to_owned());
                Some((buffer, gpu, data as usize))
            })
            .flatten();
        let view_strides = (!sampler).then(|| {
            let bytes = u64::from(count.max(victory_shader::emit::HEAP_VIEWS)) * 4;
            let (buffer, data) = host.upload_buffer(bytes)?;
            // SAFETY: the whole upload mapping belongs to this buffer.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            Some((buffer, data as usize))
        }).flatten();
        let marks = (!sampler).then(|| {
            let bytes = u64::from(count.max(victory_shader::emit::HEAP_VIEWS)) * 8;
            let (buffer, data) = host.upload_buffer(bytes)?;
            // SAFETY: every slot starts unmarked in our mapped buffer.
            unsafe { std::ptr::write_bytes(data, 0xff, bytes as usize) };
            Some((buffer, data as usize))
        }).flatten();
        let h = HostHeap {
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made: Mutex::new(vec![0; count as usize]),
            copied: Mutex::new(Vec::new()),
            kept: Mutex::new(vec![None; size as usize]),
            next: Mutex::new((0, vec![false; extra as usize])),
            free: std::sync::atomic::AtomicU32::new(extra),
            too_long: std::sync::atomic::AtomicU32::new(u32::MAX),
            strides,
            placed: Mutex::default(),
            pairs: Mutex::new((count + extra, Vec::new())),
            pairs_end: if sampler { total } else { victory_shader::emit::null_buffer_view() - 1 },
            view_strides,
            marks,
            addresses: std::sync::OnceLock::new(),
            viewed: Default::default(),
        };
        if !sampler {
            crate::bindless::register(&h.viewed);
        }
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, size) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(ms = started.elapsed().as_millis(), "host descriptor heap made null");
        // What shaders read instead of a slot without a buffer view where
        // they take one at a computed index (`victory_shader::emit`, "Buffer
        // views at computed heap indices"): a null raw SRV, and UAV below it.
        let null = victory_shader::emit::null_buffer_view();
        if !sampler && null > count + extra && null < total {
            h.raw_view_at(null, None, 0, 0, false);
            h.raw_view_at(null - 1, None, 0, 0, true);
        } else if !sampler {
            tracing::warn!(count, total, "no room for the null buffer views: computed indices of slots without a buffer view read what is there");
        }
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: BLOCK,
                    ..Default::default()
                })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device.CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device.CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::DynamicAsked, 1);
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let enough = self.free.load(std::sync::atomic::Ordering::Relaxed) >= n
            && n < self.too_long.load(std::sync::atomic::Ordering::Relaxed);
        let n = n as usize;
        // Search from `next` on, then from the start (extending past cursor by
        // n - 1 so contiguous runs crossing the cursor are not missed).
        let cursor = *next as usize;
        let end = self.dynamic as usize;
        let second_end = cursor.saturating_add(n.saturating_sub(1)).min(end);
        let starts = (cursor..end).chain(0..second_end);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts.take(if enough { end + second_end } else { 0 }) {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            if enough {
                self.too_long.fetch_min(n as u32, std::sync::atomic::Ordering::Relaxed);
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(n, "no free dynamic descriptor slots: a table is not made"));
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::DynamicRefused, 1);
                add(C::DynamicNs, started.elapsed().as_nanos() as u64);
            }
            return None;
        };
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::DynamicNs, started.elapsed().as_nanos() as u64);
        taken[first..first + n].fill(true);
        self.free.fetch_sub(n as u32, std::sync::atomic::Ordering::Relaxed);
        *next = ((first + n) % self.dynamic as usize) as u32;
        Some(self.count + first as u32)
    }

    /// Two consecutive slots (view heaps only) kept until
    /// [`HostHeap::free_pair`]: past the dynamic slots where the heap has
    /// room (a host copy's views, `crate::memory`, for as long as it
    /// lives), else dynamic ones.
    pub(crate) fn pair(&self) -> Option<u32> {
        if self.sampler {
            return None;
        }
        {
            let mut pairs = self.pairs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(index) = pairs.1.pop() {
                return Some(index);
            }
            if pairs.0 + 2 <= self.pairs_end {
                pairs.0 += 2;
                return Some(pairs.0 - 2);
            }
        }
        self.dynamic(2)
    }

    /// Gives back a pair [`HostHeap::pair`] returned.
    pub(crate) fn free_pair(&self, index: u32) {
        if index >= self.count + self.dynamic {
            self.pairs.lock().unwrap_or_else(|e| e.into_inner()).1.push(index);
        } else {
            self.release(index, 2);
        }
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// The table's buffer, for a list to hold.
    pub(crate) fn strides_buffer(&self) -> Option<&ID3D12Resource> {
        self.strides.as_ref().map(|s| &s.0)
    }

    /// Makes host slot `index` null again, as it is before its view is
    /// made: a null view of the kind it held.
    fn unmake(&self, made: &mut [u64], index: u32) {
        self.forget_copied(index);
        let Some(key) = made.get_mut(index as usize).map(std::mem::take) else {
            return;
        };
        if key == 0 {
            return;
        }
        match view_at(key as u32, (key >> 32) as u32) {
            Some(view) => null_for(&view, self.cpu(index)),
            None => null_view(self.cpu(index)),
        }
        self.keep(index, None);
        self.set_stride(index, None);
        self.set_mark(index, None);
        self.note_address(index, None);
        if index < self.count { self.viewed.set(index, None, false); }
    }

    /// Update shader metadata and invalidate replaced placed-texture views.
    /// Upload scheduling belongs to `bindless::Viewed` and the watch tracker.
    fn note_view(&self, index: u32, view: Option<&View>, seen: bool) -> Vec<u32> {
        if self.sampler {
            return Vec::new();
        }
        // SAFETY: the upload mapping has SLOT_STRIDES_COUNT dwords.
        unsafe { note_slot_stride(self.strides.as_ref().map(|s| s.2), index, view) };
        let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
        let span = match view {
            Some(View::Placed(false, resource, desc)) if resource[0] != 1 => {
                let start = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                placed.bytes(resource).map(|bytes| (start, start.saturating_add(bytes)))
            }
            _ => None,
        };
        match span {
            Some((start, end)) => {
                placed.rule = stale_rule();
                placed.note(index, start, end, !seen)
            }
            None => {
                placed.forget(index);
                Vec::new()
            }
        }
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at + n).min(self.dynamic) as usize;
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            let given = slots.iter().filter(|taken| **taken).count() as u32;
            self.free.fetch_add(given, std::sync::atomic::Ordering::Relaxed);
            self.too_long.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
            slots.fill(false);
        }
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot.
    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let (kept, mark) = make_marked(view, self.cpu(index));
        if kept.is_none() {
            null_for(view, self.cpu(index));
        }
        self.keep(index, kept.flatten());
        self.set_stride(index, Some(view));
        self.set_mark(index, mark);
    }

    /// Makes slot `index` (a dynamic one) a raw view, shader resource or
    /// unordered access (`uav`), of `bytes` of `buffer` from byte `first`
    /// (both multiples of 4), which it holds; a null one for no buffer, or
    /// for unordered access to a buffer shaders cannot write.
    pub(crate) fn raw_view_at(&self, index: u32, buffer: Option<&ID3D12Resource>, first: u64, bytes: u64, uav: bool) {
        let Some(host) = host() else { return };
        let writable = |b: &ID3D12Resource| {
            // SAFETY: a live buffer.
            unsafe { b.GetDesc() }.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
        };
        let buffer = buffer.filter(|&b| !uav || writable(b));
        // shortcut: a buffer view holds at most 2^27 elements (512 MiB raw).
        let (first, elements) = match buffer {
            Some(_) => (first / 4, (bytes / 4).clamp(1, 1 << 27) as u32),
            // A null view still names its size.
            None => (0, 1),
        };
        // SAFETY: a slot of our heap; the buffer is live (or none: a null view).
        unsafe {
            if uav {
                let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            FirstElement: first,
                            NumElements: elements,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                };
                host.device
                    .CreateUnorderedAccessView(buffer, None, Some(&desc), self.cpu(index));
            } else {
                let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV {
                            FirstElement: first,
                            NumElements: elements,
                            StructureByteStride: 0,
                            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                        },
                    },
                };
                host.device
                    .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index));
            }
        }
        self.keep(index, buffer.map(|b| (b.clone(), None)));
        self.write_stride(index, 4);
        self.set_mark(index, None);
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.keep(index, None);
        self.set_stride(index, None);
        self.set_mark(index, None);
        if index < self.count {
            self.viewed.set(index, None, false);
        }
    }

    pub(crate) fn raw_uav_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        self.raw_view_at(index, Some(buffer), offset, len, true);
    }

    pub(crate) fn null_raw_uav_at(&self, index: u32) {
        self.raw_view_at(index, None, 0, 0, true);
    }

    /// Enters the byte stride of `view` (0 for none or not a buffer view)
    /// for slot `index` in the view-stride table, with the integer-format
    /// bits of a typed buffer view ([`view_kind`]).
    fn set_stride(&self, index: u32, view: Option<&View>) {
        self.write_stride(index, view.map_or(0, |v| view_stride(v) | view_kind(v)));
    }

    fn write_stride(&self, index: u32, stride: u32) {
        let Some((_, data)) = &self.view_strides else { return };
        if index >= self.count.max(victory_shader::emit::HEAP_VIEWS) {
            return;
        }
        // SAFETY: a dword of the mapped table, which has one per slot below
        // the larger of the title's count and `HEAP_VIEWS`.
        unsafe { (*data as *mut u32).add(index as usize).write_volatile(stride) };
    }

    /// The view-stride table, for root SRV `VIEW_STRIDES`.
    pub(crate) fn strides(&self) -> Option<&ID3D12Resource> {
        self.view_strides.as_ref().map(|(buffer, _)| buffer)
    }

    /// Enters slot `index`'s view-mark entry: the mark bit and phase of the
    /// unordered buffer view there ([`crate::shadow::Shadow::mark_entry`]),
    /// none for anything else.
    pub(crate) fn set_mark(&self, index: u32, entry: Option<(u32, u32)>) {
        let Some((_, data)) = &self.marks else { return };
        if index >= self.count.max(victory_shader::emit::HEAP_VIEWS) {
            return;
        }
        let entry = entry.unwrap_or((victory_shader::emit::VIEW_MARK_NONE, 0));
        // SAFETY: two dwords of the mapped table, which has two per slot
        // below the larger of the title's count and `HEAP_VIEWS`.
        unsafe {
            let at = (*data as *mut u32).add(2 * index as usize);
            at.write_volatile(entry.0);
            at.add(1).write_volatile(entry.1);
        }
    }

    /// The view-mark table, for the root SRV of shaders marking their writes.
    pub(crate) fn marks(&self) -> Option<&ID3D12Resource> {
        self.marks.as_ref().map(|(buffer, _)| buffer)
    }

    /// Slot `index` now views `resource` (or nothing of the host's).
    fn keep(&self, index: u32, resource: Kept) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = kept.get_mut(index as usize) {
            let old = std::mem::replace(slot, resource);
            #[cfg(feature = "diagnostics")]
            if let Some((old, _)) = &old
                && !self.sampler
                && slot.as_ref().map(|s| windows::core::Interface::as_raw(&s.0)) != Some(windows::core::Interface::as_raw(old))
                // SAFETY: a live resource.
                && unsafe { old.GetDesc() }.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER
            {
                use windows::core::Interface;
                let left = crate::resource::life::holders(old);
                crate::resource::life::note(old.as_raw() as usize, crate::resource::life::SLOT, u64::from(index), left);
            }
            // Title threads rewrite slots while the GPU runs a submission;
            // one of its shaders may hold the old descriptor (or read the
            // slot at an index from wrong data): what it views is freed
            // only once that work has run.
            if let Some((old, _)) = old {
                crate::host::retire(old);
            }
        }
    }

    /// The host address of the slots' view addresses (see the field), made
    /// on first use from the slots made so far.
    pub(crate) fn view_addresses(&self) -> Option<u64> {
        let made = self.addresses.get_or_init(|| {
            let (buffer, data) = host()?.upload_buffer(u64::from(self.count) * 16)?;
            crate::host::name(&buffer, || "view addresses".into());
            let data = data as usize;
            let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            for (i, &key) in made.iter().enumerate() {
                let view = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten();
                write_address(data, i, view.as_ref());
            }
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data, gpu))
        });
        made.as_ref().map(|m| m.2)
    }

    /// Slot `index` now holds `view` (or no view): its view address.
    fn note_address(&self, index: u32, view: Option<&View>) {
        if let Some(Some((_, data, _))) = self.addresses.get() {
            write_address(*data, index as usize, view);
        }
    }

    /// Makes slots `index` and `index + 1` (dynamic ones) a raw shader
    /// resource view and a raw unordered access view of the first `bytes`
    /// (a multiple of 4) of `buffer`, which they hold; null views for no
    /// buffer.
    pub(crate) fn raw_pair_at(&self, index: u32, buffer: Option<&ID3D12Resource>, bytes: u64) {
        let Some(host) = host() else { return };
        // shortcut: a buffer view holds at most 2^27 elements (512 MiB raw);
        // a longer copy's rest reads 0 through it.
        let elements = (bytes / 4).min(1 << 27) as u32;
        let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 0,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: elements,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        // SAFETY: slots of our heap; the buffer is live (or none: null views).
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&srv), self.cpu(index));
            host.device
                .CreateUnorderedAccessView(buffer, None, Some(&uav), self.cpu(index + 1));
        }
        self.keep(index, buffer.map(|b| (b.clone(), None)));
        self.keep(index + 1, buffer.map(|b| (b.clone(), None)));
        self.write_stride(index, 4);
        self.write_stride(index + 1, 4);
        // Writes through these are title memory's, marked in its page table.
        self.set_mark(index, None);
        self.set_mark(index + 1, None);
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + u64::from(index) * u64::from(self.step),
        }
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// For a slot on a page of the title's heap that changed with no call
    /// (`super::sync_heaps`: the title copies records into its heap as
    /// bytes): [`HostHeap::mirror`], if the `bytes` bytes of the record at
    /// `handle` are not the ones this last saw there. True if it did.
    ///
    /// The page says only that something on it was written, and `mirror`
    /// by itself would take every buffer or placed-texture record on it for
    /// newly written each time: deferred placed textures retain a record
    /// key, while API writes and invalidation clear the copied fingerprint.
    /// `record`: the record's bytes as the caller read them (through the
    /// kernel, which fails on a page the title unmapped meanwhile where a
    /// read in place would fault: the title unmaps heap pages too).
    pub(crate) fn mirror_copied(&self, index: u32, record: &[u8]) -> bool {
        if index >= self.count {
            return false;
        }
        let Some(header) = copied_header(record) else {
            return false;
        };
        let print = record_print(record) | 1;
        if self
            .copied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)
            .is_some_and(|&last| last == print)
        {
            return false;
        }
        self.apply_copied(index, header, print)
    }

    /// Apply the captured header, then publish its fingerprint under the
    /// same lock that serializes API writes and invalidation of host slots.
    fn apply_copied(&self, index: u32, header: [u32; 3], print: u64) -> bool {
        let [tag, i, generation] = header;
        if tag == VIEW_TAG && let Some(view) = view_at(i, generation)
            && !is_placed_texture(&view) {
            prepare(&view);
        }
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        self.mirror_record(&mut made, index, header, false);
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        if copied.is_empty() {
            copied.resize(self.count as usize, 0);
        }
        copied[index as usize] = print;
        true
    }

    /// Called under `made`; never acquire `made` while holding `copied`.
    fn forget_copied(&self, index: u32) {
        if let Some(last) = self
            .copied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(index as usize)
        {
            *last = 0;
        }
    }

    /// [`HostHeap::mirror_copied`] for every record of one changed page of
    /// the title's heap (`super::sync_heaps`): title slots `slots`, the
    /// record of slot `i` at title address `start + i * step`, and `page`
    /// the bytes read from title address `from` on. Returns the records
    /// looked at and those mirrored.
    ///
    /// Compare fingerprints under one lock per page. Changed records carry
    /// their captured headers out of the scan; never reread live title bytes.
    pub(crate) fn mirror_copied_page(
        &self,
        slots: std::ops::Range<u64>,
        (start, step): (u64, u64),
        from: usize,
        page: &[u8],
    ) -> (u64, u64) {
        let mut looked = 0u64;
        let mut due = Vec::new();
        {
            let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            if copied.is_empty() {
                copied.resize(self.count as usize, 0);
            }
            for index in slots {
                let handle = start + index * step;
                // A record that straddles the range's ends is left to the
                // range that holds its first byte.
                let Some(at) = (handle as usize).checked_sub(from) else {
                    continue;
                };
                let Some(record) = page.get(at..at + step as usize) else {
                    continue;
                };
                looked += 1;
                // None past the host heap's slots, as `mirror_copied`.
                let Some(last) = copied.get_mut(index as usize) else {
                    continue;
                };
                // Never 0, which is "never looked at".
                let print = record_print(record) | 1;
                // Only a synchronized record can bypass the slot lifecycle.
                // An unseen zero record may be clearing an API-written slot.
                if *last == print {
                    continue;
                }
                if let Some(header) = copied_header(record) {
                    due.push((index as u32, header, print));
                }
            }
        }
        let mut mirrored = 0;
        for (index, header, print) in due {
            mirrored += u64::from(self.apply_copied(index, header, print));
        }
        (looked, mirrored)
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are ready for bindless reads; placed textures wait for a draw
    /// or a shader read mark before allocating their host copy.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        if index >= self.count {
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        loop {
            let mut bytes = [0u8; 12];
            if !crate::host::read_into(handle as *const u8, &mut bytes) { return None; }
            let header = copied_header(&bytes)?;
            let [tag, i, generation] = header;
            let key = u64::from(generation) << 32 | u64::from(i);
            let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
            let needs = {
                let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
                made[index as usize] != key || (for_draw && view.as_ref().is_some_and(is_texture)
                    && self.kept.lock().unwrap_or_else(|e| e.into_inner())[index as usize].is_none())
            };
            if needs && let Some(view) = &view
                && (for_draw || !is_placed_texture(view)) {
                prepare(view);
            }
            let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            let mut current = [0u8; 12];
            if !crate::host::read_into(handle as *const u8, &mut current) { return None; }
            if current != bytes { continue; }
            return self.mirror_record(&mut made, index, header, for_draw);
        }
    }

    fn mirror_record(
        &self,
        made: &mut [u64],
        index: u32,
        [tag, i, generation]: [u32; 3],
        for_draw: bool,
    ) -> Option<View> {
        let key = u64::from(generation) << 32 | u64::from(i);
        // API writes supersede the copied record even if the host slot is
        // still deferred. Draws preserving the same installed view do not.
        if !for_draw || tag != VIEW_TAG || made[index as usize] != key {
            self.forget_copied(index);
        }
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        let Some(view) = view else {
            self.forget_copied(index);
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            if made[index as usize] != 0 {
                made[index as usize] = 0;
                self.make_null(index);
                self.note_address(index, None);
            }
            // Invalid records also clear shader metadata and span bookkeeping.
            let _ = self.note_view(index, None, false);
            return None;
        };
        let lazy = for_draw && is_texture(&view)
            && self.kept.lock().unwrap_or_else(|e| e.into_inner())[index as usize].is_none();
        if made[index as usize] != key || lazy {
            // Views of other textures in the memory this one describes
            // are stale: their host slots read nothing from here on, and
            // let go of the host copies they held.
            for stale in self.note_view(index, Some(&view), for_draw) {
                if stale != index {
                    self.unmake(made, stale);
                }
            }
            let later = !for_draw && is_placed_texture(&view);
            // Another view of the placed texture the slot shows already (a
            // title that streams a texture in writes its view again with
            // more mips each time some arrive): the slot goes on showing
            // the old view until a draw makes the new
            // one and uploads what it shows. Made null here, the texture
            // read as nothing until the next draw, every
            // time it got sharper.
            if later
                && keep_views()
                && view_at(made[index as usize] as u32, (made[index as usize] >> 32) as u32)
                    .is_some_and(|old| same_placed_texture(&old, &view))
            {
                KEPT_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Some(view);
            }
            self.set_stride(index, Some(&view));
            self.note_address(index, Some(&view));
            if later { crate::shadow::forget_read(index); }
            made[index as usize] = key;
            let (kept, mark) = if later {
                (None, None)
            } else {
                make_marked(&view, self.cpu(index))
            };
            self.set_mark(index, mark);
            if kept.is_none() && !self.sampler {
                null_for(&view, self.cpu(index));
            }
            if !self.sampler {
                self.viewed.set(index, Some(&view), kept.is_some());
            }
            self.keep(index, kept.flatten());
        }
        Some(view)
    }

    /// Whether slot `index` is backed by a made host descriptor (tests).
    #[cfg(test)]
    pub(crate) fn made(&self, index: u32) -> bool {
        self.made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)
            .is_some_and(|&key| key != 0)
    }

    /// Bounded bookkeeping snapshot, including dynamic slots. Retained
    /// resource metadata is not the installed view or proof of nullness;
    /// descriptor writes may race this inspection. Never makes a resource.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn inspect_slot(&self, index: u32) -> String {
        // SAFETY: the heap is live; querying its capacity changes nothing.
        let capacity = unsafe { self.heap.GetDesc() }.NumDescriptors;
        if index >= capacity {
            return format!(
                "slot={index} capacity={capacity} class=out-of-range logical_key=untracked \
                 logical_view=untracked retained_metadata=unavailable null_descriptor=unknown"
            );
        }
        let class = if index < self.count {
            "title"
        } else if index - self.count < self.dynamic {
            "dynamic"
        } else {
            "padding"
        };
        let (key, resource) = {
            // Same lock order as mirror/keep. Clone before releasing the
            // locks so replacement cannot free the resource we describe.
            let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            (made.get(index as usize).copied(), kept.get(index as usize).cloned().flatten())
        };
        let logical = match key {
            None => "logical_key=untracked logical_view=untracked".to_owned(),
            Some(0) => "logical_key=0 logical_view=unmade".to_owned(),
            Some(key) => {
                let hint = view_at(key as u32, (key >> 32) as u32)
                    .map(|view| format!("{view:?}").chars().take(120).collect::<String>())
                    .unwrap_or_else(|| "stale".to_owned());
                format!("logical_key={key:#x} logical_view={hint}")
            }
        };
        let retained = match resource {
            Some((resource, _)) => {
                // SAFETY: our cloned reference keeps this resource live.
                let d = unsafe { resource.GetDesc() };
                format!(
                    "identity={:p} dimension={} format={} dims={}x{}x{} mips={} samples={}/{} flags={:#x}",
                    windows::core::Interface::as_raw(&resource),
                    d.Dimension.0, d.Format.0, d.Width, d.Height, d.DepthOrArraySize,
                    d.MipLevels, d.SampleDesc.Count, d.SampleDesc.Quality, d.Flags.0
                )
            }
            None => "none (no retained resource)".to_owned(),
        };
        format!(
            "slot={index} capacity={capacity} class={class} {logical} \
             retained_metadata=[{retained}] null_descriptor=unknown"
        )
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self.made.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize)?;
        // A view the host could not make leaves a null view, its record
        // noted all the same; it keeps no resource.
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize).is_some_and(Option::is_some);
        if key == 0 || (!kept && !self.sampler) {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }

    /// Slots viewing one of the host textures `textures` (raw pointers),
    /// which are given up: each gets a null view of its kind and holds the
    /// texture no more (retired until the running submission has run), its
    /// record stays noted so heap syncs leave it, and the next draw reading
    /// it through a table or shader reading it through the heap (its read
    /// mark, cleared) makes it again.
    /// The slots are found by a walk of the heap, once per pass that gives
    /// textures up: a map of each texture's slots, kept at every slot the
    /// title rewrote, cost ~25 us a slot (a texture viewed in thousands of
    /// slots had its list searched each time; 62,000 slots a frame, run
    /// evict3).
    pub(crate) fn evict(&self, textures: &std::collections::HashSet<usize>) {
        let slots: Vec<u32> = {
            let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            (0..kept.len() as u32)
                .filter(|&i| {
                    kept[i as usize]
                        .as_ref()
                        .is_some_and(|k| k.1.is_none() && textures.contains(&(windows::core::Interface::as_raw(&k.0) as usize)))
                })
                .collect()
        };
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        for index in slots {
            let key = made.get(index as usize).copied().unwrap_or(0);
            let view = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten();
            match &view {
                Some(v) => null_for(v, self.cpu(index)),
                None => null_view(self.cpu(index)),
            }
            self.keep(index, None);
            if index < self.count {
                self.viewed.set(index, view.as_ref(), false);
                crate::shadow::forget_read(index);
            }
        }
    }

    /// The host textures (raw pointers) slots `slots` view.
    pub(crate) fn textures_at(&self, slots: &[u32]) -> Vec<usize> {
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .iter()
            .filter_map(|&i| kept.get(i as usize)?.as_ref().filter(|k| k.1.is_none()))
            .map(|k| windows::core::Interface::as_raw(&k.0) as usize)
            .collect()
    }

    /// The view of slot `index` if it is a placed texture's left without a
    /// host view (made when read).
    pub(crate) fn unmade_placed(&self, index: u32) -> Option<View> {
        let key = *self.made.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize)?;
        let view = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten()?;
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize).is_some_and(Option::is_some);
        (is_placed_texture(&view) && !kept).then_some(view)
    }

    /// A shader read slot `index` through the heap: the texture it views
    /// was used now.
    pub(crate) fn stamp_read(&self, index: u32) {
        let key = self.made.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize).copied().unwrap_or(0);
        let Some(view) = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten() else {
            return;
        };
        let view = placed_texture(&view).unwrap_or(view);
        if let View::Shader(r, _) = view
            && let Some(t) = crate::resource::made_copy(r as crate::object::This)
        {
            t.stamp();
        }
    }

    /// Whether slot `index` holds a host view of a resource (tests).
    #[cfg(test)]
    pub(crate) fn kept(&self, index: u32) -> bool {
        self.kept.lock().unwrap_or_else(|e| e.into_inner())[index as usize].is_some()
    }
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

/// The unsigned integer format with the layout of a 16-bit float or
/// signed integer format (one, two or four channels), if `format` is one.
pub(crate) fn half_format(format: u32) -> Option<u32> {
    match format {
        // R16G16B16A16_FLOAT and _SINT: R16G16B16A16_UINT.
        10 | 14 => Some(12),
        // R16G16_FLOAT and _SINT: R16G16_UINT.
        34 | 38 => Some(36),
        // R16_FLOAT and _SINT: R16_UINT.
        54 | 59 => Some(57),
        _ => None,
    }
}

/// `view` as a shader is shown it where it moves a buffer's 16-bit
/// channels as unsigned integers
/// (`victory_shader::emit::HostShader::half_views`): a buffer view in a
/// 16-bit float or signed format, in the unsigned format of the same
/// layout, through which every 16-bit pattern is read and stored as it
/// is. `None` for every other view: the title's own serves (an unsigned
/// one already carries the bits; a texture can only be viewed in the
/// formats it was made for).
pub(crate) fn half_view(view: &View) -> Option<View> {
    // Buffer views: dimension 1 in the title's description of either kind.
    const BUFFER: u32 = 1;
    match view {
        View::Shader(resource, Some(desc)) if desc[1] == BUFFER => {
            let mut desc = *desc;
            desc[0] = half_format(desc[0])?;
            Some(View::Shader(*resource, Some(desc)))
        }
        View::Unordered(resource, counter, Some(desc)) if desc[1] == BUFFER => {
            let mut desc = *desc;
            desc[0] = half_format(desc[0])?;
            Some(View::Unordered(*resource, *counter, Some(desc)))
        }
        View::Placed(unordered, resource, desc) if resource[0] == BUFFER && desc[1] == BUFFER => {
            let mut desc = *desc;
            desc[0] = half_format(desc[0])?;
            Some(View::Placed(*unordered, *resource, desc))
        }
        _ => None,
    }
}

/// The format of `view`, a buffer view through which a shader moves
/// 16-bit channels as unsigned integers
/// (`victory_shader::emit::HostShader::half_views`), if those integers
/// are not what the console gives: a format of wider channels, a
/// normalised one or a signed 8-bit one, whose values the console converts
/// to and from halves. `None` for the formats the integers serve exactly
/// (16 bits a channel of any kind, through [`half_view`] where needed, and
/// unsigned channels of fewer bits; the title's are 16-bit unsigned) and
/// for any other view.
pub(crate) fn half_inexact(view: &View) -> Option<u32> {
    // Buffer views: dimension 1 in the title's description of either kind.
    const BUFFER: u32 = 1;
    // R16G16B16A16, R16G16 and R16 (float, unsigned, signed), then
    // R10G10B10A2, R8G8B8A8, R8G8 and R8 unsigned.
    const EXACT: [u32; 13] = [10, 12, 14, 34, 36, 38, 54, 57, 59, 25, 30, 50, 62];
    let format = match view {
        View::Shader(_, Some(desc)) | View::Unordered(_, _, Some(desc)) if desc[1] == BUFFER => {
            desc[0]
        }
        View::Placed(_, resource, desc) if resource[0] == BUFFER && desc[1] == BUFFER => desc[0],
        _ => return None,
    };
    (!EXACT.contains(&format)).then_some(format)
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// The subresources named by a texture view. Host textures allocate the whole
/// mip chain; the title's declared mip, slice and plane ranges remain authoritative.
/// None retains the default whole-resource path.
pub(crate) fn view_subresources(view: &View) -> Option<Vec<u32>> {
    let (resource, unordered, words) = match view {
        View::Shader(resource, desc) => (*resource, false, desc),
        View::Unordered(resource, _, desc) => (*resource, true, desc),
        _ => return None,
    };
    let texture = crate::resource::host_resource(resource as crate::object::This)?;
    // SAFETY: a live texture; union members below are selected by dimension.
    let own = unsafe { texture.GetDesc() };
    let view = if unordered {
        TextureView::Unordered(words.as_ref().map(uav_desc))
    } else {
        TextureView::Shader(words.as_ref().map(srv_desc))
    };
    texture_view_subresources(&own, view)
}

enum TextureView {
    Shader(Option<D3D12_SHADER_RESOURCE_VIEW_DESC>),
    Unordered(Option<D3D12_UNORDERED_ACCESS_VIEW_DESC>),
}

fn texture_view_subresources(own: &D3D12_RESOURCE_DESC, view: TextureView) -> Option<Vec<u32>> {
    let mips = u32::from(own.MipLevels);
    let slices = if own.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        u32::from(own.DepthOrArraySize)
    };
    let (mip, levels, slice, count, plane) = if let TextureView::Unordered(desc) = view {
        let Some(desc) = desc else {
            // A default UAV exposes mip zero of every array slice, not
            // the whole mip chain. A 3D texture has one subresource per mip.
            return Some(subresource_range(mips, slices, 0, 1, 0, slices, 0));
        };
        unsafe {
            match desc.ViewDimension {
                D3D12_UAV_DIMENSION_TEXTURE1D => (desc.Anonymous.Texture1D.MipSlice, 1, 0, 1, 0),
                D3D12_UAV_DIMENSION_TEXTURE1DARRAY => {
                    let v = desc.Anonymous.Texture1DArray;
                    (v.MipSlice, 1, v.FirstArraySlice, v.ArraySize, 0)
                }
                D3D12_UAV_DIMENSION_TEXTURE2D => {
                    let v = desc.Anonymous.Texture2D;
                    (v.MipSlice, 1, 0, 1, v.PlaneSlice)
                }
                D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                    let v = desc.Anonymous.Texture2DArray;
                    (v.MipSlice, 1, v.FirstArraySlice, v.ArraySize, v.PlaneSlice)
                }
                D3D12_UAV_DIMENSION_TEXTURE3D => (desc.Anonymous.Texture3D.MipSlice, 1, 0, 1, 0),
                _ => return None,
            }
        }
    } else {
        let TextureView::Shader(Some(desc)) = view else {
            return None;
        };
        unsafe {
            match desc.ViewDimension {
                D3D12_SRV_DIMENSION_TEXTURE1D => {
                    let v = desc.Anonymous.Texture1D;
                    (v.MostDetailedMip, v.MipLevels, 0, 1, 0)
                }
                D3D12_SRV_DIMENSION_TEXTURE1DARRAY => {
                    let v = desc.Anonymous.Texture1DArray;
                    (
                        v.MostDetailedMip,
                        v.MipLevels,
                        v.FirstArraySlice,
                        v.ArraySize,
                        0,
                    )
                }
                D3D12_SRV_DIMENSION_TEXTURE2D => {
                    let v = desc.Anonymous.Texture2D;
                    (v.MostDetailedMip, v.MipLevels, 0, 1, v.PlaneSlice)
                }
                D3D12_SRV_DIMENSION_TEXTURE2DARRAY => {
                    let v = desc.Anonymous.Texture2DArray;
                    (
                        v.MostDetailedMip,
                        v.MipLevels,
                        v.FirstArraySlice,
                        v.ArraySize,
                        v.PlaneSlice,
                    )
                }
                D3D12_SRV_DIMENSION_TEXTURE2DMS => (0, 1, 0, 1, 0),
                D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY => {
                    let v = desc.Anonymous.Texture2DMSArray;
                    (0, 1, v.FirstArraySlice, v.ArraySize, 0)
                }
                D3D12_SRV_DIMENSION_TEXTURE3D => {
                    let v = desc.Anonymous.Texture3D;
                    (v.MostDetailedMip, v.MipLevels, 0, 1, 0)
                }
                D3D12_SRV_DIMENSION_TEXTURECUBE => {
                    let v = desc.Anonymous.TextureCube;
                    (v.MostDetailedMip, v.MipLevels, 0, 6, 0)
                }
                D3D12_SRV_DIMENSION_TEXTURECUBEARRAY => {
                    let v = desc.Anonymous.TextureCubeArray;
                    (
                        v.MostDetailedMip,
                        v.MipLevels,
                        v.First2DArrayFace,
                        v.NumCubes.saturating_mul(6),
                        0,
                    )
                }
                _ => return None,
            }
        }
    };
    Some(subresource_range(
        mips, slices, mip, levels, slice, count, plane,
    ))
}

fn subresource_range(
    mips: u32,
    slices: u32,
    mip: u32,
    levels: u32,
    slice: u32,
    count: u32,
    plane: u32,
) -> Vec<u32> {
    let mut result = Vec::new();
    for array in slice..slice.saturating_add(count).min(slices) {
        for level in mip..mip.saturating_add(levels).min(mips) {
            if let Some(index) = plane
                .checked_mul(slices)
                .and_then(|v| v.checked_add(array))
                .and_then(|v| v.checked_mul(mips))
                .and_then(|v| v.checked_add(level))
            {
                result.push(index);
            }
        }
    }
    result
}

#[cfg(test)]
mod subresource_tests {
    use super::*;

    fn texture(mips: u16, slices: u16) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            MipLevels: mips,
            DepthOrArraySize: slices,
            ..Default::default()
        }
    }

    fn srv(first: u32, levels: u32) -> TextureView {
        TextureView::Shader(Some(D3D12_SHADER_RESOURCE_VIEW_DESC {
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MostDetailedMip: first,
                    MipLevels: levels,
                    ..Default::default()
                },
            },
            ..Default::default()
        }))
    }

    fn uav(mip: u32) -> TextureView {
        TextureView::Unordered(Some(D3D12_UNORDERED_ACCESS_VIEW_DESC {
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_UAV {
                    MipSlice: mip,
                    PlaneSlice: 0,
                },
            },
            ..Default::default()
        }))
    }

    #[test]
    fn decoded_mip_chain_views_keep_each_pass_disjoint() {
        let own = texture(7, 1);
        for first in [0, 3] {
            let reads = texture_view_subresources(&own, srv(first, 1)).unwrap();
            assert_eq!(reads, [first]);
            for output in first + 1..=first + 3 {
                let writes = texture_view_subresources(&own, uav(output)).unwrap();
                assert_eq!(writes, [output]);
                assert!(writes.iter().all(|index| !reads.contains(index)));
            }
        }
    }

    #[test]
    fn default_uav_leaves_other_mips_available_for_reads() {
        let own = texture(4, 2);
        let writes = texture_view_subresources(&own, TextureView::Unordered(None)).unwrap();
        assert_eq!(writes, [0, 4]);
        let reads = texture_view_subresources(&own, srv(1, 1)).unwrap();
        assert!(reads.iter().all(|index| !writes.contains(index)));
        let volume = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE3D,
            ..texture(4, 16)
        };
        assert_eq!(
            texture_view_subresources(&volume, TextureView::Unordered(None)).unwrap(),
            [0]
        );
    }

    #[test]
    fn mip_views_are_disjoint() {
        assert_eq!(subresource_range(5, 1, 0, 1, 0, 1, 0), [0]);
        assert_eq!(subresource_range(5, 1, 2, 1, 0, 1, 0), [2]);
        assert_eq!(subresource_range(5, 1, 3, u32::MAX, 0, 1, 0), [3, 4]);
    }

    #[test]
    fn array_and_plane_offsets_use_resource_dimensions() {
        assert_eq!(subresource_range(4, 3, 1, 2, 1, 2, 1), [17, 18, 21, 22]);
        assert!(subresource_range(4, 3, 4, u32::MAX, 0, 1, 0).is_empty());
        assert!(subresource_range(4, 3, 0, 1, 3, 1, 0).is_empty());
    }
}

/// The byte stride of a buffer view (its element size, at least 1), 0 for
/// other views: what the view-stride table holds for it.
fn view_stride(view: &View) -> u32 {
    let (unordered, words): (bool, &[u32; 10]) = match view {
        // Read as raw buffers (`make`).
        View::Constant(..) => return 4,
        View::Shader(_, Some(desc)) => (false, desc),
        View::Unordered(_, _, Some(desc)) => (true, desc),
        View::Placed(unordered, resource, desc) if resource[0] == 1 => {
            match desc[..10].try_into() {
                Ok(words) => (*unordered, words),
                Err(_) => return 0,
            }
        }
        _ => return 0,
    };
    // The view dimension: buffer (1) for shader and unordered views alike.
    if words[1] != 1 {
        return 0;
    }
    let element = if unordered {
        let desc = uav_desc(words);
        // SAFETY: a buffer view's union member.
        let buffer = unsafe { desc.Anonymous.Buffer };
        if buffer.StructureByteStride > 0 {
            u64::from(buffer.StructureByteStride)
        } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
            4
        } else {
            crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
        }
    } else {
        element_size(&srv_desc(words))
    };
    // Nonzero: shaders taking views at computed indices read the null buffer
    // view where the table holds 0 (`victory_shader::emit`).
    element.max(1) as u32
}

/// The view-stride table's format-kind bits of a buffer view: a typed view
/// (no structure stride, not raw) of an integer format gets
/// `VIEW_INTEGER`, a signed one also `VIEW_SIGNED`; anything else none.
/// Translated shaders pass 16-bit typed data of such views as bits, not as
/// halves (`victory_shader::emit`, "d16").
fn view_kind(view: &View) -> u32 {
    use victory_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
    let (unordered, words): (bool, &[u32; 10]) = match view {
        View::Shader(_, Some(desc)) => (false, desc),
        View::Unordered(_, _, Some(desc)) => (true, desc),
        View::Placed(unordered, resource, desc) if resource[0] == 1 => match desc[..10].try_into() {
            Ok(words) => (*unordered, words),
            Err(_) => return 0,
        },
        _ => return 0,
    };
    // Buffer views only (dimension 1); a structured or raw one is no typed view.
    let (format, typed) = if unordered {
        let desc = uav_desc(words);
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        (desc.Format.0 as u32, b.StructureByteStride == 0 && b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 == 0)
    } else {
        let desc = srv_desc(words);
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        (desc.Format.0 as u32, b.StructureByteStride == 0 && b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 == 0)
    };
    match (words[1] == 1 && typed).then(|| integer_format(format)).flatten() {
        Some(true) => VIEW_INTEGER | VIEW_SIGNED,
        Some(false) => VIEW_INTEGER,
        None => 0,
    }
}

/// Whether a DXGI format is an integer one, and if so whether signed:
/// `Some(false)` for `UINT`, `Some(true)` for `SINT`, `None` otherwise.
fn integer_format(format: u32) -> Option<bool> {
    match format {
        3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62 => Some(false),
        4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => Some(true),
        _ => None,
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// Writes slot `index`'s words into the view addresses at `data`, 16 bytes
/// a slot: a buffer view's title address (dword 0 low, dword 1 bits 0-15
/// high), a texture view's size fields ([`texture_words`]), zeros for
/// anything else.
fn write_address(data: usize, index: usize, view: Option<&View>) {
    let address = view.and_then(buffer_range).map_or(0, |(a, _)| a);
    let words = match view.and_then(texture_words) {
        Some(words) => words,
        None => [address as u32, (address >> 32) as u32 & 0xFFFF, 0, 0],
    };
    // SAFETY: the mapped view addresses hold 16 bytes per title slot.
    unsafe { ((data + 16 * index) as *mut [u32; 4]).write_unaligned(words) };
}

/// A texture view's console image descriptor dwords 0-3 as far as its size
/// goes: width - 1 and height - 1 of the resource's level 0 (dword 1 bits
/// 30-31, dword 2 bits 0-13 and 14-29), dword 3 an identity swizzle, the
/// view's first and last level (bits 12-15, 16-19) and the image type (bits
/// 28-31). `None` for buffers and samplers.
// shortcut: the address, format and tiling fields read 0 (no translated
// shader reads them: the shader refuses such reads where it can tell),
// and the swizzle is the identity whatever the view's component mapping.
fn texture_words(view: &View) -> Option<[u32; 4]> {
    let (resource, desc, unordered) = match view {
        View::Shader(r, desc) => (crate::resource::texture_desc(*r as crate::object::This)?, *desc, false),
        View::Unordered(r, _, desc) => (crate::resource::texture_desc(*r as crate::object::This)?, *desc, true),
        View::Placed(unordered, resource, desc) if resource[0] != 1 => {
            (*resource, desc[..10].try_into().ok(), *unordered)
        }
        _ => return None,
    };
    Some(image_words(&resource, desc.as_ref(), unordered))
}

/// [`texture_words`] of a texture of resource description `resource` (as
/// words) through a view of description `desc` (none: the resource's own).
fn image_words(resource: &[u32; 14], desc: Option<&[u32; 10]>, unordered: bool) -> [u32; 4] {
    let width = u64::from(resource[4]) | u64::from(resource[5]) << 32;
    let height = resource[6];
    let (depth, mips) = (resource[7] & 0xFFFF, resource[7] >> 16);
    let (samples, dimension) = (resource[9], resource[0]);
    // Mip count 0: the whole chain.
    let deepest = if dimension == 4 { depth } else { 1 };
    let chain = 64 - (width.max(u64::from(height)).max(u64::from(deepest)) | 1).leading_zeros();
    let mips = if mips == 0 { chain } else { mips };
    // Image types: 8 1D, 9 2D, 10 3D, 11 cube, 12 1D array, 13 2D array,
    // 14 2D MSAA, 15 2D MSAA array.
    let (ty, first, count) = match desc {
        // Shader and unordered view dimensions number alike up to 3D (8).
        Some(d) => {
            let ty = [0, 0, 8, 12, 9, 13, 14, 15, 10, 11, 11].get(d[1] as usize).copied().unwrap_or(0);
            match (unordered, d[1]) {
                (_, 6 | 7) => (ty, 0, 1),
                (true, _) => (ty, d[4], 1),
                (false, _) => (ty, d[4], if d[5] == u32::MAX { mips.saturating_sub(d[4]) } else { d[5] }),
            }
        }
        None => {
            let array = depth > 1;
            let ty = match dimension {
                2 if array => 12,
                2 => 8,
                3 if samples > 1 => 14 + u32::from(array),
                3 => 9 + 4 * u32::from(array),
                _ => 10,
            };
            (ty, 0, if unordered { 1 } else { mips })
        }
    };
    let (w, h) = (width.saturating_sub(1) as u32, height.saturating_sub(1));
    let last = first + count.max(1) - 1;
    [
        0,
        (w & 3) << 30,
        (w >> 2 & 0x3FFF) | (h & 0xFFFF) << 14,
        0xFAC | (first & 0xF) << 12 | (last & 0xF) << 16 | ty << 28,
    ]
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The host copy a buffer view's bytes are in (the one its host view
/// reads, [`crate::shadow::for_view`]) and the view's title range.
pub(crate) fn view_copy(view: &View) -> Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)> {
    let (address, len) = buffer_range(view)?;
    let (shadow, _) = crate::shadow::for_view(address, len, u64::from(view_stride(view)))?;
    Some((shadow, address, len))
}

/// [`buffer_range`] of an unordered access view of memory at `base`.
fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The host resource a slot made by [`make`] views, if any, and the view's
/// own copy of title memory ([`crate::shadow::for_view`]): the slot holds
/// it, so that draws reading the view find and refresh that copy.
type Kept = Option<(ID3D12Resource, Option<std::sync::Arc<crate::shadow::Shadow>>)>;

/// Buffer views refused so far (diagnostics: the first are logged).
#[cfg(feature = "diagnostics")]
static REFUSED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Decode only from the snapshot whose fingerprint is being cached.
fn copied_header(record: &[u8]) -> Option<[u32; 3]> {
    let bytes: &[u8; 12] = record.get(..12)?.try_into().ok()?;
    Some(std::array::from_fn(|i| {
        u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
    }))
}

/// A 64-bit fingerprint of a descriptor record's bytes (16 or 32 of them):
/// equal records give equal prints, and two different records the same
/// print about once in 2^64. Not a hash anyone can attack: the records
/// are our own.
fn record_print(record: &[u8]) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut h = K ^ record.len() as u64;
    let mut words = record.chunks_exact(8);
    for word in &mut words {
        let w = u64::from_le_bytes(word.try_into().unwrap_or([0; 8]));
        h = (h ^ w).wrapping_mul(K);
        h ^= h >> 29;
    }
    for &b in words.remainder() {
        h = (h ^ u64::from(b)).wrapping_mul(K);
    }
    h ^ (h >> 32)
}

/// A structured buffer view (`first`, `count` elements of `stride` bytes,
/// `offset` bytes into its host copy) as a raw view of the same bytes:
/// its first dword and its dwords. Translated shaders read untyped buffers
/// as raw bytes at `element * stride + offset` (the stride from the
/// used-resource entry, or from the table by heap slot for a buffer picked
/// by index), and a raw read through a structured view gives zeros on
/// NVIDIA (reported upstream with PR #193; structured reads through raw
/// views were right there). So the host's view is raw whenever it can be:
/// the bytes must start on 16 (the host's raw view alignment) and be whole
/// dwords. A view that cannot stays structured.
fn raw_window(offset: u64, first: u64, count: u32, stride: u32) -> Option<(u64, u32)> {
    if stride == 0 {
        return None;
    }
    let start = first.checked_mul(u64::from(stride))?.checked_add(offset)?;
    let bytes = u64::from(count) * u64::from(stride);
    (start % 16 == 0 && bytes % 4 == 0)
        .then(|| u32::try_from(bytes / 4).ok().map(|dwords| (start / 4, dwords)))
        .flatten()
}

/// Whether the host accepts a buffer view with these fields of a buffer of
/// `width` bytes. It removes the device for one it does not (view creation
/// returns nothing), and the console is laxer: the title's heap holds
/// views past their memory's end, of no elements, of typeless formats
/// without the raw flag, with strides the host has no structured buffers
/// for. Since every buffer view of the heap is made for shaders that pick
/// them by index, one such view ended the run (2026-11-24, run 10).
fn buffer_view_ok(format: DXGI_FORMAT, first: u64, count: u32, stride: u32, raw: bool, width: u64) -> bool {
    use crate::resource::family;
    let f = format.0 as u32;
    let element = if raw {
        // raw: R32 typeless, counted in dwords, starting on 16 bytes
        if format != DXGI_FORMAT_R32_TYPELESS || stride != 0 || !first.is_multiple_of(4) {
            return false;
        }
        4
    } else if stride > 0 {
        // structured: no format, a stride the host takes
        if format != DXGI_FORMAT_UNKNOWN || stride > 2048 || !stride.is_multiple_of(4) {
            return false;
        }
        u64::from(stride)
    } else {
        // typed: a format with a size that is not typeless
        let typeless = family(f) == f && family(f + 1) == f;
        match crate::resource::element(f) {
            Some((bytes, 1)) if f != 0 && !typeless && bytes > 0 => bytes,
            _ => return false,
        }
    };
    count > 0
        && first
            .checked_add(u64::from(count))
            .and_then(|end| end.checked_mul(element))
            .is_some_and(|end| end <= width)
}

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<Kept> {
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    if element == 0 {
        once!(len, "buffer view of elements without a size");
        return None;
    }
    // The copy its own bytes are in, its first element counted from there.
    let start = address.checked_add(buffer.FirstElement.checked_mul(element)?)?;
    let Some((shadow, own)) =
        crate::shadow::for_view(start, u64::from(buffer.NumElements) * element, element)
    else {
        #[cfg(feature = "diagnostics")]
        if REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::info!(start = format_args!("{start:#x}"), len, element, count = buffer.NumElements, "refused buffer view: no host copy");
        }
        return None;
    };
    buffer.FirstElement = shadow.offset(start) / element;
    let (first, count) = (buffer.FirstElement, buffer.NumElements);
    if !in_buffer(&shadow.buffer, first, count, element) {
        #[cfg(feature = "diagnostics")]
        if REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::info!(start = format_args!("{start:#x}"), first, count, element, "refused buffer view: empty or past its copy");
        }
        once!(first, count, element, "buffer view empty or past its host copy");
        return None;
    }
    // A structured view can use the raw path Linux shaders already consume.
    let b = unsafe { desc.Anonymous.Buffer };
    if desc.Format == DXGI_FORMAT_UNKNOWN
        && let Some((first, count)) = raw_window(0, b.FirstElement, b.NumElements, b.StructureByteStride) {
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
        desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
            FirstElement: first, NumElements: count, StructureByteStride: 0,
            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
        };
    }
    let b = unsafe { desc.Anonymous.Buffer };
    if !buffer_view_ok(desc.Format, b.FirstElement, b.NumElements, b.StructureByteStride,
        b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0, unsafe { shadow.buffer.GetDesc() }.Width) {
        return None;
    }
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    Some(Some((shadow.buffer.clone(), own.then_some(shadow))))
}

/// Whether `count` elements of `element` bytes from element `first` lie in
/// `buffer`. The console takes a view that is empty or runs past its
/// memory; the host removes the device for one.
fn in_buffer(buffer: &ID3D12Resource, first: u64, count: u32, element: u64) -> bool {
    // SAFETY: a live buffer.
    within(unsafe { buffer.GetDesc() }.Width, first, count, element)
}

fn within(width: u64, first: u64, count: u32, element: u64) -> bool {
    count > 0 && first.saturating_add(u64::from(count)).saturating_mul(element) <= width
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
/// shortcut: what shaders write stays in the host copy; title memory (and
/// so the CPU, and copies run on the CPU) does not see it.
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<Kept> {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    if element == 0 {
        once!(len, "unordered buffer view of elements without a size");
        return None;
    }
    let start = address.checked_add(buffer.FirstElement.checked_mul(element)?)?;
    let (shadow, own) = crate::shadow::for_view(start, u64::from(buffer.NumElements) * element, element)?;
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    buffer.FirstElement = shadow.offset(start) / element;
    buffer.CounterOffsetInBytes = 0;
    let (first, count) = (buffer.FirstElement, buffer.NumElements);
    if !in_buffer(&shadow.buffer, first, count, element) {
        once!(first, count, element, "unordered buffer view empty or past its host copy");
        return None;
    }
    // SAFETY: this description selects the buffer union member.
    let b = unsafe { desc.Anonymous.Buffer };
    if desc.Format == DXGI_FORMAT_UNKNOWN
        && let Some((first, count)) = raw_window(0, b.FirstElement, b.NumElements, b.StructureByteStride) {
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
        desc.Anonymous.Buffer = D3D12_BUFFER_UAV {
            FirstElement: first, NumElements: count, Flags: D3D12_BUFFER_UAV_FLAG_RAW,
            ..Default::default()
        };
    }
    let b = unsafe { desc.Anonymous.Buffer };
    if !buffer_view_ok(desc.Format, b.FirstElement, b.NumElements, b.StructureByteStride,
        b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0, unsafe { shadow.buffer.GetDesc() }.Width) {
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), at)
    };
    MARK.with(|m| m.set(shadow.mark_entry(start)));
    Some(Some((shadow.buffer.clone(), own.then_some(shadow))))
}

thread_local! {
    /// The view-mark entry of the unordered buffer view [`make`] last made
    /// on this thread ([`make_marked`]).
    static MARK: std::cell::Cell<Option<(u32, u32)>> = const { std::cell::Cell::new(None) };
}

/// [`make`], and the view-mark entry of the view if it is an unordered
/// buffer view whose writes shaders mark (`victory_shader::emit`, "Write
/// marks").
fn make_marked(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> (Option<Kept>, Option<(u32, u32)>) {
    MARK.with(|m| m.set(None));
    let kept = make(view, at);
    (kept, MARK.with(std::cell::Cell::take))
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in a format neither of the
/// resource's family nor among the formats it was made castable to
/// (`castable`, `resource::castable`). A view of a typed texture in another
/// format of its family takes the texture's format unless it is castable.
/// shortcut: such a view (sRGB of UNORM, say, on a texture the host made
/// without castable formats) reads without its conversion.
fn fits(texture: &ID3D12Resource, castable: &[u32], mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    // A view of a target whose format the host refuses, in the title's
    // format: in the stand-in the host copy has.
    if view != own && crate::resource::target_format(view) == own {
        if let Some(format) = format {
            *format = d.Format;
        }
        return true;
    }
    let cast = castable.contains(&view);
    if family(view) != family(own) && !cast {
        once!(view, own, "view format of another family");
        return false;
    }
    if cast && family(view) != family(own) && unordered && !typed_store(view) {
        once!(view, own, "an unordered view in a format the host cannot store");
        return false;
    }
    if let Some(format) = format
        && !typeless
        && !cast
    {
        *format = d.Format;
    }
    true
}

/// Whether a view without a description of `resource` (a title texture)
/// may take its host texture's defaults: not when the host texture has
/// another format than the title's (typeless, or shared with a title
/// texture of another format of the same memory).
/// shortcut: such views are null; spell the description out (dimension,
/// mips) if the title makes them.
fn default_view(resource: crate::object::This) -> bool {
    let other = crate::resource::view_format(resource);
    if other.is_some() {
        once!(format = other, "a view without a description of a texture whose host texture has another format");
    }
    other.is_none()
}

/// Whether the host stores to typed unordered views of `format`.
fn typed_store(format: u32) -> bool {
    static KNOWN: Mutex<Vec<(u32, bool)>> = Mutex::new(Vec::new());
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, ok)) = known.iter().find(|(f, _)| *f == format) {
        return ok;
    }
    let mut support = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
        Format: DXGI_FORMAT(format as i32),
        ..Default::default()
    };
    let ok = host().is_some_and(|h| {
        // SAFETY: a plain query into a struct of its size.
        unsafe {
            h.device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_SUPPORT,
                (&raw mut support).cast(),
                size_of_val(&support) as u32,
            )
        }
        .is_ok()
            && support.Support1.0 & D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0 != 0
    });
    known.push((format, ok));
    ok
}

/// Makes the host descriptor for `view` at `at` (in any host view heap):
/// the host resource it views, if it can.
pub(crate) fn make_view(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<ID3D12Resource> {
    make(view, at).flatten().map(|k| k.0)
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            // Constant buffers in the title's slots, through the heap itself
            // or a table, are read as raw buffers by translated shaders
            // (`victory_shader::emit`, "Constant buffers in the heap"): the
            // console needs 16-byte alignment, a host constant buffer view
            // 256, which most of the title's do not have on the host.
            let shadow = crate::shadow::find(address, u64::from(size))?;
            let bytes = u64::from(size).min(shadow.left(address));
            let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        NumElements: (bytes / 4) as u32,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                        ..Default::default()
                    },
                },
            };
            buffer_srv(address, bytes, desc, at)
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(copy) = crate::resource::host_copy(resource) {
                copy.viewed.store(true, std::sync::atomic::Ordering::Relaxed);
                let texture = &copy.texture;
                // The title's views of textures with a stencil plane, as it
                // describes them (format, dimension, component mapping, a
                // pad, then first level, levels, plane): each distinct one
                // once. They say how its shaders read the stencil plane
                // (the lighting takes each pixel's material class and its
                // "inside a room" bit from it).
                #[cfg(feature = "diagnostics")]
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() }.Format.0;
                    if matches!(own, 19..=22 | 44..=47) {
                        static SEEN: Mutex<Vec<(i32, Option<[u32; 10]>)>> = Mutex::new(Vec::new());
                        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                        if seen.len() < 64 && !seen.contains(&(own, *desc)) {
                            seen.push((own, *desc));
                            tracing::info!(
                                ?resource,
                                texture_format = own,
                                view = ?desc,
                                "shader view of a depth-stencil texture"
                            );
                        }
                    }
                }
                let mut desc = desc.as_ref().map(srv_desc);
                if desc.is_none() && !default_view(resource) {
                    return None;
                }
                if !fits(texture, &copy.castable, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                #[cfg(feature = "diagnostics")]
                if tracing_views() {
                    // SAFETY: the dimension selects the matching union member.
                    let mip = desc.as_ref().filter(|d| d.ViewDimension == D3D12_SRV_DIMENSION_TEXTURE2D)
                        .map(|d| unsafe { d.Anonymous.Texture2D });
                    tracing::info!(?resource, slot = at.ptr, format = ?desc.as_ref().map(|d| d.Format.0),
                        dimension = ?desc.as_ref().map(|d| d.ViewDimension.0), ?mip,
                        "shader binding snapshot: installed SRV");
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some(Some((texture.clone(), None)))
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                buffer_srv(data as u64, len, srv_desc(desc), at)
            } else {
                None
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(copy) = crate::resource::host_copy(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at)
                    }
                    _ => None,
                };
            };
            copy.viewed.store(true, std::sync::atomic::Ordering::Relaxed);
            let texture = &copy.texture;
            // A depth texture's unordered views write its twin.
            // SAFETY: a live resource.
            if unsafe { texture.GetDesc() }.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0 {
                return crate::twin::uav(resource, &copy, desc.as_ref(), at)
                    .map(|twin| Some((twin, None)));
            }
            let mut desc = desc.as_ref().map(uav_desc);
            if desc.is_none() && !default_view(resource) {
                return None;
            }
            if !fits(texture, &copy.castable, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            #[cfg(feature = "diagnostics")]
            if tracing_views() {
                // SAFETY: the dimension selects the matching union member.
                let mip = desc.as_ref().filter(|d| d.ViewDimension == D3D12_UAV_DIMENSION_TEXTURE2D)
                    .map(|d| unsafe { d.Anonymous.Texture2D });
                tracing::info!(?resource, slot = at.ptr, format = ?desc.as_ref().map(|d| d.Format.0),
                    dimension = ?desc.as_ref().map(|d| d.ViewDimension.0), ?mip,
                    "shader binding snapshot: installed UAV");
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some(Some((texture.clone(), None)))
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                // shortcut: unordered access to a texture the title placed
                // itself is not mirrored; give it a resource as shader
                // views of such textures get when a dispatch writes one.
                once!(dimension = resource[0], "placed unordered texture view not mirrored");
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            // An empty title buffer: nothing to view, and no host copy to
            // look for (#70: such views were in flight at device removals).
            if len == 0 {
                return None;
            }
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at)
            } else {
                buffer_srv(address, len, srv_desc(words), at)
            }
        }
    }
}

/// Writes of a slot with another view of the placed texture it showed,
/// since the start: the slot kept showing the old view.
pub(crate) static KEPT_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether a slot written with another view of the placed texture it
/// shows keeps showing the old one until the new is made;
/// `VICTORY_KEEP_VIEWS=0` makes it null at once, as before.
fn keep_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_KEEP_VIEWS").is_none_or(|v| v != "0"))
}

/// Whether both are shader views of one texture the title placed in
/// memory: the same address and the same resource description, whatever
/// part of it each shows.
fn same_placed_texture(a: &View, b: &View) -> bool {
    match (a, b) {
        (View::Placed(false, resource, desc), View::Placed(false, other, other_desc)) => {
            resource[0] != 1 && resource == other && desc[10..12] == other_desc[10..12]
        }
        _ => false,
    }
}

/// Whether `view` is a view of a texture the title placed in memory itself.
fn is_placed_texture(view: &View) -> bool {
    matches!(view, View::Placed(_, resource, _) if resource[0] != 1)
}

/// Makes what a host view of `view` needs, if it is not made yet: the
/// texture it views (made on first use) or the host copy of the buffer
/// memory it views. The slot's view is then quick to make.
fn prepare(view: &View) {
    #[cfg(test)]
    tests::while_preparing();
    match view {
        View::Placed(..) if is_placed_texture(view) => {
            let _ = placed_texture(view);
        }
        View::Shader(r, _) | View::Unordered(r, _, _) if crate::resource::texture_memory(*r as crate::object::This).is_some() => {
            let _ = crate::resource::host_resource(*r as crate::object::This);
        }
        _ => {
            let _ = view_copy(view);
        }
    }
}

/// Whether `view` is a shader view of a texture (one placed in memory, or a
/// resource object's): such a slot left null is made when read.
fn is_texture(view: &View) -> bool {
    is_placed_texture(view)
        || matches!(view, View::Shader(r, _) if crate::resource::texture_memory(*r as crate::object::This).is_some())
}

/// A shader view of a texture the title placed in memory itself, as a view
/// of the resource made for that memory (on first use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let texture = crate::resource::texture_at(address, resource)?;
    Some(View::Shader(texture as usize, desc[..10].try_into().ok()))
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        // Read as raw buffers, as made (`make`).
        View::Constant(..) => return null_buffer_view(at),
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span_slots(spans: &Spans, start: u64, end: u64) -> Vec<u32> {
        spans.overlapping(start, end).iter()
            .flat_map(|at| spans.by_start[at].1.iter().copied()).collect()
    }

    // Run each device regression in its own process, with diagnostics and
    // VICTORY_D3D12_DEBUG=1. Cover null and oversized title CBVs through
    // the raw SRV representation used by translated shaders.
    #[test]
    #[ignore = "requires an isolated D3D12 device; run explicitly"]
    fn constant_buffer_make_keeps_the_device_alive() {
        check_constant_buffer_creation();
    }

    fn check_constant_buffer_creation() {
        use windows::core::Interface;

        let host = host().expect("a D3D12 host is required, not a skipped test");
        let heap = HostHeap::new(false, 4).expect("a view heap");
        let slot = heap.dynamic(1).expect("a dynamic descriptor slot");
        let memory = vec![0u8; 4 << 20];
        let address = (memory.as_ptr() as u64).next_multiple_of(64 << 10);
        let queue = host.device.cast::<ID3D12InfoQueue>().ok();
        if queue.is_none() {
            eprintln!("no D3D12 info queue: checking device survival only");
        }
        // SAFETY: all resources are live; only descriptor creation and
        // device/debug-queue queries occur here, with no GPU submission.
        unsafe {
            assert!(host.device.GetDeviceRemovedReason().is_ok(), "device alive before CBVs");
            let sizes = [0, 1, 255, 256, 257, 480, 65_535, 65_536, 65_537, 896_256, 969_472, 1_406_720, 2_785_536];
            for size in sizes {
                if let Some(queue) = &queue {
                    queue.ClearStoredMessages();
                }
                let _kept = {
                    let made = make(&View::Constant(address, size), heap.cpu(slot));
                    if size < 4 {
                        assert!(made.is_none(), "an empty raw constant view is refused");
                        null_for(&View::Constant(address, size), heap.cpu(slot));
                        None
                    } else {
                        Some(made.expect("the real make path must create a descriptor")
                            .expect("the descriptor must retain its backing resource"))
                    }
                };
                if let Some(queue) = &queue {
                    for i in 0..queue.GetNumStoredMessages() {
                        let mut bytes = 0;
                        queue.GetMessage(i, None, &mut bytes).expect("debug message size");
                        let mut storage = vec![0u64; bytes.div_ceil(8)];
                        let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
                        queue.GetMessage(i, Some(message), &mut bytes).expect("debug message");
                        let m = &*message;
                        let description = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                        assert!(
                            m.Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0,
                            "CBV size {size}, D3D12 id {}: {}",
                            m.ID.0,
                            String::from_utf8_lossy(description),
                        );
                    }
                }
                assert!(
                    host.device.GetDeviceRemovedReason().is_ok(),
                    "CBV size {size}, device removed",
                );
            }
        }
    }

    /// Exercise the same classification and mapped write as note_view,
    /// without constructing a host device or silently skipping on Linux.
    #[test]
    fn slot_stride_records_kind_and_rewrites_old_entries() {
        use victory_shader::emit::{SLOT_STRIDE_CBV, SLOT_STRIDE_UAV, SLOT_STRIDES_COUNT};
        let buffer = |unordered, stride, raw, format: DXGI_FORMAT| {
            // Host descriptor words, including explicit zero padding.
            let mut words = [0u32; 10];
            words[0] = format.0 as u32;
            words[1] = 1; // buffer dimension for both SRV and UAV
            if unordered {
                words[4] = 16; // NumElements
                words[5] = stride;
                words[8] = u32::from(raw); // D3D12_BUFFER_UAV_FLAG_RAW
                View::Unordered(0, 0, Some(words))
            } else {
                words[6] = 16; // NumElements
                words[7] = stride;
                words[8] = u32::from(raw); // D3D12_BUFFER_SRV_FLAG_RAW
                View::Shader(0, Some(words))
            }
        };
        let mut slot = 0u32;
        let mut write = |view: Option<&View>, expected, stride| {
            // SAFETY: slot is live and writable; only index zero is used.
            let actual = unsafe { note_slot_stride(Some(std::ptr::from_mut(&mut slot) as usize), 0, view) };
            assert_eq!(actual, stride);
            assert_eq!(slot, expected);
        };
        for unordered in [false, true] {
            let flag = if unordered { SLOT_STRIDE_UAV } else { 0 };
            for (stride, raw, format, bytes) in [
                (0, true, DXGI_FORMAT_R32_TYPELESS, 1),
                (48, false, DXGI_FORMAT_UNKNOWN, 48),
                (0, false, DXGI_FORMAT_R32G32B32A32_FLOAT, 16),
            ] {
                let view = buffer(unordered, stride, raw, format);
                write(Some(&view), bytes | flag, Some(bytes));
                let words = match view {
                    View::Shader(_, Some(d)) | View::Unordered(_, _, Some(d)) => d,
                    _ => unreachable!(),
                };
                let mut resource = [0; 14];
                resource[0] = 1;
                let mut desc = [0; 16];
                desc[..10].copy_from_slice(&words);
                write(Some(&View::Placed(unordered, resource, desc)), bytes | flag, Some(bytes));
                write(Some(&View::Constant(0x1000, 256)), SLOT_STRIDE_CBV, None);
                let mut texture = [0; 10];
                texture[1] = D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32;
                write(Some(&View::Shader(0, Some(texture))), 0, None);
                write(Some(&View::Constant(0x1000, 256)), SLOT_STRIDE_CBV, None);
                write(None, 0, None);
            }
            write(Some(&buffer(unordered, 0, false, DXGI_FORMAT_UNKNOWN)), 0, Some(0));
            write(Some(&buffer(unordered, u32::MAX, false, DXGI_FORMAT_UNKNOWN)), 0, Some(u32::MAX));
        }
        write(Some(&View::Shader(0, None)), 0, None);
        write(Some(&View::Unordered(0, 0, None)), 0, None);
        write(Some(&View::Sampler([0; 13])), 0, None);
        // SAFETY: out-of-range indices never touch the mapping.
        unsafe { note_slot_stride(Some(0), SLOT_STRIDES_COUNT, Some(&View::Constant(0x1000, 256))) };
    }

    /// The unsigned twin of a 16-bit float or signed format has its layout,
    /// and only buffer views in such a format are shown as another view.
    #[test]
    fn only_16_bit_float_and_signed_buffer_views_get_an_unsigned_twin() {
        for (format, unsigned) in [(10, 12), (14, 12), (34, 36), (38, 36), (54, 57), (59, 57)] {
            assert_eq!(half_format(format), Some(unsigned));
            assert_eq!(
                crate::resource::element(format),
                crate::resource::element(unsigned),
                "format {format}"
            );
        }
        // The unsigned formats themselves, normalised 16-bit formats, and
        // wider and narrower ones.
        for other in [0, 12, 36, 57, 11, 13, 35, 37, 56, 58, 42, 17, 30, 61] {
            assert_eq!(half_format(other), None, "format {other}");
        }
        const BUFFER: u32 = 1;
        let words = |format: u32, dimension: u32| [format, dimension, 5768, 0, 0, 0, 64, 0, 0, 0];
        // R16G16B16A16_FLOAT: a buffer of halves a program moves as they are.
        assert_eq!(
            half_view(&View::Unordered(7, 9, Some(words(10, BUFFER)))),
            Some(View::Unordered(7, 9, Some(words(12, BUFFER))))
        );
        assert_eq!(
            half_view(&View::Shader(7, Some(words(54, BUFFER)))),
            Some(View::Shader(7, Some(words(57, BUFFER))))
        );
        let mut placed = [0u32; 16];
        placed[..10].copy_from_slice(&words(38, BUFFER));
        placed[10] = 0x1234_0000;
        let mut twin = placed;
        twin[0] = 36;
        let memory = [BUFFER, 0, 0, 0, 512, 0, 1, 0x1_0001, 0, 1, 0, 1, 0, 0];
        assert_eq!(
            half_view(&View::Placed(true, memory, placed)),
            Some(View::Placed(true, memory, twin))
        );
        // The title's kind (R16G16B16A16_UINT and R16_UINT buffers), a
        // texture view (dimension 4: 2D), a view without a description, a
        // constant buffer: the title's own.
        assert_eq!(
            half_view(&View::Unordered(7, 0, Some(words(12, BUFFER)))),
            None
        );
        assert_eq!(half_view(&View::Shader(7, Some(words(57, BUFFER)))), None);
        assert_eq!(half_view(&View::Unordered(7, 0, Some(words(10, 4)))), None);
        assert_eq!(half_view(&View::Shader(7, None)), None);
        assert_eq!(half_view(&View::Constant(0x1000, 256)), None);
    }

    /// The buffer views through which the integers a shader moves halves
    /// as are not what the console gives are named; the title's are not.
    #[test]
    fn views_whose_values_the_console_would_convert_are_named() {
        const BUFFER: u32 = 1;
        let words = |format: u32, dimension: u32| [format, dimension, 5768, 0, 0, 0, 64, 0, 0, 0];
        for exact in [10, 12, 14, 34, 36, 38, 54, 57, 59, 25, 30, 50, 62] {
            let view = View::Unordered(7, 0, Some(words(exact, BUFFER)));
            assert_eq!(half_inexact(&view), None, "format {exact}");
        }
        // R32_FLOAT, R32_UINT, R16G16B16A16_UNORM, R8G8B8A8_UNORM, R8_SINT,
        // and a structured or raw view (no format).
        for inexact in [41, 42, 11, 28, 64, 0] {
            let view = View::Shader(7, Some(words(inexact, BUFFER)));
            assert_eq!(half_inexact(&view), Some(inexact));
        }
        let mut placed = [0u32; 16];
        placed[..10].copy_from_slice(&words(41, BUFFER));
        let memory = [BUFFER, 0, 0, 0, 512, 0, 1, 0x1_0001, 0, 1, 0, 1, 0, 0];
        assert_eq!(half_inexact(&View::Placed(true, memory, placed)), Some(41));
        // Not a buffer view.
        assert_eq!(half_inexact(&View::Shader(7, Some(words(41, 4)))), None);
        assert_eq!(half_inexact(&View::Constant(0x1000, 256)), None);
    }

    /// The two ways a translated `buffer_load_format_d16_xyzw` and
    /// `buffer_store_format_d16_xyzw` have moved a typed buffer's 16-bit
    /// channels, on the host device, over a buffer that holds every 16-bit
    /// pattern. As unsigned integers through an unsigned view (the title's
    /// own R16G16B16A16_UINT, or the twin `half_view` gives a float one)
    /// every pattern comes back as it was. By way of floats, as before
    /// (load as floats, each to its half's bits; each half widened and
    /// stored as a float): through the title's integer view that is
    /// outside what D3D12 defines, and what the device gives is printed,
    /// not checked (1 pattern of 65,536 on the cards tried: the network
    /// that moves the title's faces computed zeros).
    #[test]
    fn halves_cross_an_unsigned_buffer_view_unchanged() {
        const MOVES: &str = r#"
#define RS "DescriptorTable(UAV(u0, numDescriptors=2))"
#ifdef BY_FLOATS
RWBuffer<float4> from : register(u0);
RWBuffer<float4> to : register(u1);
#else
RWBuffer<uint4> from : register(u0);
RWBuffer<uint4> to : register(u1);
#endif
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void BY_FLOATS_OR_NOT(uint3 id : SV_DispatchThreadID) {
#ifdef BY_FLOATS
    uint4 h = f32tof16(from[id.x]);
#else
    uint4 h = from[id.x] & 0xffff;
#endif
    uint low = h.x | h.y << 16;
    uint high = h.z | h.w << 16;
#ifdef BY_FLOATS
    to[id.x] = float4(f16tof32(low), f16tof32(low >> 16), f16tof32(high), f16tof32(high >> 16));
#else
    to[id.x] = uint4(low & 0xffff, low >> 16, high & 0xffff, high >> 16);
#endif
}
"#;
        const ELEMENTS: usize = 1 << 14;
        const BYTES: u64 = ELEMENTS as u64 * 8;
        const TITLES: u32 = 12; // R16G16B16A16_UINT
        let Some(host) = host() else {
            return; // no host GPU
        };
        let heap: ID3D12DescriptorHeap = {
            let desc = D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            };
            // SAFETY: plain heap creation.
            unsafe { host.device.CreateDescriptorHeap(&desc) }.expect("a view heap")
        };
        // How many of the 65,536 patterns come back as they were, moved by
        // the program `by_floats` or not, through views in `format`.
        let moved = |by_floats: bool, format: u32| -> usize {
            let source = if by_floats {
                MOVES
                    .replace("BY_FLOATS_OR_NOT", "main")
                    .replace("#ifdef BY_FLOATS", "#if 1")
            } else {
                MOVES
                    .replace("BY_FLOATS_OR_NOT", "main")
                    .replace("#ifdef BY_FLOATS", "#if 0")
            };
            let code = crate::dxc::compile(
                &source,
                "main",
                "cs_5_1",
            )
            .expect("the test's shader");
            let (from, filled) = host.shared_buffer(BYTES).expect("a buffer");
            let (to, result) = host.shared_buffer(BYTES).expect("a buffer");
            // SAFETY: the shader's own root signature and plain creation
            // calls; both mappings hold `BYTES` bytes; the views are of
            // live buffers, in slots of the heap made above; an open list.
            unsafe {
                let root: ID3D12RootSignature = host
                    .device
                    .CreateRootSignature(0, &code)
                    .expect("a root signature");
                let pso: ID3D12PipelineState = host
                    .device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: code.as_ptr().cast(),
                            BytecodeLength: code.len(),
                        },
                        ..Default::default()
                    })
                    .expect("a pipeline");
                let filled = std::slice::from_raw_parts_mut(filled.cast::<u16>(), ELEMENTS * 4);
                for (pattern, half) in filled.iter_mut().enumerate() {
                    *half = pattern as u16;
                }
                std::ptr::write_bytes(result, 0xee, BYTES as usize);
                let view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT(format as i32),
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            NumElements: ELEMENTS as u32,
                            ..Default::default()
                        },
                    },
                };
                let start = heap.GetCPUDescriptorHandleForHeapStart();
                let step = host
                    .device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                    as usize;
                host.device.CreateUnorderedAccessView(
                    &from,
                    None::<&ID3D12Resource>,
                    Some(&view),
                    start,
                );
                let second = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start.ptr + step,
                };
                host.device.CreateUnorderedAccessView(
                    &to,
                    None::<&ID3D12Resource>,
                    Some(&view),
                    second,
                );
                let list = host.new_list().expect("a host list");
                list.list.SetDescriptorHeaps(&[Some(heap.clone())]);
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list
                    .SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
                list.list.Dispatch(ELEMENTS as u32 / 64, 1, 1);
                list.close();
                host.execute(&[&list]);
                assert!(
                    host.device.GetDeviceRemovedReason().is_ok(),
                    "the device, after views in format {format}"
                );
                let result = std::slice::from_raw_parts(result.cast::<u16>(), ELEMENTS * 4);
                (0..=u16::MAX)
                    .filter(|&p| result[usize::from(p)] == p)
                    .count()
            }
        };
        // A float view of halves is shown as the title's kind of view.
        assert_eq!(half_format(10), Some(TITLES));
        let unsigned = moved(false, TITLES);
        let by_floats = moved(true, TITLES);
        eprintln!(
            "halves through an R16G16B16A16_UINT buffer view: {unsigned} of 65536 patterns came back unchanged moved as unsigned integers, {by_floats} moved by way of floats"
        );
        assert_eq!(
            unsigned, 65_536,
            "patterns unchanged, moved as unsigned integers"
        );
    }

    /// Typed buffer views of integer formats carry their kind in the
    /// view-stride table; float, normalized, raw and structured ones none.
    #[test]
    fn integer_typed_buffer_views_are_marked_in_the_stride_table() {
        use victory_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
        // Format, dimension (buffer), first element, count, stride, counter, flags.
        let uav = |format, stride, flags| View::Unordered(0, 0, Some([format, 1, 0, 0, 64, stride, 0, 0, flags, 0]));
        assert_eq!(view_kind(&uav(12, 0, 0)), VIEW_INTEGER, "R16G16B16A16_UINT");
        assert_eq!(view_kind(&uav(57, 0, 0)), VIEW_INTEGER, "R16_UINT");
        assert_eq!(view_kind(&uav(59, 0, 0)), VIEW_INTEGER | VIEW_SIGNED, "R16_SINT");
        assert_eq!(view_kind(&uav(64, 0, 0)), VIEW_INTEGER | VIEW_SIGNED, "R8_SINT");
        assert_eq!(view_kind(&uav(10, 0, 0)), 0, "R16G16B16A16_FLOAT");
        assert_eq!(view_kind(&uav(58, 0, 0)), 0, "R16_SNORM");
        assert_eq!(view_kind(&uav(39, 0, 1)), 0, "raw");
        assert_eq!(view_kind(&uav(0, 16, 0)), 0, "structured");
        // Format, dimension, mapping, first element, count, stride, flags.
        let srv = View::Shader(0, Some([62, 1, 5768, 0, 0, 0, 64, 0, 0, 0]));
        assert_eq!(view_kind(&srv), VIEW_INTEGER, "R8_UINT");
        assert_eq!(view_stride(&uav(12, 0, 0)) | view_kind(&uav(12, 0, 0)), 8 | VIEW_INTEGER);
    }

    /// A placed texture's view the title writes into a heap is left null
    /// and noted once: heap syncs (every submission) pass it by, a draw
    /// reading it through a table makes it.
    #[test]
    fn a_placed_texture_slot_left_null_is_made_once_a_draw_reads_it() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        // A 4x1 RGBA8 texture, linear, in memory of its own, and a 2D view.
        let memory = vec![0u8; 1 << 16];
        let address = memory.as_ptr() as u64;
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let mut view = [0u32; 16];
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        crate::descriptor::create_placed_srv(null, &words, &view, title_heap.start);
        let host = title_heap.host().expect("a host view heap");
        for _ in 0..2 {
            assert!(host.mirror(0, title_heap.start, false).is_some());
            assert!(host.made(0), "the record is noted");
            assert!(!host.kept(0), "no host texture for it yet");
        }
        assert!(host.mirror(0, title_heap.start, true).is_some());
        assert!(host.kept(0), "made for the draw");
    }

    /// A slot viewing a texture given up (`crate::resource::evict`) holds it
    /// no more: heap syncs pass it by, a draw reading it through a table
    /// makes it again.
    #[test]
    fn a_slot_viewing_a_texture_given_up_is_made_again_when_read() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        // A 4x1 RGBA8 texture, linear, in memory of its own, and a 2D view.
        let memory = vec![0u8; 1 << 16];
        let address = memory.as_ptr() as u64;
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let mut view = [0u32; 16];
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        crate::descriptor::create_placed_srv(null, &words, &view, title_heap.start);
        let host = title_heap.host().expect("a host view heap");
        assert!(host.mirror(0, title_heap.start, true).is_some());
        assert!(host.kept(0), "made for the draw");
        let texture = crate::resource::texture_at(address, &words).expect("its resource");
        assert!(crate::resource::give_up_now(texture));
        assert!(!host.kept(0), "the slot holds it no more");
        assert!(host.made(0), "its record stays noted");
        assert!(host.mirror(0, title_heap.start, false).is_some());
        assert!(!host.kept(0), "heap syncs pass it by");
        assert!(host.mirror(0, title_heap.start, true).is_some());
        assert!(host.kept(0), "made again for the draw");
        assert!(crate::resource::made_copy(texture).is_some(), "with a host texture again");
    }

    /// Work to run while a view's texture is prepared, once (tests).
    pub(crate) static WHILE_PREPARING: std::sync::Mutex<Option<crate::descriptor::tests::Work>> = std::sync::Mutex::new(None);

    pub(crate) fn while_preparing() {
        let work = WHILE_PREPARING.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(work) = work {
            work();
        }
    }

    /// A record the title writes into a slot while another thread prepares
    /// the slot's old record is the one the slot ends up viewing.
    #[test]
    fn a_slot_rewritten_while_its_view_is_prepared_views_the_new_record() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 1024, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let s = 960u32;
        let at = title_heap.start + u64::from(s) * 32;
        let memory = vec![0u8; 1 << 17];
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let view_of = |address: u64| {
            let mut view = [0u32; 16];
            view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
            (view[10], view[11]) = (address as u32, (address >> 32) as u32);
            view
        };
        let (first, second) = (memory.as_ptr() as u64, memory.as_ptr() as u64 + (1 << 16));
        crate::descriptor::create_placed_srv(null, &words, &view_of(first), at);
        let host = title_heap.host().expect("a host view heap");
        let (done, waited) = std::sync::mpsc::channel();
        let second_view = view_of(second);
        *WHILE_PREPARING.lock().unwrap() = Some(Box::new(move || {
            // The title writes the other record into the slot meanwhile.
            std::thread::spawn(move || {
                crate::descriptor::create_placed_srv(std::ptr::null(), &words, &second_view, at);
                done.send(()).unwrap();
            });
            assert!(waited.recv_timeout(std::time::Duration::from_secs(5)).is_ok(), "the write waits");
        }));
        // A draw reads the slot: its first record is prepared.
        assert!(host.mirror(s, at, true).is_some());
        let second_texture = crate::resource::texture_at(second, &words).expect("its resource");
        let raw = windows::core::Interface::as_raw(&crate::resource::made_copy(second_texture).expect("made").texture) as usize;
        assert_eq!(host.textures_at(&[s]), [raw], "the slot views the record written last");
    }

    /// What a view needs is made without the slot lock: another thread
    /// binding a view of the same heap meanwhile is not held up.
    #[test]
    fn views_bound_while_another_slot_gets_its_texture_do_not_wait() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 1024, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let (first, second) = (950u32, 951u32);
        let memory = vec![0u8; 1 << 17];
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let slot = |i: u32| title_heap.start + u64::from(i) * 32;
        for (i, at) in [(first, 0u64), (second, 1 << 16)] {
            let address = memory.as_ptr() as u64 + at;
            let mut view = [0u32; 16];
            view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
            (view[10], view[11]) = (address as u32, (address >> 32) as u32);
            crate::descriptor::create_placed_srv(null, &words, &view, slot(i));
        }
        let host = title_heap.host().expect("a host view heap");
        let heap_at = title_heap.start;
        let heap = heap as usize;
        let (done, waited) = std::sync::mpsc::channel();
        *WHILE_PREPARING.lock().unwrap() = Some(Box::new(move || {
            std::thread::spawn(move || {
                let h = crate::descriptor::heap(heap as crate::object::This);
                h.host().unwrap().mirror(second, heap_at + u64::from(second) * 32, true);
                done.send(()).unwrap();
            });
            assert!(
                waited.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
                "a view bound while another slot's texture is made waits"
            );
        }));
        assert!(host.mirror(first, slot(first), true).is_some());
        assert!(WHILE_PREPARING.lock().unwrap().is_none(), "the prepare ran");
        assert!(host.kept(first) && host.kept(second), "both made");
    }

    /// The textures of slots shaders read are made without the heaps' lock:
    /// a title thread writing a view meanwhile is not held up by the make.
    #[test]
    fn views_written_while_read_slots_get_their_textures_do_not_wait() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 1024, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let (read, written) = (900u32, 901u32);
        // A 4x1 RGBA8 texture, linear, in memory of its own, and a 2D view.
        let memory = vec![0u8; 1 << 16];
        let address = memory.as_ptr() as u64;
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let mut view = [0u32; 16];
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        let slot = |i: u32| title_heap.start + u64::from(i) * 32;
        crate::descriptor::create_placed_srv(null, &words, &view, slot(read));
        let host = title_heap.host().expect("a host view heap");
        assert!(!host.kept(read), "left null until read");
        let (done, waited) = std::sync::mpsc::channel();
        let at = slot(written);
        *crate::descriptor::tests::WHILE_MAKING.lock().unwrap() = Some((
            read,
            Box::new(move || {
                std::thread::spawn(move || {
                    crate::descriptor::create_placed_srv(std::ptr::null(), &words, &view, at);
                    done.send(()).unwrap();
                });
                assert!(
                    waited.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
                    "a view written while a read slot's texture is made waits"
                );
            }),
        ));
        crate::descriptor::make_read(&[read]);
        assert!(crate::descriptor::tests::WHILE_MAKING.lock().unwrap().is_none(), "the make ran");
        assert!(host.kept(read), "made for the read");
    }

    /// A heap sync makes the slots whose records the title changed as bytes
    /// (a copy of another slot's record, a record cleared) and passes the
    /// others by.
    #[test]
    fn heap_syncs_make_the_slots_whose_records_changed() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let start = title_heap.start;
        // A view of no memory: a view of the test's own heap memory would get
        // a host copy whose 4 MiB window the guards may cover (the process
        // heap guarded read-only hung the suite).
        crate::descriptor::create_cbv(null, &[0, 0, 256, 0], start);
        let host = title_heap.host().expect("a host view heap");
        let sync = || {
            let mut page = [0u8; 128];
            assert!(crate::host::read_into(start as *const u8, &mut page));
            host.mirror_copied_page(0..4, (start, 32), start as usize, &page);
        };
        // SAFETY: slots 0 and 1 of the heap's memory, 32 bytes each.
        unsafe { std::ptr::copy_nonoverlapping(start as *const u8, (start + 32) as *mut u8, 32) };
        assert!(!host.made(1), "copied as bytes: not seen yet");
        sync();
        assert!(host.made(0) && host.made(1) && !host.made(2));
        // SAFETY: as above.
        unsafe { std::ptr::write_bytes((start + 32) as *mut u8, 0, 32) };
        sync();
        assert!(host.made(0) && !host.made(1), "a cleared record leaves its slot");
    }

    /// The world's 16-byte view of 0 elements removed the device.
    #[test]
    fn empty_and_overlong_buffer_views_are_refused() {
        assert!(within(64, 0, 4, 16));
        assert!(within(64, 3, 1, 16));
        assert!(!within(64, 0, 0, 16));
        assert!(!within(64, 3, 2, 16));
        assert!(!within(64, u64::MAX, 1, 16));
    }

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// Views of one placed texture are told from views of others by the
    /// address and the resource description, not by what they show.
    #[test]
    fn views_of_one_placed_texture_are_told_by_address_and_description() {
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], first_mip: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[4], desc[5], desc[10], desc[11]) = (98, 4, first_mip, 8 - first_mip, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let low = view(texture, 3, 0x1000_0000);
        // more mips of the same texture
        assert!(same_placed_texture(&low, &view(texture, 0, 0x1000_0000)));
        // another address, another description, a buffer, an unordered view
        assert!(!same_placed_texture(&low, &view(texture, 3, 0x2000_0000)));
        let mut wider = texture;
        wider[4] = 1024;
        assert!(!same_placed_texture(&low, &view(wider, 3, 0x1000_0000)));
        let mut buffer = texture;
        buffer[0] = 1;
        assert!(!same_placed_texture(&view(buffer, 3, 0x1000_0000), &view(buffer, 3, 0x1000_0000)));
        let View::Placed(_, resource, desc) = low.clone() else { unreachable!() };
        assert!(!same_placed_texture(&low, &View::Placed(true, resource, desc)));
        assert!(!same_placed_texture(&low, &View::Shader(0, None)));
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn a_structured_view_made_raw_passes_the_host_check() {
        // 100 elements of 20 bytes from element 8, in a copy that starts 32
        // bytes before the view's memory: bytes 192..2192.
        let (first, count) = raw_window(32, 8, 100, 20).unwrap();
        assert_eq!((first, count), (48, 500));
        assert!(buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, first, count, 0, true, 4096));
        // the format a raw view must name; the structured one's is refused
        assert!(!buffer_view_ok(DXGI_FORMAT_UNKNOWN, first, count, 0, true, 4096));
        // past the buffer, not on 16 bytes, no elements
        assert!(!buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, first, count, 0, true, 2000));
        assert!(raw_window(4, 0, 100, 20).is_none());
        assert!(!buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, 0, 0, 0, true, 4096));
        // a structured view the host takes as it is
        assert!(buffer_view_ok(DXGI_FORMAT_UNKNOWN, 1, 10, 12, false, 4096));
    }

    /// Textures of which only the smallest levels are held, packed into
    /// one another's address range (the three of run 188: 0x2c000 bytes
    /// each at full size, their starts 0x2000 and 0x4000 apart): writing
    /// any of them, in any order and again, leaves the others.
    #[test]
    fn packed_neighbours_stay() {
        let mut spans = Spans::default();
        assert_eq!(spans.rule, Rule::Start);
        let at = 0xb_86bf_8000u64;
        let (a, b, c) = (at, at + 0x2000, at + 0x6000);
        assert!(spans.note(105_998, a, a + 0x2c000, true).is_empty());
        assert!(spans.note(106_004, b, b + 0x2c000, true).is_empty());
        assert!(spans.note(108_521, c, c + 0x2c000, true).is_empty());
        // The first written again (as a title does when levels arrive),
        // from another slot too: its span reaches over the other two.
        assert!(spans.note(7, a, a + 0x2c000, true).is_empty());
        assert!(spans.note(106_004, b, b + 0x2c000, true).is_empty());
        let mut all = span_slots(&spans, 0, u64::MAX);
        all.sort_unstable();
        assert_eq!(all, [7, 105_998, 106_004, 108_521]);
        // A draw makes a slot of the second texture: it is one of them.
        assert!(spans.note(9, b, b + 0x2c000, false).is_empty());
        assert_eq!(spans.by_slot.get(&9), Some(&b));
    }

    /// A texture written where another starts replaces it: at the same
    /// address with another length, or inside the block of its smallest
    /// levels. The packed neighbours of the one replaced stay.
    #[test]
    fn a_texture_written_at_the_start_of_another_replaces_it() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        // Another length from the same start: slot 1 is stale, slot 2 not.
        assert_eq!(spans.note(3, at, at + 0xb000, true), [1]);
        // A start inside the first 4 KiB of slot 3's texture: stale too.
        assert_eq!(spans.note(4, at + 0x800, at + 0x1800, true), [3]);
        // And the other way: a texture that starts just before one.
        assert_eq!(spans.note(5, at + 0x1800, at + 0x4800, true), [2]);
        let mut left = span_slots(&spans, 0, u64::MAX);
        left.sort_unstable();
        assert_eq!(left, [4, 5]);
        // Exactly 4 KiB apart: both there.
        assert!(spans.note(6, at + 0x2800, at + 0x3800, true).is_empty());
        // A draw's view that clashes is left out, and nothing is stale.
        assert!(spans.note(8, at + 0x900, at + 0x2900, false).is_empty());
        assert_eq!(spans.by_slot.get(&8), None);
        assert_eq!(spans.by_slot.len(), 3);
    }

    /// A slot written with another texture leaves its old span; the same
    /// view written again changes nothing.
    #[test]
    fn a_slot_rewritten_moves_to_its_new_span() {
        let mut spans = Spans::default();
        assert!(spans.note(1, 0x10000, 0x3c000, true).is_empty());
        assert!(spans.note(2, 0x12000, 0x3e000, true).is_empty());
        assert!(spans.note(1, 0x10000, 0x3c000, true).is_empty());
        assert_eq!(span_slots(&spans, 0x10000, 0x10001), [1]);
        // Slot 1 now views a texture far away.
        assert!(spans.note(1, 0x90000, 0x91000, true).is_empty());
        assert_eq!(span_slots(&spans, 0x10000, 0x10001), [] as [u32; 0]);
        assert_eq!(span_slots(&spans, 0x12000, 0x12001), [2]);
        assert_eq!(span_slots(&spans, 0x90000, 0x90001), [1]);
        spans.forget(1);
        spans.forget(2);
        assert!(spans.by_start.is_empty() && spans.by_slot.is_empty());
    }

    /// `VICTORY_STALE_TEXTURES=0`: nothing is ever stale, and a view that
    /// overlaps others is left out of the spans, as before.
    #[test]
    fn never_stale_leaves_overlapping_views_out() {
        let mut spans = Spans { rule: Rule::Never, ..Default::default() };
        assert!(spans.note(1, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(2, 0x2000, 0x4000, true).is_empty());
        assert!(spans.note(3, 0x1000, 0x2000, true).is_empty());
        assert_eq!(span_slots(&spans, 0, u64::MAX), [1]);
    }

    /// The rule until run 188 (`VICTORY_STALE_TEXTURES=overlap`): a view
    /// written over memory that older views describe otherwise makes
    /// those stale; views of the same span are one texture's.
    #[test]
    fn a_newer_view_of_other_memory_makes_the_older_stale() {
        let mut spans = Spans { rule: Rule::Overlap, ..Default::default() };
        // Three textures side by side, the first in two slots.
        assert!(spans.note(1, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(2, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(3, 0x3000, 0x4000, true).is_empty());
        assert!(spans.note(4, 0x8000, 0x9000, true).is_empty());
        // The same slot written with the same span again: nothing changes.
        assert!(spans.note(2, 0x1000, 0x3000, true).is_empty());
        assert_eq!(span_slots(&spans, 0x2000, 0x3800), [1, 2, 3]);
        assert_eq!(span_slots(&spans, 0x4000, 0x8000), [] as [u32; 0]);
        // A draw makes a slot whose view overlaps them: left out, nothing stale.
        assert!(spans.note(9, 0x2000, 0x3800, false).is_empty());
        assert_eq!(span_slots(&spans, 0x2000, 0x3800), [1, 2, 3]);
        // The title writes a texture over the end of the first and all of
        // the second: their three slots are stale.
        let mut stale = spans.note(5, 0x2000, 0x4000, true);
        stale.sort_unstable();
        assert_eq!(stale, [1, 2, 3]);
        assert_eq!(span_slots(&spans, 0, u64::MAX), [5, 4]);
        // Another start in the same memory: stale in turn. Another length
        // from the same start too.
        assert_eq!(spans.note(6, 0x2800, 0x3000, true), [5]);
        assert_eq!(spans.note(7, 0x2800, 0x2c00, true), [6]);
        // A slot that moves to another texture leaves its span behind.
        assert!(spans.note(7, 0x8000, 0x9000, true).is_empty());
        assert_eq!(span_slots(&spans, 0x2800, 0x2c00), [] as [u32; 0]);
        assert_eq!(span_slots(&spans, 0x8000, 0x8001), [4, 7]);
        spans.forget(4);
        spans.forget(7);
        assert!(spans.by_start.is_empty() && spans.by_slot.is_empty());
    }

    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        // Every index a translated shader can reach exists (clamped below
        // `HEAP_VIEWS`, `HEAP_SAMPLERS`).
        // SAFETY: plain queries of live heaps.
        unsafe {
            assert!(heap.heap.GetDesc().NumDescriptors > victory_shader::emit::view_heap_limit());
            let samplers = HostHeap::new(true, 128).unwrap();
            assert!(samplers.heap.GetDesc().NumDescriptors >= victory_shader::emit::HEAP_SAMPLERS);
        }
        // A slot that stops viewing a resource leaves it alive until the GPU
        // work in flight has run (a read of a freed host texture otherwise).
        // SAFETY: a plain committed buffer; one reference taken and given back.
        unsafe {
            let host = host().unwrap();
            let mut buffer: Option<ID3D12Resource> = None;
            host.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 256,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .unwrap();
            let buffer = buffer.unwrap();
            let references = |b: &ID3D12Resource| {
                let raw = windows::core::Interface::as_raw(b);
                let unknown = &*(raw as *const *const windows::core::IUnknown_Vtbl);
                ((**unknown).AddRef)(raw);
                ((**unknown).Release)(raw)
            };
            let slot = heap.count + 2;
            heap.raw_view_at(slot, Some(&buffer), 0, 256, false);
            assert_eq!(references(&buffer), 2, "ours and the slot's");
            heap.make_null(slot);
            assert!(references(&buffer) >= 2, "the slot's, retired");
            crate::host::release_retired();
            // Released on a thread of its own.
            let since = std::time::Instant::now();
            while references(&buffer) > 1 && since.elapsed() < std::time::Duration::from_secs(5) {
                std::thread::yield_now();
            }
            assert_eq!(references(&buffer), 1, "ours");
        }
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    #[test]
    fn dynamic_search_can_find_a_run_across_the_cursor() {
        let Some(heap) = HostHeap::new(false, 4) else {
            eprintln!("no host descriptor heap; skipped");
            return;
        };
        let first = heap.dynamic(1).unwrap();
        let left = heap.dynamic(2).unwrap();
        let right = heap.dynamic(2).unwrap();
        let rest = heap.dynamic(heap.dynamic - 5).unwrap();
        assert_eq!((first, left, right, rest), (4, 5, 7, 9));

        // Reallocating the left half puts the cursor back in the middle.
        heap.release(left, 2);
        assert_eq!(heap.dynamic(2), Some(left));
        heap.release(left, 2);
        heap.release(right, 2);
        assert_eq!(heap.dynamic(4), Some(left));
        assert_eq!(heap.dynamic(1), None);

        // A free slot at each physical end does not form a two-slot range.
        heap.release(first, 1);
        let last = first + heap.dynamic - 1;
        heap.release(last, 1);
        assert_eq!(heap.dynamic(2), None);
    }

    /// A texture view's size words are the console image descriptor's
    /// fields: level 0's size of the resource, the view's levels, its type.
    #[test]
    fn a_texture_view_gives_its_image_descriptor_size_fields() {
        // A 1000x600 2D texture of 10 levels, then an array of 6 with the
        // whole chain (mip count 0, 1024 wide: 11 levels).
        let mut texture = [0u32; 14];
        (texture[0], texture[4], texture[6], texture[7], texture[9]) = (3, 1000, 600, 1 | 10 << 16, 1);
        let mut array = texture;
        (array[4], array[7]) = (1024, 6);
        let mut view = [0u32; 10];
        // Levels 2-4 of the texture: width - 1 = 999 (0b11_1110_0111).
        (view[1], view[4], view[5]) = (4, 2, 3);
        let words = image_words(&texture, Some(&view), false);
        assert_eq!(words, [0, 3 << 30, 249 | 599 << 14, 0xFAC | 2 << 12 | 4 << 16 | 9 << 28]);
        // Its whole chain from level 1.
        (view[4], view[5]) = (1, u32::MAX);
        assert_eq!(image_words(&texture, Some(&view), false)[3], 0xFAC | 1 << 12 | 9 << 16 | 9 << 28);
        // An unordered view of level 3: that level alone.
        let uav = [0, 4, 0, 0, 3, 0, 0, 0, 0, 0];
        assert_eq!(image_words(&texture, Some(&uav), true)[3], 0xFAC | 3 << 12 | 3 << 16 | 9 << 28);
        // The array's own view: a 2D array, levels 0-10.
        let words = image_words(&array, None, false);
        assert_eq!(words[1..], [3 << 30, 255 | 599 << 14, 0xFAC | 10 << 16 | 13 << 28]);
    }
}

#[cfg(test)]
mod record_print_tests {
    use super::record_print;

    /// Equal records print alike; a record that differs in any one byte,
    /// or in its length, prints otherwise.
    #[test]
    fn a_record_that_changed_prints_otherwise() {
        let record: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(37).wrapping_add(5)).collect();
        let print = record_print(&record);
        assert_eq!(print, record_print(&record.clone()));
        for at in 0..32 {
            for bit in 0..8 {
                let mut other = record.clone();
                other[at] ^= 1 << bit;
                assert_ne!(record_print(&other), print, "byte {at} bit {bit}");
            }
        }
        assert_ne!(record_print(&record[..16]), print);
        assert_ne!(record_print(&[0u8; 32]), record_print(&[0u8; 16]));
        // A record of zeros is told from "never looked at" by the caller's `| 1`.
        assert_ne!(record_print(&[0u8; 32]) | 1, 0);
    }
}

#[cfg(test)]
mod copied_record_tests {
    use super::*;
    use crate::descriptor::{self, HeapDesc, Placement};
    use crate::object::{S_OK, This};

    // Registered heaps exercise Create*View -> descriptor::mirror as well
    // as both copied-record entry points. No command lists are submitted.
    struct HeapFixture(This);

    impl HeapFixture {
        fn new(sampler: bool) -> Self {
            #[repr(C)]
            struct Extended(HeapDesc, Placement);
            let desc = Extended(
                HeapDesc {
                    kind: u32::from(sampler),
                    count: 4,
                    flags: 1,
                    nodes: 1,
                },
                Placement {
                    address: 0,
                    size: 0,
                },
            );
            let mut heap = std::ptr::null();
            assert_eq!(
                descriptor::create_heap(
                    std::ptr::null(),
                    &desc.0,
                    &descriptor::HEAP_IIDS[0],
                    &mut heap
                ),
                S_OK
            );
            let fixture = Self(heap);
            fixture.host(); // Require a host adapter (WARP is sufficient).
            fixture
        }

        fn host(&self) -> &HostHeap {
            descriptor::heap(self.0)
                .host()
                .expect("copied-record tests require a D3D12 adapter")
        }

        fn handle(&self, index: u32) -> u64 {
            descriptor::heap(self.0).handle(index).unwrap()
        }

        fn write(&self, index: u32, record: &[u8]) {
            // SAFETY: fixture records fit their allocated descriptor slots.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    record.as_ptr(),
                    self.handle(index) as *mut u8,
                    record.len(),
                )
            };
        }

        fn sync(&self, page: bool, index: u32, record: &[u8]) -> bool {
            let handle = self.handle(index);
            if page {
                let step = if self.host().sampler { 16 } else { 32 };
                let (looked, made) = self.host().mirror_copied_page(
                    u64::from(index)..u64::from(index) + 1,
                    (descriptor::heap(self.0).start, step),
                    handle as usize,
                    record,
                );
                assert_eq!(looked, 1);
                made != 0
            } else {
                self.host().mirror_copied(index, record)
            }
        }

        fn key(&self, index: u32) -> u64 {
            self.host().made.lock().unwrap()[index as usize]
        }
    }

    impl Drop for HeapFixture {
        fn drop(&mut self) {
            descriptor::release(self.0);
        }
    }

    fn bytes<const N: usize>(words: [u32; N]) -> Vec<u8> {
        words.into_iter().flat_map(u32::to_le_bytes).collect()
    }

    fn key(record: &[u8]) -> u64 {
        let i = u32::from_le_bytes(record[4..8].try_into().unwrap());
        let generation = u32::from_le_bytes(record[8..12].try_into().unwrap());
        u64::from(generation) << 32 | u64::from(i)
    }

    const SAMPLER_A: [u32; 13] = [21, 3, 3, 3, 0, 1, 0, 0, 0, 0, 0, 0x7f7f_ffff, 0];
    const SAMPLER_B: [u32; 13] = [21, 1, 1, 1, 0, 1, 0, 0, 0, 0, 0, 0x7f7f_ffff, 0];

    #[test]
    fn copied_records_follow_api_writes_and_zeroing() {
        let mut source = [0u32; 4];
        descriptor::create_sampler(std::ptr::null(), &SAMPLER_A, source.as_mut_ptr() as u64);
        let a = bytes(source);
        for page in [false, true] {
            let heap = HeapFixture::new(true);
            // raw A -> API B -> raw A must install A, not deduplicate it.
            heap.write(0, &a);
            assert!(heap.sync(page, 0, &a));
            assert_eq!(heap.key(0), key(&a));
            heap.host().mirror(0, heap.handle(0), true);
            assert!(
                !heap.sync(page, 0, &a),
                "unchanged installed record stays fast"
            );
            descriptor::create_sampler(std::ptr::null(), &SAMPLER_B, heap.handle(0));
            assert_ne!(heap.key(0), key(&a));
            heap.write(0, &a);
            assert!(heap.sync(page, 0, &a));
            assert_eq!(heap.key(0), key(&a));

            // API-only population has no copied fingerprint yet.
            descriptor::create_sampler(std::ptr::null(), &SAMPLER_A, heap.handle(1));
            assert_eq!(heap.key(1), key(&a));
            let zero = [0u8; 16];
            heap.write(1, &zero);
            assert!(heap.sync(page, 1, &zero));
            assert_eq!(heap.key(1), 0);
            assert!(!heap.sync(page, 1, &zero));
        }
    }

    #[test]
    fn copied_records_install_the_fingerprinted_snapshot() {
        let mut source = [0u32; 4];
        descriptor::create_sampler(std::ptr::null(), &SAMPLER_A, source.as_mut_ptr() as u64);
        let a = bytes(source);
        descriptor::create_sampler(std::ptr::null(), &SAMPLER_B, source.as_mut_ptr() as u64);
        let b = bytes(source);
        assert_ne!(key(&a), key(&b));
        for page in [false, true] {
            let heap = HeapFixture::new(true);
            heap.write(0, &b); // The live slot changed after A was captured.
            assert!(heap.sync(page, 0, &a));
            assert_eq!(
                heap.key(0),
                key(&a),
                "do not install live B while caching A"
            );
            heap.write(0, &a);
            assert!(!heap.sync(page, 0, &a));
            assert_eq!(heap.key(0), key(&a));
            heap.write(0, &b);
            assert!(heap.sync(page, 0, &b));
            assert_eq!(heap.key(0), key(&b));
        }
    }

    #[test]
    fn copied_records_repair_repurposed_slots_and_unmade_views() {
        let null = std::ptr::null();
        // Distinct synthetic null texture SRVs; no title data is needed.
        let texture_a = [28, 4, 5768, 0, 0, 1, 0, 0, 0, 0];
        let texture_b = [28, 4, 5768, 0, 0, 2, 0, 0, 0, 0];
        let mut source = [0u32; 8];
        descriptor::create_srv(
            null,
            std::ptr::null_mut(),
            &texture_a,
            source.as_mut_ptr() as u64,
        );
        let a = bytes(source);
        descriptor::create_srv(
            null,
            std::ptr::null_mut(),
            &texture_b,
            source.as_mut_ptr() as u64,
        );
        let b = bytes(source);
        let resource = [1, 0, 0, 0, 256, 0, 1, 0x1_0001, 0, 1, 0, 1, 0, 0];
        let buffer_desc = [0, 1, 5768, 0, 0, 0, 4, 16, 0, 0, 0, 0x6f, 0, 0, 0, 0];
        descriptor::create_placed_srv(null, &resource, &buffer_desc, source.as_mut_ptr() as u64);
        let buffer = bytes(source);
        for page in [false, true] {
            let heap = HeapFixture::new(false);
            let host = heap.host();
            let stride = |index: usize| {
                let (_, _, data) = host.strides.as_ref().expect("stride table");
                // SAFETY: the fixture owns a mapped stride table with four slots.
                unsafe { (*data as *const u32).add(index).read_volatile() }
            };
            descriptor::create_srv(null, std::ptr::null_mut(), &texture_a, heap.handle(0));
            heap.write(0, &buffer);
            assert!(heap.sync(page, 0, &buffer));
            assert_eq!(stride(0), 16);
            assert_eq!(heap.key(0), key(&buffer), "buffer records are installed for bindless reads");
            assert!(!heap.sync(page, 0, &buffer));
            // Zeroing must clear the installed record and its buffer bookkeeping.
            let zero = [0u8; 32];
            heap.write(0, &zero);
            assert!(heap.sync(page, 0, &zero));
            assert_eq!(stride(0), 0);

            // Replacement textures must be installed even when no API mirror runs.
            for index in [1, 2] {
                descriptor::create_srv(null, std::ptr::null_mut(), &texture_a, heap.handle(index));
                heap.write(index, &b);
                assert!(heap.sync(page, index, &b));
                assert_eq!(heap.key(index), key(&b));
                assert!(!heap.sync(page, index, &b));
            }

            // Identical bytes are skipped only after their first synchronization.
            descriptor::create_srv(null, std::ptr::null_mut(), &texture_a, heap.handle(3));
            assert!(heap.sync(page, 3, &a));
            assert_eq!(heap.key(3), key(&a));
            assert!(!heap.sync(page, 3, &a));
            // A texture made null by invalidation must be remade even when
            // the next captured bytes equal its old fingerprint.
            {
                let mut made = host.made.lock().unwrap();
                host.unmake(&mut made, 3);
            }
            assert_eq!(heap.key(3), 0);
            assert!(heap.sync(page, 3, &a));
            assert_eq!(heap.key(3), key(&a));
            assert!(!heap.sync(page, 3, &a));
        }
    }
}
