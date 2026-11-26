//! IR to DXIL, one machine instruction at a time (decision 0003).
//!
//! Every register dword is an `i32` (SCC an `i1`); float operations bitcast
//! their operands. Lane masks exist only as the per-lane booleans of
//! structurisation; branches and guards are built from them. A value the
//! emitter cannot express is `Missing` with a reason, and only fails the
//! program when something with an effect (an export, a branch) needs it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::UserData;
use super::bitcode::{Attr, Bin, Cast, Rmw, Tag, Term, V, pred};
use super::container::SigElement;
use super::shader::{Shader, VIEW_INTEGER, VIEW_SIGNED, VIEW_STRIDE_BITS, VIEW_STRIDES, flags};
use crate::ShaderError;
use crate::gfx10::{Dpp, Fields, NULL, Operand};
use crate::io::{Barycentric, Elsewhere, Export, Input, Interp, Io, Target};
use crate::ir::{Block, EXEC, Exit, Inst, Phi, Program, Reg, Value};
use crate::native::{HwStage, NativeProgram, ResourceType, RootType, StageIo, Usage, UsedResource};
use crate::resource::{Address, CODE, MEMORY, Resources, Role, Site, Source, USER_DATA};
use crate::structure::{Atom, Bool, LaneExit, Structured};

mod bvh;
#[path = "guarded.rs"]
mod guarded;
mod paired;
mod wave_lower;
pub(super) use paired::with_paired_lanes;
use paired::{LaneState, PAIRED, lane_scalars};
mod wide_loads;
#[cfg(test)]
pub(crate) use wide_loads::TEST_WIDE_LOADS;
#[cfg(test)]
#[path = "wide_load_tests.rs"]
mod wide_load_tests;
pub(super) use wave_lower::*;
#[path = "code_table.rs"]
mod code_table;
#[path = "halves.rs"]
mod halves;
use code_table::start as code_table_start;
#[cfg(test)]
#[path = "annotation_cache_tests.rs"]
mod annotation_cache_tests;
#[cfg(test)]
#[path = "descriptor_kind_tests.rs"]
mod descriptor_kind_tests;
#[cfg(test)]
#[path = "first_half_tests.rs"]
mod first_half_tests;
#[cfg(test)]
#[path = "loop_cap_tests.rs"]
mod loop_cap_tests;
#[cfg(test)]
#[path = "mesh_prologue_tests.rs"]
mod mesh_prologue_tests;

/// The refusal of a first half of an interpolation whose result is read
/// as data ([`Lower::interp`]).
pub(super) const FIRST_HALF: &str = "the first half of an interpolation";

thread_local! {
    /// Whether first halves of interpolations are evaluated as the pair
    /// they start ([`with_first_half_pairs`]).
    static FIRST_HALF_PAIRS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `translate` with first halves of interpolations evaluated as the
/// pair they start: the translation of a program refused with
/// [`FIRST_HALF`] (`VICTORY_FIRST_HALF=0`: refused again, as before).
pub(super) fn with_first_half_pairs<R>(translate: impl FnOnce() -> R) -> R {
    let before = FIRST_HALF_PAIRS.replace(true);
    let result = translate();
    FIRST_HALF_PAIRS.set(before);
    result
}

/// Host capability retained in shader cache identity; lowering uses upstream wave semantics.
static HOST_MAX_WAVE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn set_host_max_wave(lanes: u32) {
    HOST_MAX_WAVE.store(lanes, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_HOST_WAVE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

pub(super) fn host_max_wave() -> u32 {
    #[cfg(test)]
    if let Some(lanes) = TEST_HOST_WAVE.get() {
        return lanes;
    }
    HOST_MAX_WAVE.load(std::sync::atomic::Ordering::Relaxed)
}

type Why = &'static str;

/// How an interpolation evaluates an input channel.
#[derive(Debug, Clone, Copy)]
enum Eval {
    /// Loaded with this DXIL interpolation mode (1: constant, P0).
    Mode(u8),
    /// From the vertex values of a constant element: P0 when `p0`, plus
    /// i (P1 - P0), plus j (P2 - P0), weights as floats.
    Weights {
        p0: bool,
        i: Option<V>,
        j: Option<V>,
    },
}

#[derive(Debug, Clone, Copy)]
enum Val {
    I32(V),
    Bit(V),
    Missing(Why),
}

pub(crate) struct Lowered {
    pub dxil: Vec<u8>,
    pub wave_size: Option<u32>,
    /// Coherent fixed table slots requiring unsigned half views.
    pub half_views: Vec<(u8, u32)>,
    /// Bit n: immediate sampler n is a comparison sampler.
    pub cmp_samplers: u64,
    pub root_cbvs: Vec<u8>,
    pub root_srvs: Vec<u8>,
    pub root_uavs: Vec<u8>,
    /// Reads title memory at computed addresses (the page table).
    pub memory: bool,
    /// Writes there (the written-pages bitmap).
    pub memory_writes: bool,
    /// Reads buffer views' addresses or texture views' sizes from
    /// descriptors (the view addresses).
    pub view_addresses: bool,
    /// Takes descriptors from the resource heap itself (root type 5, not
    /// a table), and whether as unordered access views.
    pub heap: bool,
    pub heap_writes: bool,
    /// Marks the pages it writes through buffer views (`emit`, "Write
    /// marks").
    pub view_marks: bool,
    pub user_data: Vec<UserData>,
    pub samplers: Vec<[u32; 4]>,
    /// Heap handles made with a non-uniform index.
    #[cfg(test)]
    pub nonuniform_handles: u32,
    /// Heap handles written.
    #[cfg(test)]
    pub heap_handles: u32,
    /// Whether the program uses wave operations.
    #[cfg(test)]
    pub wave_ops: bool,
    /// Whether it declares rasterizer-ordered views.
    #[cfg(all(test, windows))]
    pub rovs: bool,
}

pub(crate) struct Parts<'a> {
    pub native: &'a NativeProgram,
    pub p: &'a Program,
    pub st: &'a Structured,
    pub io: &'a Io,
    pub res: &'a Resources,
    pub inputs: Vec<SigElement>,
    pub outputs: Vec<SigElement>,
    pub prims: Vec<SigElement>,
    /// Mesh: what the group id arrives multiplied by.
    pub mesh_stride: u32,
    /// Writes through buffer views mark their pages (`emit`, "Write marks").
    pub view_marks: bool,
}

enum Stop {
    /// A phi whose incoming value has no DXIL form was read: redo with it missing.
    Retry(Value, Why),
    Fail(ShaderError),
}

pub(crate) fn lower(parts: &Parts) -> Result<Lowered, ShaderError> {
    let mut bad = HashMap::new();
    loop {
        match Lower::new(parts, &bad).run() {
            Ok(l) => return Ok(l),
            Err(Stop::Retry(v, why)) => {
                if bad.insert(v, why).is_some() {
                    return Err(ShaderError::Unsupported {
                        offset: 0,
                        what: "a phi that does not settle",
                    });
                }
            }
            Err(Stop::Fail(e)) => return Err(e),
        }
    }
}

const TWO_PI: f32 = std::f32::consts::TAU;
/// `s_sendmsg` id of a mesh or NGG stage's output allocation request.
const GS_ALLOC_REQ: u16 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Numeric {
    Unorm,
    Snorm,
    Uscaled,
    Sscaled,
    Uint,
    Sint,
    Float,
}

/// A GFX10 buffer format (the unified format of typed buffer loads): the
/// channel widths from the lowest bits up, and how the channels convert.
/// Formats are numbered in families (`8`, `16`, `8_8`, ...: AMD names
/// list channels from the highest bits), each with its numeric formats in
/// the order unorm, snorm, uscaled, sscaled, uint, sint, float.
fn buffer_format(format: u8) -> Option<(&'static [u32], Numeric)> {
    use Numeric::*;
    const NORM: &[Numeric] = &[Unorm, Snorm, Uscaled, Sscaled, Uint, Sint];
    const ALL: &[Numeric] = &[Unorm, Snorm, Uscaled, Sscaled, Uint, Sint, Float];
    const WIDE: &[Numeric] = &[Uint, Sint, Float];
    // Numbers and order from LLVM's `UfmtGFX10` (`SIDefines.h`); the GFX10
    // buffer formats have no 5_9_9_9. Channel order from Mesa
    // (`ac_formats.c`): R11G11B10_FLOAT is 10_11_11, and 10_10_10_2 has
    // its 2-bit channel in the lowest bits.
    const FAMILIES: [(&[u32], &[Numeric]); 14] = [
        (&[8], NORM),
        (&[16], ALL),
        (&[8, 8], NORM),
        (&[32], WIDE),
        (&[16, 16], ALL),
        (&[11, 11, 10], ALL),     // 10_11_11
        (&[10, 11, 11], ALL),     // 11_11_10
        (&[2, 10, 10, 10], NORM), // 10_10_10_2
        (&[10, 10, 10, 2], NORM), // 2_10_10_10
        (&[8, 8, 8, 8], NORM),
        (&[32, 32], WIDE),
        (&[16, 16, 16, 16], ALL),
        (&[32, 32, 32], WIDE),
        (&[32, 32, 32, 32], WIDE),
    ];
    let mut n = usize::from(format.checked_sub(1)?);
    for (bits, numerics) in FAMILIES {
        if n < numerics.len() {
            return Some((bits, numerics[n]));
        }
        n -= numerics.len();
    }
    None
}

/// How far a float channel of `width` bits (16, or the unsigned 11- and
/// 10-bit floats of the packed formats) shifts up to be a half:
/// the small floats are a half's exponent (5 bits, biased 15) and the top
/// of its mantissa, without the sign, so `legacyF16ToF32` widens them
/// exactly, denormals, infinities and NaNs included.
fn half_shift(width: u32) -> u32 {
    debug_assert!(matches!(width, 10 | 11 | 16), "{width}-bit float");
    if width == 16 { 0 } else { 15 - width }
}

/// A descriptor load's address given by root parameters alone: root
/// parameter and byte offset.
type EntryAddress = (usize, u32);

struct Lower<'a> {
    raw: guarded::Raws,
    raw_use: Option<(Bool, V)>,
    raw_meets: crate::structure::Meets,
    twin: Option<Box<LaneState>>,
    upper: bool,
    lds_last: Option<bool>,
    lane_scalars: HashSet<Value>,
    fence_only: HashSet<usize>,
    half_views: BTreeSet<(u8, u32)>,
    halves_only: BTreeSet<(u8, u32)>,
    cap_follows: HashMap<usize, usize>,
    s: Shader,
    x: &'a Parts<'a>,
    bad: &'a HashMap<Value, Why>,
    lane_fail: HashMap<Value, Why>,
    vals: HashMap<Value, Val>,
    floats: HashMap<Value, V>,
    /// Values (missing in `vals`) whose low 16 bits are known: a 16-bit
    /// result written over a value with no DXIL form keeps its high half.
    low16: HashMap<Value, V>,
    /// DXIL block per IR block (unreachable: none).
    block: Vec<Option<usize>>,
    cur: usize,
    bools: HashMap<(usize, Bool), V>,
    lane_bits: HashMap<Value, V>,
    /// IR phi location by value.
    phi_at: HashMap<Value, (usize, usize)>,
    mask_phis: HashMap<Value, V>,
    pending_masks: Vec<(V, usize, usize)>,
    data_phis: Vec<(V, usize, usize)>,
    read_phis: HashSet<Value>,
    params: HashMap<Value, (usize, u32)>,
    user_data: Option<V>,
    user_reads: BTreeMap<u32, UserData>,
    /// Handles of root SRVs and UAVs read as bounded views, by user-data
    /// dword ([`Lower::root_view`]).
    root_views: HashMap<u32, V>,
    samplers: Vec<[u32; 4]>,
    descriptors: HashMap<(Site, bool), &'a Source>,
    scalar: HashMap<Site, Address>,
    exports: HashMap<Site, &'a Export>,
    interps: HashMap<Site, &'a Interp>,
    // Annotated handles include both resource kind and component/stride data.
    handles: HashMap<(usize, Site, u8, bool, (u32, u32)), V>,
    strides: HashMap<(usize, Site, u8), V>,
    /// Handles made in the entry block for descriptors at addresses the
    /// root parameters alone give (table and heap loads with no other
    /// term), by base value, byte offset, dword, offset register, sampler
    /// heap and properties ([`Lower::entry_address`]).
    entry_handles: HashMap<(EntryAddress, u8, bool, (u32, u32)), V>,
    /// Handles of descriptors chosen by control flow, made in their join
    /// block, by the choice (its `Debug` text), sampler heap and properties.
    join_handles: HashMap<(String, bool, (u32, u32)), V>,
    /// Blocks splitting the edge from one IR block to another.
    edges: HashMap<(usize, usize), usize>,
    /// Per IR block: a `done` export is behind every lane entering it.
    exported: Vec<bool>,
    /// Reverse-postorder index per IR block.
    rpo: Vec<usize>,
    /// DXIL block each IR block ends in (guarded stores split blocks).
    tail: Vec<Option<usize>>,
    /// The program writes UAVs: barriers fence them too.
    writes_uavs: bool,
    /// [`Lowered::heap`], [`Lowered::heap_writes`].
    heap: [bool; 2],
    /// src0 of the current instruction as DPP fetched it.
    src0: Option<V>,
    /// Compute code that reads other lanes: thread ids follow the host
    /// lanes, so lane n is console lane n.
    console_lanes: bool,
    /// Pixel: an ordered section (POPS), whose UAVs the host makes
    /// rasterizer-ordered views.
    pops: bool,
    /// The program uses title memory (addresses it computes, the BVH
    /// instruction, root parameters through the user-data pointer): its root
    /// SRVs and UAVs are read there too ([`Lower::raw_view`]).
    root_memory: bool,
    /// Mesh: the thread index in the group (made in the entry block).
    flat: Option<V>,
    /// Mesh: IR blocks whose `s_barrier` is left out.
    no_barrier: HashSet<usize>,
    /// A group barrier before every LDS access (a console wave run as two host waves).
    split_lds: bool,
    /// With `split_lds`, LDS accesses in lane branches: per IR block whether every lane runs
    /// it, and whether the host waves meet at its start and before its exit ([`lds_meetings`]).
    lds_meet: Option<(Vec<bool>, Vec<(bool, bool)>)>,
    /// A console wave64 program run as two host waves of 32 that exchange through LDS: the first
    /// of two exchange dwords, whether the group has no upper half (32 threads or fewer), and per
    /// IR block whether every lane runs it (exchanges need both halves there).
    exchange: Option<(u32, bool, Vec<bool>)>,
    /// With `exchange`: nothing is exchanged, each host wave acts as a wave
    /// of its own ([`with_local_waves`]).
    local: bool,
    /// The instruction being lowered may read a lane mask as a number under
    /// `local` (a bit count, a first set bit, `v_mbcnt`).
    local_ok: bool,
    /// With `local`: every mask read and lane read is taken per host wave
    /// ([`with_local_waves_any`]).
    local_any: bool,
    /// With `exchange`: VGPR values every lane holds alike ([`lane_uniform`]); a first-lane
    /// read of one needs nothing from the other host wave.
    uniform: HashSet<Value>,
    /// With `exchange`: the wave ORs that only steer a loop over each lane's own bits, which
    /// a host wave takes over its own lanes wherever they stand ([`super::own_bits`]).
    own_bits: super::own_bits::OwnBits,
    /// The console lane (0..63) of this thread, made in the entry block.
    clane: Option<V>,
    /// The first exchange dword of this thread's console wave ([`Self::exchange_pair`]).
    pair: Option<V>,
    /// A console wave64 program in host waves of 32: the LDS dword counting the console threads
    /// handed to host waves (`wave_base`), and the base made in the entry block.
    halves: Option<u32>,
    wave_base: Option<V>,

    /// Vertex and mesh outputs holding 16-bit values: row, whether two per
    /// dword ([`packed_half`]).
    half_outputs: HashMap<u8, bool>,
    /// How lane masks become data.
    mask_data: MaskData,
    /// The instruction being lowered may read lane masks as data.
    mask_ok: bool,
    /// Ballots of masks read as data, by low dword: (low, high).
    ballots: HashMap<Value, (V, V)>,
    /// Masks whose ballot is being built: a mask whose lane bit needs its
    /// own value as a number (an opaque mask keyed by a dword it shares
    /// with another) would otherwise recurse until the stack overflows.
    balloting: HashSet<Value>,
    /// Block writing each value.
    def_block: HashMap<Value, usize>,
    /// IR block being lowered.
    cur_ir: usize,
    /// Whole-wave sections by block and start index.
    sections: HashMap<usize, HashMap<usize, super::wave::Section>>,
    /// Values written in a section that code outside it reads.
    section_out: HashSet<Value>,
    /// The program's constant table (the code from the first byte read
    /// through `s_getpc_b64` on): global, first byte, dwords.
    code_table: Option<(V, u32, u32)>,
    /// Loop headers' iteration counters (see [`LOOP_LIMIT`]): the phi by
    /// IR block, and its incoming (value, DXIL block) pairs.
    counters: HashMap<usize, (V, Vec<(V, usize)>)>,
    /// The block a loop stopped by [`LOOP_LIMIT`] leaves the program from.
    abort: Option<usize>,
    /// The 16/64-bit round mode (MODE bits 3:2) each block starts with, and
    /// the one in force at the instruction being lowered (see [`round16`]).
    round16: Vec<Option<u8>>,
    mode16: Option<u8>,
}

/// The 16/64-bit round mode (MODE bits 3:2: 0 nearest even, 3 toward zero)
/// after `inst`, from `mode` before it (`None`: not known).
fn round16_after(inst: &Inst, mode: Option<u8>) -> Option<u8> {
    match (inst.name(), &inst.fields) {
        ("s_round_mode", Fields::Sopp { imm }) => Some((imm >> 2 & 3) as u8),
        (
            "s_setreg_imm32_b32" | "s_setreg_b32",
            Fields::Sop {
                src, imm: Some(h), ..
            },
        ) => {
            let (id, off, size) = (h & 0x3F, h >> 6 & 0x1F, (h >> 11 & 0x1F) + 1);
            if id != 1 || off >= 4 || off + size <= 2 {
                return mode;
            }
            match src.first() {
                Some(&Operand::Literal(x)) if off <= 2 && off + size >= 4 => {
                    Some((x >> (2 - off) & 3) as u8)
                }
                _ => None,
            }
        }
        _ => mode,
    }
}

/// The 16/64-bit round mode each block starts with: nearest even at the
/// program's start, then as the MODE writes on the way set it; `None` where
/// paths that set it differently join (or a write is not a constant).
fn round16(p: &crate::ir::Program) -> Vec<Option<u8>> {
    // None: not reached yet; Some(m): the mode (m None: not known).
    let mut entry: Vec<Option<Option<u8>>> = vec![None; p.blocks.len()];
    entry[0] = Some(Some(0));
    let order = crate::ir::reverse_postorder(&p.blocks);
    loop {
        let mut changed = false;
        for &b in &order {
            let Some(mut m) = entry[b] else { continue };
            for inst in &p.blocks[b].insts {
                m = round16_after(inst, m);
            }
            for s in p.blocks[b].exit.successors() {
                let new = match entry[s] {
                    None => Some(m),
                    Some(old) if old == m => continue,
                    Some(_) => Some(None),
                };
                if entry[s] != new {
                    entry[s] = new;
                    changed = true;
                }
            }
        }
        if !changed {
            return entry.into_iter().map(Option::flatten).collect();
        }
    }
}

/// Iterations after which a loop is left (per entry into it), unless
/// [`super::loop_limit`] says otherwise. The console runs a loop as long as
/// its data says; data the host has not mirrored yet (a null view reads
/// zeros) can make that four billion iterations, which hangs the host GPU
/// until the driver resets it. Proven loop exits continue after the loop;
/// other capped lanes leave the program as killed (a pixel discards).
// shortcut: a genuine loop of more iterations than this is cut short too.
pub(super) const LOOP_LIMIT: u32 = 1 << 20;

/// Lane masks read as numbers (`s_bcnt1`, `s_ff1`, `v_mbcnt`, scalar
/// compares, bit tests) are ballots of the lanes' booleans, taken where the
/// mask is written: structurisation's `active` masks hold only lanes that
/// reach that block, so the ballot misses none.
/// A page table lookup ([`Lower::memory`]).
struct MemoryEntry {
    handle: V,
    offset: V,
    /// Block × 4096 + page in the block.
    entry: V,
    block: V,
    /// The level-1 index: address bits 28-39.
    l1: V,
    /// No host copy holds the page.
    miss: V,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskData {
    /// Compute: host lane n is console lane n (the program declares the
    /// console's wave size), so ballots are the console's masks bit for bit
    /// and any use is exact.
    Console,
    /// Pixel and vertex programs: ballots over the host's wave, whatever its
    /// size. Only uses that mean the same on any wave are allowed: counts,
    /// the first lane, the count below a lane, equality (the HLSL wave
    /// operations the console compiler expands). Lane numbers stay host
    /// lanes throughout (`v_mbcnt`, `v_readlane` at a computed lane).
    Host,
    /// Mesh programs: their lanes and waves (LDS per console wave) are the
    /// console's but no host wave size can be declared. Only a mask's bit
    /// count is taken, over the host's wave (the console compiler's test of
    /// a value for uniformity, `WaveActiveAllEqual`): lane numbers stay the
    /// console's, so the first lane and the count below a lane are refused.
    None,
}

/// What a value holding a lane mask read as a number stops with.
pub(super) const LANE_MASK: Why = "a lane mask read as data";
pub(super) const MASK_ELSEWHERE: Why = "a lane mask holding lanes elsewhere read as data";

/// Instructions that read a lane mask (not EXEC implicitly, not a lane's
/// bit) as a number.
pub(super) fn reads_mask_as_data(st: &Structured, i: &Inst) -> bool {
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    let ops: Vec<&Operand> = match &i.fields {
        // A mask operation (its result a mask) is no data use.
        Fields::Sop { .. }
            if i.writes
                .iter()
                .any(|&(r, w)| matches!(r, Reg::S(_)) && is_mask(w)) =>
        {
            return false;
        }
        Fields::Sop { src, .. } => src.iter().collect(),
        Fields::Valu(v)
            if !matches!(i.name(), "v_cndmask_b32") && !i.name().contains("_co_ci_") =>
        {
            v.src.iter().collect()
        }
        _ => return false,
    };
    ops.into_iter().any(|op| match *op {
        Operand::Scalar { code, .. } if code != NULL => i.read(Reg::S(code)).is_some_and(is_mask),
        _ => false,
    })
}

/// Where half `h` (dword h / 2, the high half when odd) of the parameter
/// whose first element is in row `a` comes from, when the console packs
/// 16-bit values two per dword: a row and channel of `elements`, none for
/// padding. The console fills its parameters with the 16-bit elements taken
/// by first channel, then row: each takes whole dwords of the first
/// parameter with room for it (four dwords each), its channels in order.
fn packed_half(elements: &[SigElement], a: u8, h: u8) -> Option<(u8, u8)> {
    let mut halves: Vec<&SigElement> = elements
        .iter()
        .filter(|e| e.half && e.kind == 0 && e.row.is_some())
        .collect();
    halves.sort_by_key(|e| (e.col, e.row));
    // (first element's row, (row, channel) per half)
    type Param = (Option<u32>, Vec<Option<(u8, u8)>>);
    let mut params: Vec<Param> = Vec::new();
    for e in halves {
        let need = usize::from(e.cols.div_ceil(2) * 2);
        let i = match params.iter().position(|p| p.1.len() + need <= 8) {
            Some(i) => i,
            None => {
                params.push((e.row, Vec::new()));
                params.len() - 1
            }
        };
        let row = u8::try_from(e.row?).ok()?;
        let slots = &mut params[i].1;
        slots.extend((0..e.cols).map(|c| Some((row, e.col + c))));
        slots.resize(slots.len().next_multiple_of(2), None);
    }
    // Two parameters starting in one row would leave the half ambiguous.
    let mut found = params.iter().filter(|p| p.0 == Some(u32::from(a)));
    match (found.next(), found.next()) {
        (Some(p), None) => p.1.get(usize::from(h)).copied().flatten(),
        _ => None,
    }
}

/// Vertex or mesh outputs of 16-bit values (see [`Lower::half_outputs`]),
/// by row: whether two per dword.
fn half_outputs(io: &StageIo) -> HashMap<u8, bool> {
    let entries = match io {
        StageIo::Vs { outputs, .. } => outputs,
        StageIo::Ms(ms) => &ms.outputs,
        _ => return HashMap::new(),
    };
    entries
        .iter()
        .filter(|o| o.param & 0x80 == 0 && o.fp16)
        .map(|o| (o.row, o.pack2))
        .collect()
}

fn unsupported(offset: usize, what: Why) -> Stop {
    Stop::Fail(ShaderError::Unsupported { offset, what })
}

/// The scalar instruction defining an SSA value in this block and its operands.
fn scalar_def<'a, const N: usize>(
    block: &'a Block,
    value: Value,
    op: &str,
) -> Option<(&'a Inst, [Operand; N])> {
    let inst = block
        .insts
        .iter()
        .find(|i| i.writes.iter().any(|w| w.1 == value))?;
    let Fields::Sop { src, .. } = &inst.fields else {
        return None;
    };
    (inst.name() == op).then_some((inst, src.as_slice().try_into().ok()?))
}

fn sgpr_operand(inst: &Inst, operand: Operand) -> Option<Value> {
    match operand {
        Operand::Scalar {
            code: code @ 0..=101,
            dwords: 1,
        } => inst.read(Reg::S(code)),
        _ => None,
    }
}

fn scalar_constant(operand: Operand) -> Option<u32> {
    match operand {
        Operand::Const(code) => crate::resource::inline(code),
        Operand::Literal(value) => Some(value),
        _ => None,
    }
}

/// The dwords an `s_load_dword*` reads.
fn smem_dwords(name: &str) -> Option<u32> {
    match name.strip_prefix("s_load_dword")? {
        "" => Some(1),
        "x2" => Some(2),
        "x4" => Some(4),
        "x8" => Some(8),
        "x16" => Some(16),
        _ => None,
    }
}

impl<'a> Lower<'a> {
    fn new(x: &'a Parts<'a>, bad: &'a HashMap<Value, Why>) -> Self {
        let p = x.p;
        let mut phi_at = HashMap::new();
        for (b, block) in p.blocks.iter().enumerate() {
            for (i, phi) in block.phis.iter().enumerate() {
                phi_at.insert(phi.value, (b, i));
            }
        }
        let mut s = Shader::new(x.native.stage == HwStage::Ps);
        s.inputs = x.inputs.clone();
        s.outputs = x.outputs.clone();
        let insts = || p.blocks.iter().flat_map(|b| &b.insts);
        let writes_uavs = insts().any(|i| {
            matches!(i.fields, Fields::Image { .. } | Fields::Buffer { .. })
                && (i.name().contains("store") || i.name().contains("atomic"))
        });
        let root_memory = x
            .res
            .scalar
            .iter()
            .any(|s| matches!(s.1.param, MEMORY | USER_DATA))
            || insts().any(|i| i.name().starts_with("image_bvh"));
        let mut console_lanes = false;
        // LDS accesses get a group barrier each (a console wave split in two host waves)
        let mut split_lds = false;
        // with `split_lds`, LDS accesses in lane branches: the blocks every lane runs, and
        // where the host waves meet around those branches (`lds_meetings`)
        let mut lds_meet: Option<(Vec<bool>, Vec<(bool, bool)>)> = None;
        // a console wave64 program in two host waves of 32 (`exchange`)
        let mut exchange: Option<(u32, bool, Vec<bool>)> = None;
        // a console wave64 program in host waves of 32: the LDS dword of `wave_base`'s counter
        let mut halves: Option<u32> = None;
        let paired = PAIRED.get() && x.native.wave64 && host_max_wave() == 32;
        if let StageIo::Cs(cs) = x.native.io
            && paired
        {
            // (`with_paired_lanes`) a host wave of 32 for each console wave of the group, its
            // threads in a row; the caller has checked that the console's waves are full
            let waves = cs.threads.iter().product::<u32>().div_ceil(64);
            s.threads = Some([32 * waves, 1, 1]);
            s.lds_dwords = cs.lds_bytes() / 4;
            if waves > 1 {
                // `wave_base`'s counter: a dword past the program's LDS, or the program's
                // first where it takes all a host group has (32 KiB)
                let slot = if s.lds_dwords < 8192 { s.lds_dwords } else { 0 };
                s.lds_dwords = s.lds_dwords.max(slot + 1);
                halves = Some(slot);
            }
            console_lanes = true;
            s.wave_size = Some(32);
        } else if let StageIo::Cs(cs) = x.native.io {
            s.threads = Some(cs.threads);
            s.lds_dwords = cs.lds_bytes() / 4;
            // Code reading other lanes needs the console's wave size and
            // lane order; so do lane masks read as numbers, and LDS shared
            // within one wave, where the console compiler leaves out barriers.
            console_lanes = insts().any(|i| {
                let n = i.name();
                n.starts_with("ds_swizzle")
                    || n.contains("permute")
                    || n.starts_with("v_mbcnt")
                    || matches!(n, "v_readlane_b32" | "v_writelane_b32")
                    || matches!(&i.fields, Fields::Valu(v) if v.dpp.is_some())
                    || reads_mask_as_data(x.st, i)
            }) || lane_constant_masks(x.st)
                // (a branch the wave takes together asks the wave)
                || !x.st.wide.is_empty();
            let mut wave = if x.native.wave64 { 64 } else { 32 };
            let group = cs.threads.iter().product::<u32>();
            let one_wave = group <= wave;
            // A host whose waves are at most 32 lanes (NVIDIA) refuses `WaveSize(64)`. A console
            // wave64 program whose cross-lane code stays inside 32-lane halves – DPP rows,
            // `ds_swizzle`, `v_permlane16` / `v_permlanex16`, which the ISA confines there – runs
            // exactly in waves of 32: the thread relabelling makes each host wave one half of a
            // console wave (lanes 32k..32k+31). Not when it crosses halves (`v_mbcnt`,
            // `v_readlane`, `v_writelane`, `ds_(b)permute`, masks as numbers or lane-constant
            // masks), nor when LDS may be shared within the console wave without barriers (a
            // group over 32 threads with LDS): those still need 64-lane waves.
            if wave == 64
                && host_max_wave() == 32
                && (console_lanes || one_wave && s.lds_dwords > 0)
            {
                let crosses = insts().any(|i| {
                    let n = i.name();
                    n.starts_with("v_mbcnt")
                        || matches!(n, "v_readlane_b32" | "v_writelane_b32")
                        || n.starts_with("ds_permute")
                        || n.starts_with("ds_bpermute")
                        || reads_mask_as_data(x.st, i)
                }) || lane_constant_masks(x.st)
                    || !x.st.wide.is_empty();
                if !crosses && (s.lds_dwords == 0 || group <= 32) {
                    wave = 32;
                } else {
                    // (Groups of several console waves too: each console wave is two host
                    // waves, which exchange through their own pair of dwords.)
                    // LDS the console shares within its one wave without barriers: split into
                    // two waves of 32, a group barrier before every LDS access keeps their order
                    // – where every thread of the group gets there (no lane branch around it,
                    // no guard), else a barrier would hang the group
                    let lds = |i: &Inst| {
                        matches!(i.fields, Fields::Ds { .. })
                            && !(i.name().starts_with("ds_swizzle") || i.name().contains("permute"))
                    };
                    if let Some(all) = all_lane_blocks(x.st) {
                        let ok = p.blocks.iter().enumerate().all(|(b, blk)| {
                            blk.insts
                                .iter()
                                .all(|inst| !lds(inst) || all.get(b).copied().unwrap_or(false))
                        });
                        let mut lds_ok = !(s.lds_dwords > 0 && group > 32) || ok;
                        if !lds_ok && let Some(meet) = lds_meetings(p, x.st, &all) {
                            lds_meet = Some((all.clone(), meet));
                            lds_ok = true;
                        }
                        if !lds_ok {
                            // (the blocks whose LDS accesses not every lane runs: with the
                            // branches around them taken wave-wide they get their barriers)
                            for (b, blk) in p.blocks.iter().enumerate() {
                                if !all.get(b).copied().unwrap_or(false)
                                    && blk.insts.iter().any(lds)
                                {
                                    need(b);
                                }
                            }
                        }
                        if lds_ok {
                            wave = 32;
                            split_lds = s.lds_dwords > 0 && group > 32;
                            if crosses {
                                // lanes 32..63 are the other host wave: masks, lane reads and lane
                                // numbers go through two exchange dwords past the program's LDS
                                exchange = Some((s.lds_dwords, group <= 32, all));
                                s.lds_dwords += 2 * group.div_ceil(64);
                            }
                        }
                    }
                }
                // `VICTORY_WAVE_WHY=1`: why a program keeps 64-lane waves (one line per reason)
                if wave == 64 && std::env::var_os("VICTORY_WAVE_WHY").is_some() {
                    let mut why: Vec<&str> = Vec::new();
                    for i in insts() {
                        let n = i.name();
                        let r = if n.starts_with("v_mbcnt") {
                            "v_mbcnt"
                        } else if n == "v_readlane_b32" {
                            "v_readlane"
                        } else if n == "v_writelane_b32" {
                            "v_writelane"
                        } else if n.starts_with("ds_permute") || n.starts_with("ds_bpermute") {
                            "ds_(b)permute"
                        } else if reads_mask_as_data(x.st, i) {
                            "mask as data"
                        } else {
                            continue;
                        };
                        if !why.contains(&r) {
                            why.push(r);
                        }
                    }
                    if lane_constant_masks(x.st) {
                        why.push("lane-constant mask");
                    }
                    if s.lds_dwords > 0 && group > 32 {
                        why.push("LDS in a group over 32");
                        match all_lane_blocks(x.st) {
                            None => why.push("an indirect exit"),
                            Some(all) if lds_meetings(p, x.st, &all).is_none() => {
                                why.push("an LDS read and write in one lane-branch region")
                            }
                            Some(_) => {}
                        }
                    }
                    why.sort_unstable();
                    eprintln!(
                        "WAVE64-WHY group={group} lds={} [{}]",
                        s.lds_dwords > 0,
                        why.join(", ")
                    );
                }
            }
            if wave == 32 && x.native.wave64 {
                // console threads go to host waves in the order the waves take them (`wave_base`)
                halves = Some(s.lds_dwords);
                s.lds_dwords += 1;
            }
            if console_lanes || one_wave && s.lds_dwords > 0 {
                s.wave_size = Some(wave);
            }
        }
        let fence_only: HashSet<usize> = if insts().any(|i| i.name() == "s_barrier") {
            let order = crate::ir::reverse_postorder(&p.blocks);
            let mut place = vec![usize::MAX; p.blocks.len()];
            for (n, &b) in order.iter().enumerate() {
                place[b] = n;
            }
            let back = |from: usize, to: usize| {
                let at = |b: usize| place.get(b).copied().unwrap_or(usize::MAX);
                at(from) != usize::MAX && at(to) <= at(from)
            };
            let cut_short =
                x.st.exits
                    .iter()
                    .enumerate()
                    .any(|(b, e)| matches!(*e, LaneExit::Jump(t) if back(b, t)));
            let all = all_lane_blocks(x.st);
            (0..p.blocks.len())
                .filter(|&b| p.blocks[b].insts.iter().any(|i| i.name() == "s_barrier"))
                .filter(|&b| {
                    cut_short
                        || all
                            .as_ref()
                            .is_none_or(|all| !all.get(b).copied().unwrap_or(false))
                })
                .collect()
        } else {
            HashSet::new()
        };
        let mut no_barrier = HashSet::new();
        if let StageIo::Ms(ms) = &x.native.io {
            s.prims = x.prims.clone();
            s.threads = Some([ms.threads, 1, 1]);
            s.lds_dwords = ms.lds_bytes.div_ceil(4);
            s.mesh = Some(super::shader::Mesh {
                max_vertices: ms.max_vertices,
                max_primitives: ms.max_primitives,
                topology: ms.topology,
            });
            // Barriers order the lanes' LDS accesses; the console's
            // allocation request needs them too, but the host does not. One
            // lane sends that request, and what it dominates (the path for
            // counts out of range) runs in that lane alone, so its barrier
            // would hang the host.
            let order = crate::ir::reverse_postorder(&p.blocks);
            let mut index = vec![usize::MAX; p.blocks.len()];
            for (n, &b) in order.iter().enumerate() {
                index[b] = n;
            }
            let idom = crate::ir::idoms(&p.blocks, &order, &index);
            let alloc: Vec<usize> = (0..p.blocks.len())
                .filter(|&b| {
                    p.blocks[b].insts.iter().any(|i| {
                        i.name() == "s_sendmsg"
                            && matches!(i.fields, Fields::Sopp { imm } if imm & 0xF == GS_ALLOC_REQ)
                    })
                })
                .collect();
            for &b in &order {
                let mut d = b;
                let dominated = loop {
                    if alloc.contains(&d) {
                        break true;
                    }
                    if idom[d] == d || idom[d] == usize::MAX {
                        break false;
                    }
                    d = idom[d];
                };
                if ms.lds_bytes == 0 || dominated {
                    no_barrier.insert(b);
                }
            }
        }
        Lower {
            s,
            x,
            bad,
            lane_fail: HashMap::new(),
            vals: HashMap::new(),
            floats: HashMap::new(),
            low16: HashMap::new(),
            block: vec![None; p.blocks.len()],
            cur: 0,
            bools: HashMap::new(),
            lane_bits: HashMap::new(),
            phi_at,
            mask_phis: HashMap::new(),
            pending_masks: Vec::new(),
            data_phis: Vec::new(),
            read_phis: HashSet::new(),
            params: x.res.params.iter().map(|&(v, p, d)| (v, (p, d))).collect(),
            user_data: None,
            user_reads: BTreeMap::new(),
            root_views: HashMap::new(),
            samplers: Vec::new(),
            descriptors: x
                .res
                .descriptors
                .iter()
                .map(|d| ((d.site, d.role == Role::Sampler), &d.source))
                .collect(),
            scalar: x.res.scalar.iter().copied().collect(),
            exports: x.io.exports.iter().map(|e| (e.site, e)).collect(),
            interps: {
                let mut m: HashMap<Site, &Interp> =
                    x.io.interps.iter().map(|(s, i)| (*s, i)).collect();
                // A first half whose result is also read as data (an f16 second half keeps
                // the other half of its dst, PS_DeferredTextured): evaluated as the pair it
                // starts. Approximate for those other bits (P0 + i P10 on the console); a
                // first half only the second half reads is gone before lowering.
                // Only in the second translation of a program the first
                // one refused for it ([`with_first_half_pairs`]).
                if FIRST_HALF_PAIRS.get()
                    && std::env::var_os("VICTORY_FIRST_HALF").is_none_or(|v| v != "0")
                {
                    for (_, i) in &x.io.interps {
                        if let Interp::Interpolated { first, .. } = i {
                            for s in first {
                                m.entry(*s).or_insert(i);
                            }
                        }
                    }
                }
                m
            },
            handles: HashMap::new(),
            strides: HashMap::new(),
            entry_handles: HashMap::new(),
            join_handles: HashMap::new(),
            edges: HashMap::new(),
            exported: vec![false; p.blocks.len()],
            rpo: vec![usize::MAX; p.blocks.len()],
            tail: vec![None; p.blocks.len()],
            writes_uavs,
            heap: [false; 2],
            src0: None,
            console_lanes,
            split_lds,
            lds_meet,
            local: exchange.is_some() && LOCAL_WAVES.get(),
            local_ok: false,
            local_any: LOCAL_ANY.get(),
            uniform: match &exchange {
                Some((_, false, all)) if !LOCAL_WAVES.get() => lane_uniform(p, x.st, all),
                _ => HashSet::new(),
            },
            own_bits: match &exchange {
                Some((_, false, _)) if !LOCAL_WAVES.get() && own_bits_on() => {
                    super::own_bits::find(p, x.st, std::env::var_os("VICTORY_WAVE_WHY").is_some())
                }
                _ => Default::default(),
            },
            exchange,
            clane: None,
            pair: None,
            halves,
            wave_base: None,

            pops: matches!(x.native.io, StageIo::Ps { db_shader_control, .. }
                if db_shader_control & 1 << 16 != 0),
            root_memory,
            flat: None,
            no_barrier,
            half_outputs: half_outputs(&x.native.io),
            mask_data: match x.native.stage {
                HwStage::Cs => MaskData::Console,
                HwStage::Ps | HwStage::Vs => MaskData::Host,
                _ => MaskData::None,
            },
            mask_ok: false,
            ballots: HashMap::new(),
            balloting: HashSet::new(),
            def_block: p
                .blocks
                .iter()
                .enumerate()
                .flat_map(|(b, block)| {
                    block
                        .insts
                        .iter()
                        .flat_map(move |i| i.writes.iter().map(move |w| (w.1, b)))
                        .chain(block.phis.iter().map(move |phi| (phi.value, b)))
                })
                .collect(),
            cur_ir: 0,
            sections: HashMap::new(),
            section_out: HashSet::new(),

            raw: HashMap::new(),
            raw_use: None,
            raw_meets: Default::default(),
            twin: paired.then(Box::default),
            upper: false,
            lds_last: None,
            lane_scalars: if paired {
                lane_scalars(p, x.res)
            } else {
                HashSet::new()
            },
            fence_only,
            half_views: BTreeSet::new(),
            halves_only: BTreeSet::new(),
            cap_follows: HashMap::new(),
            code_table: None,
            counters: HashMap::new(),
            abort: None,
            round16: round16(p),
            mode16: Some(0),
        }
    }

    /// The dword of the program's code at byte `byte` (a constant table
    /// the compiler appends to the code), as an internal constant array.
    // shortcut: an index outside the table reads its last dword (the
    // console would read whatever memory follows the code).
    fn code_dword(&mut self, byte: V) -> Result<V, Why> {
        let (g, base, len) = match self.code_table {
            Some(t) => t,
            None => {
                let addresses: Vec<&crate::resource::Address> = self
                    .x
                    .res
                    .scalar
                    .iter()
                    .map(|s| &s.1)
                    .chain(self.x.res.descriptors.iter().flat_map(|d| {
                        d.source.leaves().into_iter().filter_map(|l| match l {
                            Source::Built { address, .. } => Some(address),
                            _ => None,
                        })
                    }))
                    .filter(|a| a.param == CODE)
                    .collect();
                let lowest = addresses
                    .iter()
                    .map(|a| a.offset & !3)
                    .min()
                    .ok_or("a code table")?;
                let first = code_table_start(self.x.p, &addresses, lowest);
                let code = &self.x.native.code;
                let words: Vec<u32> = code
                    .get(first as usize..)
                    .ok_or("a code table past the code")?
                    .chunks(4)
                    .map(|c| {
                        let mut w = [0u8; 4];
                        w[..c.len()].copy_from_slice(c);
                        u32::from_le_bytes(w)
                    })
                    .collect();
                if words.is_empty() {
                    return Err("a code table past the code");
                }
                let elems: Vec<V> = words.iter().map(|&w| self.s.m.i32(w)).collect();
                let arr = self.s.m.array(words.len() as u32, self.s.t.i32);
                let init = self.s.m.aggregate(arr, &elems);
                let g = self.s.m.constant_global("code", init);
                let t = (g, first, words.len() as u32);
                self.code_table = Some(t);
                t
            }
        };
        let c = self.cur;
        let k = self.s.m.i32(base);
        let rel = self.s.m.bin(c, Bin::Sub, byte, k);
        let two = self.s.m.i32(2);
        let index = self.s.m.bin(c, Bin::LShr, rel, two);
        let last = self.s.m.i32(len - 1);
        let over = self.s.m.cmp(c, pred::IUGT, index, last);
        let index = self.s.m.select(c, over, last, index);
        let at = self.s.m.element(c, g, index);
        Ok(self.s.m.load(c, at))
    }

    /// A global memory access (`global_*`): loads from the program's
    /// constant table, from a root view at the address the title sets, or
    /// from title memory at an address the program computes; stores and
    /// atomics through a root view.
    fn flat(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Flat { dst, .. } = &inst.fields else {
            unreachable!()
        };
        let name = inst.name();
        if !name.starts_with("global_load_") {
            return self.flat_write(inst, b, i);
        }
        let rest = &name["global_load_".len()..];
        let (n, sub) = match rest {
            "dword" => (1, None),
            "dwordx2" => (2, None),
            "dwordx3" => (3, None),
            "dwordx4" => (4, None),
            "ubyte" => (1, Some((8, false))),
            "sbyte" => (1, Some((8, true))),
            "ushort" => (1, Some((16, false))),
            "sshort" => (1, Some((16, true))),
            _ => return Err(name),
        };
        let a = *self
            .scalar
            .get(&(b, i))
            .ok_or("an untracked global address")?;
        let guard = self.guard(b, i)?;
        let Some(Operand::Vector { reg, .. }) = *dst else {
            return Err("a global load result");
        };
        if sub.is_none() {
            let address = if a.param == MEMORY {
                Some(self.global_address(inst)?)
            } else {
                let src = Source::Built {
                    address: a,
                    bits: 0,
                    words: [0, 0],
                };
                self.root_address(&src)
                    .map(|(lo, hi)| {
                        let mut off = self.s.m.i32(a.offset);
                        if let Some(d) = a.dynamic {
                            let d = self.i32_of(d)?;
                            off = self.s.m.bin(self.cur, Bin::Add, off, d);
                        }
                        Ok::<_, Why>(self.add64(lo, hi, off, false))
                    })
                    .transpose()?
            };
            if let Some((lo, hi)) = address {
                let words =
                    self.memory_transfer(lo, hi, guard, n as u8, false, |l, h, byte, range| {
                        l.memory_load_words(h, byte, range.end - range.start)
                    });
                for (k, word) in (0..n).zip(words) {
                    self.write(inst, Reg::V(reg + k as u8), word, guard);
                }
                return Ok(());
            }
        }
        let c = self.cur;
        let bytes = sub.map_or(4 * n, |(width, _)| width / 8);
        let (h, off) = if a.param == MEMORY {
            let (lo, hi) = self.global_address(inst)?;
            let (h, off) = self.memory(lo, hi, bytes, false, guard);
            (Some(h), off)
        } else {
            let mut off = self.s.m.i32(a.offset);
            if let Some(d) = a.dynamic {
                let d = self.i32_of(d)?;
                off = self.s.m.bin(c, Bin::Add, off, d);
            }
            if a.param == CODE {
                (None, off)
            } else {
                let src = Source::Built {
                    address: a,
                    bits: 0,
                    words: [0, 0],
                };
                let (h, off) = self.raw_view((b, i), &src, off, bytes, (11, 0), false, guard)?;
                (Some(h), off)
            }
        };
        if let (None, Some(h), true) = (sub, h, n > 1 && Self::wide_loads()) {
            for (k, x) in self.raw_dwords(h, off, n).into_iter().enumerate() {
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        let c = self.cur;
        let dword = |l: &mut Self, byte: V| -> Result<V, Why> {
            match h {
                None => l.code_dword(byte),
                Some(h) => {
                    let c = l.cur;
                    let (u, mask, al) = (l.s.m.undef(l.s.t.i32), l.s.m.i8(1), l.s.m.i32(4));
                    let r = l.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        l.s.t.res_ret,
                        139,
                        &[h, byte, u, mask, al],
                    );
                    let x = l.s.m.extract(c, r, 0);
                    Ok(l.as_int(x))
                }
            }
        };
        for k in 0..n {
            let x = match sub {
                None => {
                    let four = self.s.m.i32(4 * k);
                    let at = self.s.m.bin(c, Bin::Add, off, four);
                    dword(self, at)?
                }
                Some((width, signed)) => {
                    // shortcut: the field is taken not to cross a dword.
                    let (not3, three) = (self.s.m.i32(!3), self.s.m.i32(3));
                    let at = self.s.m.bin(c, Bin::And, off, not3);
                    let w = dword(self, at)?;
                    let byte = self.s.m.bin(c, Bin::And, off, three);
                    let sh = self.s.m.bin(c, Bin::Shl, byte, three);
                    let x = self.s.m.bin(c, Bin::LShr, w, sh);
                    let left = self.s.m.i32(32 - width);
                    let x = self.s.m.bin(c, Bin::Shl, x, left);
                    self.s
                        .m
                        .bin(c, if signed { Bin::AShr } else { Bin::LShr }, x, left)
                }
            };
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }

    /// The 64-bit address a `global_*` instruction accesses in each lane:
    /// its SGPR pair plus its VGPR offset, or its VGPR pair, plus its
    /// immediate; the high dword cut to address bits 32-47.
    fn global_address(&mut self, inst: &Inst) -> Result<(V, V), Why> {
        const WHY: Why = "a global address";
        let Fields::Flat {
            addr: Some(Operand::Vector { reg, .. }),
            saddr,
            offset,
            ..
        } = inst.fields
        else {
            return Err(WHY);
        };
        let read = |l: &mut Self, r: Reg| -> Result<V, Why> {
            let v = inst.read(r).ok_or(WHY)?;
            l.source(v)
        };
        let (lo, hi) = match saddr {
            Some(Operand::Scalar { code, .. }) if code != NULL => {
                let (lo, hi) = (read(self, Reg::S(code))?, read(self, Reg::S(code + 1))?);
                let x = read(self, Reg::V(reg))?;
                self.add64(lo, hi, x, false)
            }
            _ => (read(self, Reg::V(reg))?, read(self, Reg::V(reg + 1))?),
        };
        let k = self.s.m.i32(offset as i32 as u32);
        let (lo, hi) = self.add64(lo, hi, k, offset < 0);
        let mask = self.s.m.i32(0xFFFF);
        Ok((lo, self.s.m.bin(self.cur, Bin::And, hi, mask)))
    }

    /// A global store or atomic (`global_store_*`, `global_atomic_*`)
    /// through a root view at the address the title sets, or into title
    /// memory at an address the program computes.
    fn flat_write(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Flat {
            dst, data, cache, ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let name = inst.name();
        let a = *self
            .scalar
            .get(&(b, i))
            .ok_or("an untracked global address")?;
        if a.param == CODE {
            return Err("a global store into the code");
        }
        let n = match name.strip_prefix("global_store_") {
            None => 1,
            Some("dword") => 1,
            Some("dwordx2") => 2,
            Some("dwordx3") => 3,
            Some("dwordx4") => 4,
            Some(_) => return Err(name),
        };
        // DXIL atomic operations: add, and, or, xor, signed and unsigned min
        // and max, exchange; subtraction adds the negation. A float minimum
        // or maximum is the signed integer one where the value is positive
        // and the reversed unsigned one where negative (IEEE order of the
        // bits), as two atomics, each a no-op in the lanes the other serves.
        let ops: Option<&[(u32, u32)]> = match name.strip_prefix("global_atomic_") {
            None => None,
            Some("add" | "sub") => Some(&[(0, 0)]),
            Some("and") => Some(&[(1, 0)]),
            Some("or") => Some(&[(2, 0)]),
            Some("xor") => Some(&[(3, 0)]),
            Some("smin") => Some(&[(4, 0)]),
            Some("smax") => Some(&[(5, 0)]),
            Some("umin") => Some(&[(6, 0)]),
            Some("umax") => Some(&[(7, 0)]),
            Some("swap") => Some(&[(8, 0)]),
            Some("fmin") if !cache.glc => Some(&[(4, i32::MAX as u32), (7, 0)]),
            Some("fmax") if !cache.glc => Some(&[(5, i32::MIN as u32), (6, u32::MAX)]),
            Some(_) => return Err(name),
        };
        if ops.is_none() && !name.starts_with("global_store_") {
            return Err(name);
        }
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let d = data.as_ref().ok_or("global store data")?;
        let vals = self.vgprs(inst, d, n)?;
        let (memory, root) = if a.param == MEMORY {
            (Some(self.global_address(inst)?), None)
        } else {
            let mut off = self.s.m.i32(a.offset);
            if let Some(d) = a.dynamic {
                let d = self.i32_of(d)?;
                off = self.s.m.bin(c, Bin::Add, off, d);
            }
            let src = Source::Built {
                address: a,
                bits: 0,
                words: [0, 0],
            };
            match self.root_address(&src) {
                Some((lo, hi)) => {
                    // As `raw_view`, looked up inside the guard below.
                    let (lo, hi) = self.add64(lo, hi, off, false);
                    let mask = self.s.m.i32(0xFFFF);
                    (Some((lo, self.s.m.bin(self.cur, Bin::And, hi, mask))), None)
                }
                None => {
                    let h = self.handle((b, i), &src, false, (11 | 0x1000, 0))?;
                    let off = match self.root_view_offset(&src) {
                        Some(at) => self.s.m.bin(self.cur, Bin::Add, off, at),
                        None => off,
                    };
                    (None, Some((h, off)))
                }
            }
        };
        let (iu, void, i32t) = (self.s.m.undef(self.s.t.i32), self.s.t.void, self.s.t.i32);
        // Title memory is looked up (and its pages marked written) only in
        // the lanes that write.
        let at = self.begin_guarded(guard);
        if name.starts_with("global_store_")
            && let Some((lo, hi)) = memory
        {
            self.memory_transfer(lo, hi, None, n, true, |l, h, byte, range| {
                l.memory_store_words(
                    h,
                    byte,
                    &vals[usize::from(range.start)..usize::from(range.end)],
                )
            });
            self.end_guarded(at, &[]);
            return Ok(());
        }
        let (h, off) = match (memory, root) {
            (Some((lo, hi)), _) => self.memory(lo, hi, 4 * u32::from(n), true, None),
            (_, Some(r)) => r,
            _ => unreachable!(),
        };
        if name.starts_with("global_store_") {
            let mut args = vec![h, off, iu];
            args.extend(&vals);
            args.resize(3 + 4, iu);
            args.push(self.s.m.i8((1u8 << n) - 1));
            args.push(self.s.m.i32(4));
            self.s.op(
                self.cur,
                "rawBufferStore.i32",
                Attr::NoUnwind,
                void,
                140,
                &args,
            );
            self.mark_view_write(h, off, 4 * u32::from(n), None);
            self.end_guarded(at, &[]);
            return Ok(());
        }
        let ops = ops.expect("an atomic");
        let mut x = vals[0];
        let c = self.cur;
        if name == "global_atomic_sub" {
            let zero = self.s.m.i32(0);
            x = self.s.m.bin(c, Bin::Sub, zero, x);
        }
        let negative = {
            let zero = self.s.m.i32(0);
            self.s.m.cmp(c, pred::SLT, x, zero)
        };
        let returned = match *dst {
            Some(Operand::Vector { reg, .. }) if cache.glc => Some(reg),
            _ => None,
        };
        let mut first = None;
        for (k, &(op, neutral)) in ops.iter().enumerate() {
            let value = if ops.len() == 1 {
                x
            } else {
                let n = self.s.m.i32(neutral);
                // The first serves positive values, the second negative ones.
                if k == 0 {
                    self.s.m.select(self.cur, negative, n, x)
                } else {
                    self.s.m.select(self.cur, negative, x, n)
                }
            };
            let op = self.s.m.i32(op);
            let r = self.s.op(
                self.cur,
                "atomicBinOp.i32",
                Attr::NoUnwind,
                i32t,
                78,
                &[h, op, off, iu, iu, value],
            );
            first.get_or_insert(r);
        }
        self.mark_view_write(h, off, 4, None);
        let zero = self.s.m.i32(0);
        let r = self.end_guarded(at, &[(first.expect("an atomic"), zero)])[0];
        if let Some(reg) = returned {
            self.write(inst, Reg::V(reg), r, guard);
        }
        Ok(())
    }

    /// Finds the whole-wave sections and the values they leave.
    fn find_sections(&mut self) {
        let p = self.x.p;
        let mut inside: HashMap<Value, (usize, usize, usize)> = HashMap::new();
        for b in 0..p.blocks.len() {
            let found = super::wave::sections(p, b);
            for (&start, sec) in &found {
                for i in start + 1..sec.end {
                    for &(_, w) in &p.blocks[b].insts[i].writes {
                        inside.insert(w, (b, start, sec.end));
                    }
                }
            }
            if !found.is_empty() {
                self.sections.insert(b, found);
            }
        }
        if inside.is_empty() {
            return;
        }
        for (b, block) in p.blocks.iter().enumerate() {
            for phi in &block.phis {
                self.section_out
                    .extend(phi.args.iter().filter(|a| inside.contains_key(a)));
            }
            for (i, inst) in block.insts.iter().enumerate() {
                for &(_, r) in &inst.reads {
                    if let Some(&(sb, start, end)) = inside.get(&r)
                        && !(sb == b && i > start && i < end)
                    {
                        self.section_out.insert(r);
                    }
                }
            }
        }
    }

    fn emit_body(&mut self) -> Result<(), Stop> {
        self.halves_only = self.places_read_as_halves();
        let p = self.x.p;
        let order = crate::ir::reverse_postorder(&p.blocks);
        for (n, &b) in order.iter().enumerate() {
            self.rpo[b] = n;
        }
        for (n, &b) in order.iter().enumerate() {
            self.block[b] = Some(if n == 0 {
                self.s.entry
            } else {
                self.s.m.new_block()
            });
        }
        // Loop headers count iterations (`LOOP_LIMIT`).
        for &b in &order {
            let header = p.blocks[b]
                .preds
                .iter()
                .any(|&q| self.block[q].is_some() && self.rpo[b] <= self.rpo[q]);
            if header {
                let blk = self.block[b].expect("reachable");
                let phi = self.s.m.phi(blk, self.s.t.i32);
                self.counters.insert(b, (phi, Vec::new()));
            }
        }
        self.find_cap_follows(&order);
        self.cur = self.s.entry;
        self.find_sections();
        self.inputs()?;
        if self.twin.is_some() {
            self.swap_lanes();
            let r = self.inputs();
            self.swap_lanes();
            r?;
        }
        if let Some(mesh) = self.s.mesh {
            self.mesh_prologue(mesh).map_err(|w| unsupported(0, w))?;
            self.block[order[0]] = Some(self.cur);
        }
        for &b in &order {
            self.cur = self.block[b].expect("reachable");
            self.cur_ir = b;
            let preds: Vec<usize> = p.blocks[b]
                .preds
                .iter()
                .copied()
                .filter(|&q| self.block[q].is_some())
                .collect();
            self.exported[b] = !preds.is_empty() && preds.iter().all(|&q| self.exported_out(q));
            self.block_phis(b);
            if self.twin.is_some() {
                self.swap_lanes();
                self.block_phis(b);
                self.swap_lanes();
            }
            self.lds_last = None;
            if self.lds_meet.as_ref().is_some_and(|(_, meet)| meet[b].0) {
                self.lds_meeting();
            }
            let mut i = 0;
            self.mode16 = self.round16[b];
            while i < p.blocks[b].insts.len() {
                if let Some(&lanes) = self.x.st.discards.get(&(b, i)) {
                    // A pixel's discard: the lanes stay on as helpers.
                    let at = self.cur;
                    let offset = p.blocks[b].insts[i].machine.offset;
                    let kill = self
                        .boolean(lanes, at)
                        .map_err(|w| unsupported(offset, w))?;
                    self.s
                        .op(at, "discard", Attr::NoUnwind, self.s.t.void, 82, &[kill]);
                }
                if self.twin.is_some() {
                    self.paired_inst(b, i)?;
                } else {
                    self.inst(b, i)?;
                }
                self.mode16 = round16_after(&p.blocks[b].insts[i], self.mode16);
                if let Some(end) = self.sections.get(&b).and_then(|s| s.get(&i)).map(|s| s.end) {
                    for j in i + 1..end {
                        if self.twin.is_some() {
                            self.paired_section_inst(b, j, i);
                        } else {
                            self.section_inst(b, j, i);
                        }
                    }
                    i = end;
                    continue;
                }
                i += 1;
            }
            self.mask_ok = self.mask_data == MaskData::Console;
            if self.lds_meet.as_ref().is_some_and(|(_, meet)| meet[b].1) {
                self.lds_meeting();
            }
            self.tail[b] = Some(self.cur);
            self.exit(b)?;
        }
        self.fill_phis()?;
        while self.twin.is_some() {
            self.swap_lanes();
            let r = self.fill_phis();
            self.swap_lanes();
            r?;
            let left = self
                .twin
                .as_deref()
                .is_some_and(|t| !t.pending_masks.is_empty());
            if self.pending_masks.is_empty() && !left {
                break;
            }
            self.fill_phis()?;
        }
        Ok(())
    }

    fn run(mut self) -> Result<Lowered, Stop> {
        self.emit_body()?;
        let user_data = self.user_reads.values().copied().collect();
        self.s.samplers = self.samplers.len() as u32;
        let root_cbvs = self.s.root_cbvs.iter().map(|r| r.0).collect();
        let root_srvs = self.s.root_srvs.iter().map(|r| r.0).collect();
        let root_uavs = self.s.root_uavs.iter().map(|r| r.0).collect();
        let memory = self.s.memory.is_some();
        let memory_writes = self.s.memory_dirty.is_some();
        let view_addresses = self.s.view_addresses.is_some();
        let cmp_samplers = self.s.cmp_samplers;
        Ok(Lowered {
            wave_size: self.s.wave_size,
            half_views: std::mem::take(&mut self.half_views).into_iter().collect(),
            cmp_samplers,
            root_cbvs,
            root_srvs,
            root_uavs,
            memory,
            memory_writes,
            view_addresses,
            heap: self.heap[0],
            heap_writes: self.heap[1],
            view_marks: self.s.view_writes.is_some(),
            #[cfg(test)]
            nonuniform_handles: self.s.nonuniform_handles,
            #[cfg(test)]
            heap_handles: self.s.m.written_calls("dx.op.createHandleFromHeap") as u32,
            #[cfg(test)]
            wave_ops: self.s.flags & flags::WAVE_OPS != 0,
            #[cfg(all(test, windows))]
            rovs: self.s.flags & flags::ROVS != 0,
            dxil: self.s.finish(),
            user_data,
            samplers: self.samplers,
        })
    }

    fn exported_out(&self, b: usize) -> bool {
        self.exported[b] || self.has_done_export(b)
    }

    fn has_done_export(&self, b: usize) -> bool {
        (0..self.x.p.blocks[b].insts.len())
            .any(|i| self.exports.get(&(b, i)).is_some_and(|e| e.done))
    }

    /// Values of the program's input registers.
    fn inputs(&mut self) -> Result<(), Stop> {
        let io_inputs: HashMap<Value, Input> = self.x.io.inputs.iter().copied().collect();
        for &(reg, v) in &self.x.p.inputs {
            let val = match reg {
                Reg::S(_) if self.params.contains_key(&v) => continue, // loaded on first use
                Reg::V(_) if matches!(io_inputs.get(&v), Some(Input::Barycentric { .. })) => {
                    continue;
                }
                Reg::S(n) => match io_inputs.get(&v) {
                    Some(&input) => self.input(input),
                    // Above the SGPRs the hardware initializes, a register
                    // holds what the last wave left, so no program depends
                    // on it: a read the paths only seem to allow (a branch
                    // past the write, taken only with one past the read)
                    // reads zero, as VGPRs do.
                    None if n <= 105 && self.x.io.initialized_sgprs.is_some_and(|k| n >= k) => {
                        Val::I32(self.s.m.i32(0))
                    }
                    // The primitive mask (pixel), which console code only
                    // moves to M0 for its interpolation (spilled and reloaded
                    // on the way); the host interpolates by itself, so the
                    // value goes unread.
                    None if self.s.pixel && self.x.io.initialized_sgprs == Some(n + 2) => {
                        Val::I32(self.s.m.i32(0))
                    }
                    // The ordered section's collision id, and (`operand`) the
                    // id of the last wave that left it: 0, so no wave waits;
                    // the host's rasterizer-ordered views order the accesses.
                    None if self.pops && self.x.io.initialized_sgprs == Some(n + 1) => {
                        Val::I32(self.s.m.i32(0))
                    }
                    None => Val::Missing("a hardware SGPR input"),
                },
                Reg::Scc => Val::Missing("SCC on entry"),
                Reg::V(_) => match io_inputs.get(&v) {
                    Some(&input) => self.input(input),
                    // Registers no one set read as zero (the validator
                    // refuses reads of undef).
                    None => Val::I32(self.s.m.i32(0)),
                },
            };
            self.vals.insert(v, val);
        }
        Ok(())
    }

    fn input(&mut self, input: Input) -> Val {
        let (kind, col) = match input {
            Input::VertexId => (1, 0),
            Input::InstanceId => (2, 0),
            Input::Position(c) => (3, c),
            Input::Attribute { row, channel } => {
                return match self.find_input(row, channel) {
                    Some((id, col)) => self.load_input(id, col),
                    None => Val::Missing("a vertex input the signature lacks"),
                };
            }
            Input::Barycentric { kind, index } => return self.barycentric(kind, index),
            Input::PrimitiveId => return Val::Missing("the primitive id"),
            // The console code tests it against 0 (`v_cmp_ne_u32`), as the
            // host's value can be.
            Input::FrontFace => (13, 0),
            Input::ThreadIdInGroup(c) => return self.thread_id(c),
            Input::GroupId(c) => return self.compute_id("groupId.i32", 94, c),
            Input::GroupInfo => return Val::Missing("the thread-group info SGPR"),
            Input::MeshGroup => {
                let Val::I32(g) = self.compute_id("groupId.i32", 94, 0) else {
                    unreachable!()
                };
                let n = self.s.m.i32(self.x.mesh_stride);
                return Val::I32(self.s.m.bin(self.s.entry, Bin::Mul, g, n));
            }
            Input::MergedWaveInfo => {
                // shortcut: only the wave index (bits 24-27) is set; the
                // console's other bits (lane counts) are read by no program.
                let f = self.mesh_thread();
                let shift = self.s.m.i32(self.wave().trailing_zeros());
                let w = self.s.m.bin(self.s.entry, Bin::LShr, f, shift);
                let k24 = self.s.m.i32(24);
                return Val::I32(self.s.m.bin(self.s.entry, Bin::Shl, w, k24));
            }
            _ => return Val::Missing("a pixel system input"),
        };
        let free = self
            .s
            .inputs
            .iter()
            .filter_map(|e| e.row)
            .map(|r| r + 1)
            .max()
            .unwrap_or(0);
        let id = match self.s.inputs.iter().position(|e| e.kind == kind) {
            Some(id) => {
                if kind != 3 {
                    self.s.inputs[id].row.get_or_insert(free);
                }
                id
            }
            None if kind >= 3 => return Val::Missing("a pixel system input the signature lacks"),
            None => {
                // The console signature leaves out vertex and instance ids; PC
                // gives them a row after the others.
                let row = self
                    .s
                    .inputs
                    .iter()
                    .filter_map(|e| e.row)
                    .map(|r| r + 1)
                    .max()
                    .unwrap_or(0);
                self.s.inputs.push(SigElement {
                    name: ["", "SV_VertexID", "SV_InstanceID"][kind as usize].into(),
                    index: 0,
                    kind,
                    comp: 5,
                    interp: 0,
                    row: Some(row),
                    col: 0,
                    cols: 1,
                    usage: 0,
                    half: false,
                });
                self.s.inputs.len() - 1
            }
        };
        self.load_input(id, col)
    }

    /// A component of a compute thread or group id, read in the entry block.
    fn compute_id(&mut self, name: &str, opcode: u32, c: u8) -> Val {
        let (entry, c) = (self.s.entry, self.s.m.i32(u32::from(c)));
        Val::I32(
            self.s
                .op(entry, name, Attr::ReadNone, self.s.t.i32, opcode, &[c]),
        )
    }

    fn find_input(&self, row: u8, channel: u8) -> Option<(usize, u8)> {
        self.s.inputs.iter().enumerate().find_map(|(id, e)| {
            let r = e.row?;
            // `then`, not `then_some`: the channel is only past `col` when
            // the condition holds.
            (r == u32::from(row) && channel >= e.col && channel < e.col + e.cols)
                .then(|| (id, channel - e.col))
        })
    }

    fn load_input(&mut self, id: usize, col: u8) -> Val {
        if self.s.inputs[id].half {
            // shortcut: 16-bit vertex inputs (the fetch routine's) are not
            // unpacked; pixel inputs are (`interp`).
            return Val::Missing("a 16-bit input");
        }
        self.load_any_input(id, col)
    }

    /// An input element's channel as declared (16-bit ones as 32-bit floats).
    fn load_any_input(&mut self, id: usize, col: u8) -> Val {
        let e = &mut self.s.inputs[id];
        e.usage |= 1 << col;
        let (int, bit) = (e.comp != 9, e.comp == 1);
        let ty = if bit {
            self.s.t.i1
        } else if int {
            self.s.t.i32
        } else {
            self.s.t.f32
        };
        let suffix = if bit {
            ".i1"
        } else if int {
            ".i32"
        } else {
            ".f32"
        };
        let entry = self.s.entry;
        let args = [
            self.s.m.i32(id as u32),
            self.s.m.i32(0),
            self.s.m.i8(col),
            self.s.m.undef(self.s.t.i32),
        ];
        let v = self.s.op(
            entry,
            &format!("loadInput{suffix}"),
            Attr::ReadNone,
            ty,
            4,
            &args,
        );
        if bit {
            let z = self.s.m.cast(entry, Cast::ZExt, v, self.s.t.i32);
            Val::I32(z)
        } else if int {
            Val::I32(v)
        } else {
            Val::I32(self.s.m.cast(entry, Cast::Bitcast, v, self.s.t.i32))
        }
    }

    /// The DXIL form of an IR value.
    fn get(&mut self, v: Value) -> Result<Val, Why> {
        if self.phi_at.contains_key(&v) {
            self.read_phis.insert(v);
        }
        let is_mask = self.x.st.masks.contains_key(&v) || self.x.st.highs.contains_key(&v);
        match self.vals.get(&v) {
            Some(&Val::Missing(_)) | None if self.mask_ok && is_mask => {
                return self.mask_dword(v).map(Val::I32);
            }
            Some(&Val::Missing(w)) => return Err(w),
            Some(&val) => return Ok(val),
            None => {}
        }
        if let Some(&(param, dword)) = self.params.get(&v) {
            let val = self.user(param, dword);
            self.vals.insert(v, val);
            return Ok(val);
        }
        if let Some(loaded) = self.lazy_barycentric(v) {
            loaded?;
            return Ok(self.vals[&v]);
        }
        Err("a value defined after its use")
    }

    fn lazy_barycentric(&mut self, v: Value) -> Option<Result<(), Why>> {
        let (kind, index) = self.x.io.inputs.iter().find_map(|&(w, i)| match i {
            Input::Barycentric { kind, index } if w == v => Some((kind, index)),
            _ => None,
        })?;
        let val = self.barycentric(kind, index);
        self.vals.insert(v, val);
        Some(match val {
            Val::Missing(w) => Err(w),
            _ => Ok(()),
        })
    }

    fn barycentric(&mut self, kind: Barycentric, index: u8) -> Val {
        let Some(mode) = interp_mode(kind) else {
            return Val::Missing("pull-model barycentrics");
        };
        if index > 1 {
            return Val::Missing("pull-model barycentrics");
        }
        // One element per interpolation mode (the console's own, if its
        // signature declares one): DXIL allows two.
        let bary = |e: &SigElement| e.kind == 28;
        let id = match self
            .s
            .inputs
            .iter()
            .position(|e| bary(e) && (e.interp == mode || e.usage == 0))
        {
            Some(id) => id,
            None => {
                let index = self.s.inputs.iter().filter(|e| bary(e)).count() as u32;
                if index >= 2 {
                    return Val::Missing("barycentrics of a third interpolation mode");
                }
                // A system value the validator wants unpacked (packing
                // location -1), like other non-packed system values.
                self.s.inputs.push(SigElement {
                    name: "SV_Barycentrics".into(),
                    index,
                    kind: 28,
                    comp: 9,
                    interp: mode,
                    row: None,
                    col: 0,
                    cols: 3,
                    usage: 0,
                    half: false,
                });
                self.s.inputs.len() - 1
            }
        };
        let e = &mut self.s.inputs[id];
        if e.col != 0 || e.cols < 3 {
            return Val::Missing("a console barycentrics element without i and j");
        }
        e.row = None;
        e.interp = mode;
        self.s.flags |= flags::BARYCENTRICS;
        self.load_any_input(id, index + 1)
    }

    fn i32_of(&mut self, v: Value) -> Result<V, Why> {
        match self.get(v)? {
            Val::I32(x) => Ok(x),
            Val::Bit(x) => Ok(self.s.m.cast(self.cur, Cast::ZExt, x, self.s.t.i32)),
            Val::Missing(w) => Err(w),
        }
    }

    /// A user-data dword: a root parameter's value.
    fn user(&mut self, param: usize, dword: u32) -> Val {
        let rp = self.x.native.layout[param];
        match rp.root {
            RootType::ResourceHeap | RootType::SamplerHeap => Val::I32(self.s.m.i32(0)),
            RootType::Constants | RootType::Table => Val::I32(self.user_dword(param, dword)),
            // Its address, where root views are title memory.
            RootType::Descriptor if self.root_memory => Val::I32(self.user_dword(param, dword)),
            _ => Val::Missing("a root descriptor"),
        }
    }

    /// User-data dword `dword` of root parameter `param` as the host passes
    /// it (for a root descriptor: its GPU address, low dword first).
    fn user_dword(&mut self, param: usize, dword: u32) -> V {
        let rp = self.x.native.layout[param];
        let index = u32::from(rp.user_data) + dword;
        self.user_at(UserData {
            index,
            slot: rp.slot,
            root: rp.root,
            dword,
        })
    }

    /// A root SRV or UAV (`uav`) of root parameter `param`, in a program
    /// that does not read title memory: the resource-heap view the host
    /// makes of the host copy holding the buffer, whose index comes in the
    /// root descriptor's own first user-data dword (module notes, "Root SRVs
    /// and root UAVs"); offsets from the root address move by
    /// [`Lower::root_view_offset`]. Host root views have no bounds; a view's
    /// reads past the copy give 0 and its writes there are dropped.
    fn root_view(&mut self, param: usize, uav: bool) -> V {
        let rp = self.x.native.layout[param];
        // The descriptor's two user-data dwords hold its address on the
        // console; such a program never reads them as data (`user`).
        let index = u32::from(rp.user_data);
        if let Some(&h) = self.root_views.get(&index) {
            return h;
        }
        let i = self.user_at(UserData {
            index,
            slot: rp.slot,
            root: RootType::Descriptor,
            dword: if uav {
                super::ROOT_UAV_VIEW
            } else {
                super::ROOT_SRV_VIEW
            },
        });
        let props = if uav { (11 | 0x1000, 0) } else { (11, 0) };
        let h = self.s.heap_handle(self.s.entry, i, false, false, props);
        self.root_views.insert(index, h);
        h
    }

    /// For a root SRV or UAV read through the view of its host copy
    /// ([`Lower::root_view`]): the root address's byte offset in that view,
    /// from the descriptor's second user-data dword. None for other sources.
    fn root_view_offset(&mut self, src: &Source) -> Option<V> {
        let Source::Built { address, .. } = src else {
            return None;
        };
        let rp = *self.x.native.layout.get(address.param)?;
        let cbv = self
            .used_kind(src)
            .is_some_and(|u| u.kind == ResourceType::Cbv);
        if self.root_memory || rp.root != RootType::Descriptor || cbv {
            return None;
        }
        Some(self.user_at(UserData {
            index: u32::from(rp.user_data) + 1,
            slot: rp.slot,
            root: RootType::Descriptor,
            dword: super::ROOT_VIEW_OFFSET,
        }))
    }

    /// User-data dword `u.index`, which the host fills as `u` says, loaded
    /// in the entry block.
    fn user_at(&mut self, u: UserData) -> V {
        let index = u.index;
        self.user_reads.insert(index, u);
        self.s.user_data = self.s.user_data.max(index + 1);
        let h = match self.user_data {
            Some(h) => h,
            None => {
                let h = self.s.user_data_handle();
                self.user_data = Some(h);
                h
            }
        };
        let entry = self.s.entry;
        let row = self.s.m.i32(index / 4);
        let r = self.s.op(
            entry,
            "cbufferLoadLegacy.i32",
            Attr::ReadOnly,
            self.s.t.cbuf_ret,
            59,
            &[h, row],
        );
        self.s.m.extract(entry, r, index % 4)
    }

    /// In a program that uses title memory, the address of the root SRV or
    /// UAV a descriptor `src` is built around: `(lo, hi)`, `hi` cut to bits
    /// 32-47. None for other descriptors and programs.
    fn root_address(&mut self, src: &Source) -> Option<(V, V)> {
        let Source::Built { address, .. } = src else {
            return None;
        };
        let rp = self.x.native.layout.get(address.param)?;
        let cbv = self
            .used_kind(src)
            .is_some_and(|u| u.kind == ResourceType::Cbv);
        if !self.root_memory || rp.root != RootType::Descriptor || cbv {
            return None;
        }
        let lo = self.user_dword(address.param, 0);
        let hi = self.user_dword(address.param, 1);
        let mask = self.s.m.i32(0xFFFF);
        Some((lo, self.s.m.bin(self.s.entry, Bin::And, hi, mask)))
    }

    /// A raw view `src` at byte offset `off` (`props` as [`Self::handle`]
    /// takes them): its handle and the offset in it. A root view of a
    /// program that uses title memory is title memory at the root address
    /// plus the offset ([`Self::memory`]: `bytes` accessed, `write`, the
    /// `active` lanes), so the access is bounds-checked (host root views
    /// have no bounds) and sees the copies the program's other accesses see.
    #[allow(clippy::too_many_arguments)]
    fn raw_view(
        &mut self,
        site: Site,
        src: &Source,
        off: V,
        bytes: u32,
        props: (u32, u32),
        write: bool,
        active: Option<V>,
    ) -> Result<(V, V), Why> {
        let Some((lo, hi)) = self.root_address(src) else {
            let h = self.handle(site, src, false, props)?;
            let off = match self.root_view_offset(src) {
                Some(at) => self.s.m.bin(self.cur, Bin::Add, off, at),
                None => off,
            };
            return Ok((h, off));
        };
        // The hardware adds the 32-bit offset to the 48-bit base.
        let (lo, hi) = self.add64(lo, hi, off, false);
        let mask = self.s.m.i32(0xFFFF);
        let hi = self.s.m.bin(self.cur, Bin::And, hi, mask);
        Ok(self.memory(lo, hi, bytes, write, active))
    }

    /// Both dwords of a mask holding lanes that do not reach the block
    /// writing it (a complement, a copy made where EXEC is not known), so
    /// no ballot there gives it: with console lanes, the scalar operation
    /// that writes it applied to its operands' numbers, as the console does.
    fn mask_number(&mut self, lo: Value) -> Result<(V, V), Why> {
        const ELSEWHERE: Why = "a lane mask holding lanes elsewhere read as data";
        if self.mask_data != MaskData::Console {
            return Err(ELSEWHERE);
        }
        let p = self.x.p;
        // A register the compiler reuses: a mask on one path, data on another.
        if let Some(&(b, i)) = self.phi_at.get(&lo)
            && p.blocks[b].phis[i]
                .args
                .iter()
                .any(|a| !self.x.st.masks.contains_key(a))
        {
            return Err("a register joining a lane mask and data, read as data");
        }
        let &b = self.def_block.get(&lo).ok_or(ELSEWHERE)?;
        let inst = p.blocks[b]
            .insts
            .iter()
            .find(|i| i.writes.iter().any(|w| w.1 == lo))
            .ok_or(ELSEWHERE)?;
        let Fields::Sop {
            dst: Some(Operand::Scalar { code, .. }),
            src,
            ..
        } = &inst.fields
        else {
            return Err(ELSEWHERE);
        };
        let wave64 = self.x.native.wave64;
        let width = if wave64 { "_b64" } else { "_b32" };
        let op = inst
            .name()
            .strip_prefix("s_")
            .and_then(|n| n.strip_suffix(width))
            .ok_or(ELSEWHERE)?;
        if inst.write(Reg::S(*code)) != Some(lo) || inst.write(Reg::S(EXEC)).is_some() {
            return Err(ELSEWHERE);
        }
        let at = if b == self.cur_ir {
            self.cur
        } else {
            self.tail[b].ok_or("a lane mask written after its use")?
        };
        // Dword `h` of source `k` (inline constants sign-extend).
        let word = |l: &mut Self, k: usize, h: u8| -> Result<V, Why> {
            let x = match src.get(k) {
                Some(&Operand::Scalar { code, .. }) if code != NULL => {
                    let v = inst.read(Reg::S(code + h)).ok_or(ELSEWHERE)?;
                    return l.i32_of(v);
                }
                Some(&Operand::Const(c)) => {
                    let x = crate::resource::inline(c).ok_or(ELSEWHERE)?;
                    match h {
                        0 => x,
                        _ if (x as i32) < 0 => u32::MAX,
                        _ => 0,
                    }
                }
                Some(&Operand::Literal(x)) if h == 0 => x,
                Some(Operand::Literal(_)) => 0,
                _ => return Err(ELSEWHERE),
            };
            Ok(l.s.m.i32(x))
        };
        let mut out = [self.s.m.i32(0); 2];
        for h in 0..1 + u8::from(wave64) {
            let a = word(self, 0, h)?;
            let ones = self.s.m.i32(u32::MAX);
            out[usize::from(h)] = match op {
                "mov" => a,
                "not" => self.s.m.bin(at, Bin::Xor, a, ones),
                "cselect" => {
                    let c = word(self, 1, h)?;
                    let scc = inst.read(Reg::Scc).ok_or(ELSEWHERE)?;
                    let cond = match self.get(scc)? {
                        Val::Bit(x) => x,
                        Val::I32(x) => {
                            let z = self.s.m.i32(0);
                            self.s.m.cmp(at, pred::NE, x, z)
                        }
                        Val::Missing(w) => return Err(w),
                    };
                    self.s.m.select(at, cond, a, c)
                }
                _ => {
                    let c = word(self, 1, h)?;
                    // (operation, complement of src1, complement of the result)
                    let (f, not_c, not_r) = match op {
                        "and" => (Bin::And, false, false),
                        "or" => (Bin::Or, false, false),
                        "xor" => (Bin::Xor, false, false),
                        "andn2" => (Bin::And, true, false),
                        "orn2" => (Bin::Or, true, false),
                        "nand" => (Bin::And, false, true),
                        "nor" => (Bin::Or, false, true),
                        "xnor" => (Bin::Xor, false, true),
                        _ => return Err(ELSEWHERE),
                    };
                    let c = if not_c {
                        self.s.m.bin(at, Bin::Xor, c, ones)
                    } else {
                        c
                    };
                    let r = self.s.m.bin(at, f, a, c);
                    if not_r {
                        self.s.m.bin(at, Bin::Xor, r, ones)
                    } else {
                        r
                    }
                }
            };
        }
        Ok((out[0], out[1]))
    }

    // ---- operands ----

    fn operand(&mut self, inst: &Inst, op: &Operand) -> Result<V, Why> {
        match *op {
            Operand::Scalar { code: NULL, .. } => Ok(self.s.m.i32(0)),
            Operand::Scalar { code, .. } => {
                let v = inst.read(Reg::S(code)).ok_or("an unread operand")?;
                self.i32_of(v)
            }
            Operand::Vector { reg, .. } => {
                let v = inst.read(Reg::V(reg)).ok_or("an unread operand")?;
                self.source(v)
            }
            // src_scc: SCC as 0 or 1.
            Operand::Const(253) => {
                let bit = self.scc_in(inst)?;
                Ok(self.s.m.cast(self.cur, Cast::ZExt, bit, self.s.t.i32))
            }
            // `src_pops_exiting_wave_id` (see `inputs`).
            Operand::Const(239) if self.pops => Ok(self.s.m.i32(0)),
            Operand::Const(c) => crate::resource::inline(c)
                .map(|x| self.s.m.i32(x))
                .ok_or("a special operand (vccz, execz, scc, lds_direct)"),
            Operand::Literal(x) => Ok(self.s.m.i32(x)),
            Operand::Attr { .. } => Err("an attribute operand"),
        }
    }

    /// GFX10 puts an inline f16 constant in the low half of the operand.
    fn f16_operand(&mut self, inst: &Inst, op: &Operand) -> Result<V, Why> {
        match *op {
            Operand::Const(c @ 240..=248) => {
                let h = [
                    0x3800, 0xB800, 0x3C00, 0xBC00, 0x4000, 0xC000, 0x4400, 0xC400, 0x3118,
                ][usize::from(c - 240)];
                Ok(self.s.m.i32(h))
            }
            _ => self.operand(inst, op),
        }
    }

    /// VGPR source `k` read for its low 16 bits alone, when only those are
    /// known (`low16`); the caller applies the modifiers and selects.
    fn low16_src(&self, inst: &Inst, k: usize) -> Option<V> {
        let Fields::Valu(v) = &inst.fields else {
            return None;
        };
        match v.src.get(k) {
            Some(&Operand::Vector { reg, .. }) if v.dpp.is_none() => inst
                .read(Reg::V(reg))
                .and_then(|o| self.low16.get(&o).copied()),
            _ => None,
        }
    }

    fn as_float(&mut self, x: V) -> V {
        self.s.m.cast(self.cur, Cast::Bitcast, x, self.s.t.f32)
    }

    fn as_int(&mut self, f: V) -> V {
        self.s.m.cast(self.cur, Cast::Bitcast, f, self.s.t.i32)
    }

    /// The SDWA operand selection of source `k`: a byte or word of the
    /// dword, zero- or sign-extended (none: the whole dword).
    fn sdwa_src(inst: &Inst, k: usize) -> Option<(u8, bool)> {
        let Fields::Valu(v) = &inst.fields else {
            return None;
        };
        let s = v.sdwa?;
        let sel = *s.src_sel.get(k)?;
        let sext = s.sext >> k & 1 != 0;
        (sel < 6).then_some((sel, sext))
    }

    /// Bits `sel` (0-3 a byte, 4-5 a word) of `x`, extended.
    fn select_bits(&mut self, x: V, sel: u8, sext: bool) -> V {
        let c = self.cur;
        let (width, at) = if sel < 4 {
            (8, 8 * sel)
        } else {
            (16, 16 * (sel - 4))
        };
        let left = self.s.m.i32(u32::from(32 - width - at));
        let t = self.s.m.bin(c, Bin::Shl, x, left);
        let right = self.s.m.i32(32 - u32::from(width));
        let op = if sext { Bin::AShr } else { Bin::LShr };
        self.s.m.bin(c, op, t, right)
    }

    /// A float source with its abs/neg modifiers.
    fn fsrc(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let selected = Self::sdwa_src(inst, k);
        let mut f = match op {
            _ if selected.is_some() => {
                let (sel, sext) = selected.expect("checked");
                let x = self.operand(inst, &op)?;
                let x = self.select_bits(x, sel, sext);
                self.as_float(x)
            }
            _ if k == 0 && self.src0.is_some() => {
                let x = self.src0.expect("checked");
                self.as_float(x)
            }
            Operand::Vector { reg, .. } | Operand::Scalar { code: reg, .. }
                if !matches!(op, Operand::Scalar { code: NULL, .. }) =>
            {
                let r = if matches!(op, Operand::Vector { .. }) {
                    Reg::V(reg)
                } else {
                    Reg::S(reg)
                };
                let val = inst.read(r).ok_or("an unread operand")?;
                self.float_of(val)?
            }
            _ => {
                let x = self.operand(inst, &op)?;
                self.as_float(x)
            }
        };
        if v.abs >> k & 1 != 0 {
            f = self.unary(6, f);
        }
        if v.neg >> k & 1 != 0 {
            let z = self.s.m.f32(0x8000_0000);
            f = self.s.m.bin(self.cur, Bin::Sub, z, f);
        }
        Ok(f)
    }

    fn src(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        if v.abs >> k & 1 != 0 || v.neg >> k & 1 != 0 {
            return Err("integer operand modifiers");
        }
        match self.src0 {
            Some(x) if k == 0 => Ok(x),
            _ => {
                let sel = Self::sdwa_src(inst, k);
                // A byte or word select within the low half.
                let low = matches!(sel, Some((0 | 1 | 4, _)));
                let x = match self.low16_src(inst, k).filter(|_| low) {
                    Some(x) => x,
                    None => self.operand(inst, &op)?,
                };
                Ok(match sel {
                    Some((sel, sext)) => self.select_bits(x, sel, sext),
                    None => x,
                })
            }
        }
    }

    fn unary(&mut self, op: u32, f: V) -> V {
        self.s.op(
            self.cur,
            "unary.f32",
            Attr::ReadNone,
            self.s.t.f32,
            op,
            &[f],
        )
    }

    fn binary(&mut self, op: u32, a: V, b: V) -> V {
        self.s.op(
            self.cur,
            "binary.f32",
            Attr::ReadNone,
            self.s.t.f32,
            op,
            &[a, b],
        )
    }

    /// A 32-bit denormal as the console's float mode reads it: a zero of its
    /// sign, decided on the bits (the host's own denormal mode varies).
    fn ftz(&mut self, f: V) -> V {
        let c = self.cur;
        let x = self.as_int(f);
        let (exp, sign, zero) = (
            self.s.m.i32(0x7F80_0000),
            self.s.m.i32(0x8000_0000),
            self.s.m.i32(0),
        );
        let e = self.s.m.bin(c, Bin::And, x, exp);
        let den = self.s.m.cmp(c, pred::EQ, e, zero);
        let s = self.s.m.bin(c, Bin::And, x, sign);
        let r = self.s.m.select(c, den, s, x);
        self.as_float(r)
    }

    /// The hardware's maximum (`max`) or minimum of two floats: as DXIL's
    /// FMax/FMin, but of the operands with denormals read as zeros and with
    /// -0 ordered below +0 (RDNA2 ISA, `v_max_f32`), where the compare and
    /// select DXIL specifies gives either zero.
    fn max_min(&mut self, max: bool, a: V, b: V) -> V {
        let (a, b) = (self.ftz(a), self.ftz(b));
        self.max_min_flushed(max, a, b)
    }

    /// [`Self::max_min`] of operands without denormals.
    fn max_min_flushed(&mut self, max: bool, a: V, b: V) -> V {
        let c = self.cur;
        let r = self.binary(if max { 35 } else { 36 }, a, b);
        let (ia, ib) = (self.as_int(a), self.as_int(b));
        let either = self.s.m.bin(c, Bin::Or, ia, ib);
        let magnitude = self.s.m.i32(0x7FFF_FFFF);
        let either = self.s.m.bin(c, Bin::And, either, magnitude);
        let zero = self.s.m.i32(0);
        let zeros = self.s.m.cmp(c, pred::EQ, either, zero);
        // Of two zeros, +0 unless both are -0 (max), -0 unless neither (min).
        let op = if max { Bin::And } else { Bin::Or };
        let sign = self.s.m.bin(c, op, ia, ib);
        let signed = self.as_float(sign);
        self.s.m.select(c, zeros, signed, r)
    }

    fn fmad(&mut self, a: V, b: V, c: V) -> V {
        self.s.op(
            self.cur,
            "tertiary.f32",
            Attr::ReadNone,
            self.s.t.f32,
            46,
            &[a, b, c],
        )
    }

    /// `f` as an f16 in the low bits, rounded as the program's MODE says at
    /// this point: toward zero where it set that for 16-bit results (the
    /// title does around its 16-bit interpolation), else to nearest even;
    /// f32 denormals read as zero; on the bits, where the host's own
    /// conversion rounds as it likes (toward zero on the RX 6700 XT and WARP).
    // shortcut: rounding toward +-infinity, or a mode not known at this
    // point (paths that set it differently join), rounds to nearest even.
    fn narrow16(&mut self, f: V) -> V {
        let rtz = self.mode16 == Some(3);
        let c = self.cur;
        let x = self.as_int(f);
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let (k13, k16, k31) = (k(self, 13), k(self, 16), k(self, 31));
        let t = self.s.m.bin(c, Bin::LShr, x, k16);
        let m = k(self, 0x8000);
        let sign = self.s.m.bin(c, Bin::And, t, m);
        let m = k(self, 0x7FFF_FFFF);
        let a = self.s.m.bin(c, Bin::And, x, m);
        // Normal halves: the exponent rebased, the mantissa rounded.
        let bias = k(self, 0x3800_0000);
        let rebased = self.s.m.bin(c, Bin::Sub, a, bias);
        let normal = if rtz {
            self.s.m.bin(c, Bin::LShr, rebased, k13)
        } else {
            let one = k(self, 1);
            let lsb = self.s.m.bin(c, Bin::LShr, rebased, k13);
            let lsb = self.s.m.bin(c, Bin::And, lsb, one);
            let half = k(self, 0xFFF);
            let up = self.s.m.bin(c, Bin::Add, rebased, half);
            let up = self.s.m.bin(c, Bin::Add, up, lsb);
            self.s.m.bin(c, Bin::LShr, up, k13)
        };
        let (inf, big) = (k(self, 0x7C00), k(self, if rtz { 0x7BFF } else { 0x7C00 }));
        let over = self.s.m.cmp(c, pred::IUGE, normal, inf);
        let normal = self.s.m.select(c, over, big, normal);
        let infinity = k(self, 0x7F80_0000);
        let is_inf = self.s.m.cmp(c, pred::EQ, a, infinity);
        let normal = self.s.m.select(c, is_inf, inf, normal);
        // Denormal halves: the mantissa shifted into units of 2^-24.
        let k23 = k(self, 23);
        let e = self.s.m.bin(c, Bin::LShr, a, k23);
        let k126 = k(self, 126);
        let s = self.s.m.bin(c, Bin::Sub, k126, e);
        let far = self.s.m.cmp(c, pred::IUGT, s, k31);
        let s = self.s.m.select(c, far, k31, s);
        let (frac, hidden, zero) = (k(self, 0x7F_FFFF), k(self, 0x80_0000), k(self, 0));
        let mant = self.s.m.bin(c, Bin::And, a, frac);
        let mant = self.s.m.bin(c, Bin::Or, mant, hidden);
        let f32_denormal = self.s.m.cmp(c, pred::EQ, e, zero);
        let mant = self.s.m.select(c, f32_denormal, zero, mant);
        let q = self.s.m.bin(c, Bin::LShr, mant, s);
        let den = if rtz {
            q
        } else {
            let one = k(self, 1);
            let s1 = self.s.m.bin(c, Bin::Sub, s, one);
            let half = self.s.m.bin(c, Bin::LShr, mant, s1);
            let half = self.s.m.bin(c, Bin::And, half, one);
            let below = self.s.m.bin(c, Bin::Shl, one, s1);
            let below = self.s.m.bin(c, Bin::Sub, below, one);
            let rest = self.s.m.bin(c, Bin::And, mant, below);
            let rest = self.s.m.cmp(c, pred::NE, rest, zero);
            let rest = self.s.m.cast(c, Cast::ZExt, rest, self.s.t.i32);
            let lsb = self.s.m.bin(c, Bin::And, q, one);
            let tie = self.s.m.bin(c, Bin::Or, rest, lsb);
            let up = self.s.m.bin(c, Bin::And, half, tie);
            self.s.m.bin(c, Bin::Add, q, up)
        };
        let min_normal = k(self, 0x3880_0000);
        let small = self.s.m.cmp(c, pred::IULT, a, min_normal);
        let r = self.s.m.select(c, small, den, normal);
        let is_nan = self.s.m.cmp(c, pred::IUGT, a, infinity);
        // A NaN keeps the top of its payload, quieted (as the RX 6700 XT,
        // RDNA2 like the console, converts).
        let payload = self.s.m.bin(c, Bin::LShr, a, k13);
        let (low10, quiet) = (k(self, 0x3FF), k(self, 0x7E00));
        let payload = self.s.m.bin(c, Bin::And, payload, low10);
        let nan = self.s.m.bin(c, Bin::Or, payload, quiet);
        let r = self.s.m.select(c, is_nan, nan, r);
        self.s.m.bin(c, Bin::Or, r, sign)
    }

    /// `f` as an f16 in the low bits, rounded toward zero: the nearest-even
    /// conversion, one step back toward zero where it rounded away (which
    /// also turns an overflow to infinity into the largest finite half).
    fn f16_toward_zero(&mut self, f: V) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let h = self
            .s
            .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f]);
        let back = self
            .s
            .op(c, "legacyF16ToF32", Attr::ReadNone, self.s.t.f32, 131, &[h]);
        let (fa, ba) = (self.unary(6, f), self.unary(6, back));
        // Ordered: never for NaN.
        let away = self.s.m.cmp(c, pred::OGT, ba, fa);
        let one = self.s.m.i32(1);
        let stepped = self.s.m.bin(c, Bin::Sub, h, one);
        self.s.m.select(c, away, stepped, h)
    }

    /// `(a >> off[4:0]) & ((1 << width) - 1)`, sign-extended from the field's
    /// top bit when `signed`; widths from 32 on take every bit. A signed
    /// field that runs past bit 31 extends from bit 31, as the hardware
    /// does: the shift is arithmetic then.
    fn bfe(&mut self, a: V, off: V, width: V, signed: bool) -> V {
        let c = self.cur;
        let (k31, k32, zero, one, ones) = (
            self.s.m.i32(31),
            self.s.m.i32(32),
            self.s.m.i32(0),
            self.s.m.i32(1),
            self.s.m.i32(u32::MAX),
        );
        let off = self.s.m.bin(c, Bin::And, off, k31);
        let shift = if signed { Bin::AShr } else { Bin::LShr };
        let shifted = self.s.m.bin(c, shift, a, off);
        let full = self.s.m.cmp(c, pred::IUGE, width, k32);
        let w = self.s.m.bin(c, Bin::And, width, k31);
        let mask = self.s.m.bin(c, Bin::Shl, one, w);
        let mask = self.s.m.bin(c, Bin::Sub, mask, one);
        let mask = self.s.m.select(c, full, ones, mask);
        let r = self.s.m.bin(c, Bin::And, shifted, mask);
        if !signed {
            return r;
        }
        // Sign-extend a field narrower than 32 bits (none for width 0).
        let left = self.s.m.bin(c, Bin::Sub, k32, w);
        let left = self.s.m.bin(c, Bin::And, left, k31);
        let t = self.s.m.bin(c, Bin::Shl, r, left);
        let t = self.s.m.bin(c, Bin::AShr, t, left);
        let is0 = self.s.m.cmp(c, pred::EQ, w, zero);
        let keep = self.s.m.bin(c, Bin::Or, full, is0);
        self.s.m.select(c, keep, r, t)
    }

    /// The median of three. With a NaN among them the hardware gives the
    /// minimum of the three instead (the host's minimum skips NaNs, so that
    /// is the smallest number among them).
    fn med3(&mut self, a: V, b: V, third: V) -> V {
        let c = self.cur;
        let (a, b, third) = (self.ftz(a), self.ftz(b), self.ftz(third));
        let lo = self.max_min_flushed(false, a, b);
        let hi = self.max_min_flushed(true, a, b);
        let t = self.max_min_flushed(false, hi, third);
        let median = self.max_min_flushed(true, lo, t);
        let least = self.max_min_flushed(false, lo, third);
        // Integer NaN tests survive host fast-math folding.
        let nan_a = self.is_nan_f32(a);
        let nan_b = self.is_nan_f32(b);
        let nan3 = self.is_nan_f32(third);
        let ab = self.s.m.bin(c, Bin::Or, nan_a, nan_b);
        let nan = self.s.m.bin(c, Bin::Or, ab, nan3);
        self.s.m.select(c, nan, least, median)
    }

    /// `a - floor(a)`, which on the hardware never reaches 1.0: where it
    /// would round to it (a tiny negative `a`), the result is `below_one`,
    /// the largest value under 1.0 in the operation's width. Hosts differ
    /// here (some give 1.0). Comparing against a `threshold` distinct from
    /// `below_one` prevents drivers (observed on NVIDIA) from folding
    /// `r > k ? k : r` into `minNum` (`FMNMX`) and swallowing NaN.
    fn fract(&mut self, a: V, threshold: f32, below_one: f32) -> V {
        let c = self.cur;
        let r = self.unary(22, a);
        let limit = self.s.m.f32(threshold.to_bits());
        let k = self.s.m.f32(below_one.to_bits());
        let over = self.s.m.cmp(c, pred::OGE, r, limit);
        self.s.m.select(c, over, k, r)
    }

    /// Sine, or cosine, of an angle in turns, as the hardware takes it. The
    /// whole turns are taken off first (`x - round_even(x)`, exact, within
    /// half a turn), so a large angle keeps its fraction; scaling to radians
    /// first rounds it away. A zero passes unreduced (the sine of -0 is -0).
    /// The sine of a half turn and the cosine of a quarter turn are exactly
    /// +0 on the hardware, where the host's function of the rounded angle
    /// gives about 1e-7. Infinities and NaN give NaN.
    /// shortcut: below 2^-12 turns the hardware scales by a slightly
    /// different constant than 2π (a last-place difference).
    fn trig_turns(&mut self, x: V, cosine: bool) -> V {
        let c = self.cur;
        let zero = self.s.m.f32(0);
        let whole = self.unary(26, x);
        let part = self.s.m.bin(c, Bin::Sub, x, whole);
        let is_zero = self.s.m.cmp(c, pred::OEQ, x, zero);
        let part = self.s.m.select(c, is_zero, x, part);
        let k = self.s.m.f32(TWO_PI.to_bits());
        let angle = self.s.m.bin(c, Bin::Mul, part, k);
        let r = self.unary(if cosine { 12 } else { 13 }, angle);
        let at = if cosine { 0.25f32 } else { 0.5 };
        let at = self.s.m.f32(at.to_bits());
        let size = self.unary(6, part);
        let exact = self.s.m.cmp(c, pred::OEQ, size, at);
        self.s.m.select(c, exact, zero, r)
    }

    /// The high dword of a 32 × 32-bit product.
    fn mul_hi(&mut self, a: V, b: V, signed: bool) -> V {
        let c = self.cur;
        let i64t = self.s.m.int(64);
        let ext = if signed { Cast::SExt } else { Cast::ZExt };
        let a = self.s.m.cast(c, ext, a, i64t);
        let b = self.s.m.cast(c, ext, b, i64t);
        let m = self.s.m.bin(c, Bin::Mul, a, b);
        let k32 = self.s.m.int_const(i64t, 32);
        let hi = self.s.m.bin(c, Bin::LShr, m, k32);
        self.s.flags |= flags::INT64;
        self.s.m.cast(c, Cast::Trunc, hi, self.s.t.i32)
    }

    /// `0 × anything = 0`, as in D3D9.
    fn legacy_mul(&mut self, a: V, b: V) -> V {
        let c = self.cur;
        let m = self.s.m.bin(c, Bin::Mul, a, b);
        let zero = self.s.m.f32(0);
        let az = self.s.m.cmp(c, pred::OEQ, a, zero);
        let bz = self.s.m.cmp(c, pred::OEQ, b, zero);
        let z = self.s.m.bin(c, Bin::Or, az, bz);
        self.s.m.select(c, z, zero, m)
    }

    // ---- lane booleans ----

    fn atom(&mut self, atom: Atom, at: usize) -> Result<V, Why> {
        match atom {
            Atom::Lane(v) => self.lane_bits.get(&v).copied().ok_or_else(|| {
                self.lane_fail
                    .get(&v)
                    .copied()
                    .unwrap_or("a lane mask from a carry")
            }),
            Atom::Uniform(v) => match self.get(v)? {
                Val::Bit(x) => Ok(x),
                Val::I32(x) => {
                    let z = self.s.m.i32(0);
                    Ok(self.s.m.cmp(at, pred::NE, x, z))
                }
                Val::Missing(w) => Err(w),
            },
            Atom::Phi(id) => {
                if let Some(&v) = self.mask_phis.get(&id) {
                    return Ok(v);
                }
                let &(b, i) = self
                    .phi_at
                    .get(&id)
                    .ok_or("a lane-mask join without a phi")?;
                let blk = self.block[b].ok_or("an unreachable join")?;
                let v = self.s.m.phi(blk, self.s.t.i1);
                self.mask_phis.insert(id, v);
                self.pending_masks.push((v, b, i));
                Ok(v)
            }
            // shortcut: "some lane of the quad is set" is taken as true: the
            // lanes it adds only feed derivatives, and exports keep their own
            // masks. Wrong if such a section had a side effect.
            Atom::Quad(_) => Ok(self.s.m.bool(true)),
            // With console lanes, a mask held as a number is the console's:
            // the lane tests its own bit.
            Atom::Opaque(v) if self.console_lanes => {
                let hi = match self.pair_high(v) {
                    Some(h) => Some(self.get(h)?),
                    None => None,
                };
                let lo = self.get(v)?;
                let word = |w: Val| match w {
                    Val::I32(x) => Ok(x),
                    _ => Err("an opaque lane mask"),
                };
                let lo = word(lo)?;
                let hi = hi.map(word).transpose()?;
                Ok(self.lane_bit(at, lo, hi))
            }
            Atom::Constant(k) if self.console_lanes => {
                let lo = self.s.m.i32(k as u32);
                let hi = self.s.m.i32((k >> 32) as u32);
                Ok(self.lane_bit(at, lo, Some(hi)))
            }
            Atom::Opaque(_) => Err("an opaque lane mask"),
            Atom::Constant(_) => Err("a constant lane mask"),
        }
    }

    /// The bit of lane `lane` in the mask `lo`, `hi`.
    fn lane_bit_of(&mut self, at: usize, lane: V, lo: V, hi: V) -> V {
        let (k31, k32, one, z) = (
            self.s.m.i32(31),
            self.s.m.i32(32),
            self.s.m.i32(1),
            self.s.m.i32(0),
        );
        let low = self.s.m.cmp(at, pred::IULT, lane, k32);
        let word = self.s.m.select(at, low, lo, hi);
        let sh = self.s.m.bin(at, Bin::And, lane, k31);
        let t = self.s.m.bin(at, Bin::LShr, word, sh);
        let t = self.s.m.bin(at, Bin::And, t, one);
        self.s.m.cmp(at, pred::NE, t, z)
    }

    /// The bit position of byte LDS address `a` + `offset` in its dword.
    fn lds_shift(&mut self, a: V, offset: u32) -> V {
        let c = self.cur;
        let (o, three) = (self.s.m.i32(offset), self.s.m.i32(3));
        let at = self.s.m.bin(c, Bin::Add, a, o);
        let byte = self.s.m.bin(c, Bin::And, at, three);
        self.s.m.bin(c, Bin::Shl, byte, three)
    }

    /// The value of the high dword of the wave64 register pair whose low
    /// dword holds `lo` (none in wave32).
    fn pair_high(&self, lo: Value) -> Option<Value> {
        if !self.x.native.wave64 {
            return None;
        }
        let p = self.x.p;
        let pick = |reg: Reg| match reg {
            Reg::S(c) => Some(Reg::S(c + 1)),
            _ => None,
        };
        if let Some(&(reg, _)) = p.inputs.iter().find(|i| i.1 == lo) {
            let h = pick(reg)?;
            return p.inputs.iter().find(|i| i.0 == h).map(|i| i.1);
        }
        if let Some(&(b, i)) = self.phi_at.get(&lo) {
            let h = pick(p.blocks[b].phis[i].reg)?;
            return p.blocks[b]
                .phis
                .iter()
                .find(|q| q.reg == h)
                .map(|q| q.value);
        }
        let b = *self.def_block.get(&lo)?;
        let inst = p.blocks[b]
            .insts
            .iter()
            .find(|i| i.writes.iter().any(|w| w.1 == lo))?;
        let &(reg, _) = inst.writes.iter().find(|w| w.1 == lo)?;
        inst.write(pick(reg)?)
    }

    fn guard(&mut self, b: usize, i: usize) -> Result<Option<V>, Why> {
        match self.x.st.guards[b][i] {
            None => Ok(None),
            Some(g) => Ok(Some(self.boolean(g, self.cur)?)),
        }
    }

    /// Whether every lane outside the guard of the export at (b, i) runs
    /// the `done` export later in block b without its own guard holding,
    /// i.e. is discarded there.
    fn discarded_at_done(&self, b: usize, i: usize) -> bool {
        let guards = &self.x.st.guards[b];
        let g = guards[i].unwrap_or(Bool::TRUE);
        self.exports.iter().any(|(&(eb, ei), e)| {
            e.done && eb == b && ei > i && {
                let done = guards[ei].unwrap_or(Bool::TRUE);
                !self.x.st.bools.meet(&[(done, true), (g, false)])
            }
        })
    }

    // ---- results ----

    fn set(&mut self, v: Option<Value>, val: Val) {
        if let Some(v) = v {
            self.vals.insert(v, val);
        }
    }

    /// Writes VGPR `reg` (or the SGPR of a lane read), under `guard`.
    fn write(&mut self, inst: &Inst, reg: Reg, x: V, guard: Option<V>) {
        let x = match (Self::sdwa_dst(inst), reg) {
            (Some((sel, unused)), Reg::V(_)) => self.place_bits(inst, reg, x, sel, unused),
            _ => x,
        };
        self.write_placed(inst, reg, x, guard);
    }

    /// [`Self::write`] of a dword already placed (SDWA applied).
    fn write_placed(&mut self, inst: &Inst, reg: Reg, x: V, guard: Option<V>) {
        let Some(new) = inst.write(reg) else { return };
        let x = match (guard, reg) {
            (Some(g), Reg::V(_)) => {
                let o = inst.read(reg);
                let read = o.is_some_and(|o| self.read_phis.contains(&o));
                let old = match o.map(|o| self.i32_of(o)) {
                    Some(Ok(o)) => o,
                    // The validator refuses reads of undef.
                    _ => self.s.m.i32(0),
                };
                // What lanes keep is zero where it has no DXIL form (as
                // above), so keeping a phi does not make it live.
                if let Some(o) = o.filter(|_| !read) {
                    self.read_phis.remove(&o);
                }
                let kept = self.s.m.select(self.cur, g, x, old);
                self.s.m.tag(kept, Tag::Guard);
                self.note_raw(new, x, g);
                kept
            }
            _ => x,
        };
        self.vals.insert(new, Val::I32(x));
    }

    /// The SDWA destination selection: a byte or word, and what the other
    /// bits get (0 zeros, 1 the sign above the field, 2 the old bits).
    fn sdwa_dst(inst: &Inst) -> Option<(u8, u8)> {
        let Fields::Valu(v) = &inst.fields else {
            return None;
        };
        let s = v.sdwa?;
        (s.dst_sel < 6).then_some((s.dst_sel, s.dst_unused))
    }

    /// The low byte or word of `x` placed at `sel` in VGPR `reg`.
    fn place_bits(&mut self, inst: &Inst, reg: Reg, x: V, sel: u8, unused: u8) -> V {
        let c = self.cur;
        let (width, at) = if sel < 4 {
            (8u32, 8 * u32::from(sel))
        } else {
            (16, 16 * u32::from(sel - 4))
        };
        let left = self.s.m.i32(32 - width);
        let t = self.s.m.bin(c, Bin::Shl, x, left);
        let right = self.s.m.i32(32 - width - at);
        if unused == 1 {
            // Bits below the field shift in as zeros.
            return self.s.m.bin(c, Bin::AShr, t, right);
        }
        let field = self.s.m.bin(c, Bin::LShr, t, right);
        if unused != 2 {
            return field;
        }
        let old = match inst.read(reg).map(|o| self.i32_of(o)) {
            Some(Ok(o)) => o,
            _ => self.s.m.i32(0),
        };
        let keep = self.s.m.i32(!((((1u64 << width) - 1) as u32) << at));
        let old = self.s.m.bin(c, Bin::And, old, keep);
        self.s.m.bin(c, Bin::Or, old, field)
    }

    fn write_f(&mut self, inst: &Inst, f: V, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let mut f = f;
        match v.omod {
            0 => {}
            k => {
                // On the flushed result; a zero or denormal product comes out
                // +0 (measured on RDNA2 with IEEE mode off).
                let factor = [1.0f32, 2.0, 4.0, 0.5][usize::from(k)];
                let c = self.cur;
                let k = self.s.m.f32(factor.to_bits());
                let r = self.ftz(f);
                let r = self.s.m.bin(c, Bin::Mul, r, k);
                let x = self.as_int(r);
                let (exp, zero) = (self.s.m.i32(0x7F80_0000), self.s.m.i32(0));
                let e = self.s.m.bin(c, Bin::And, x, exp);
                let tiny = self.s.m.cmp(c, pred::EQ, e, zero);
                let x = self.s.m.select(c, tiny, zero, x);
                f = self.as_float(x);
            }
        }
        if v.clamp {
            f = self.unary(7, f);
        }
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a float result outside a VGPR");
        };
        let x = self.as_int(f);
        self.write(inst, Reg::V(reg), x, guard);
        if Self::sdwa_dst(inst).is_none()
            && let Some(new) = inst.write(Reg::V(reg))
        {
            if guard.is_none() {
                self.floats.insert(new, f);
            } else {
                self.note_raw_float(new, f);
            }
        }
        Ok(())
    }

    fn write_i(&mut self, inst: &Inst, x: V, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        if v.clamp || v.omod != 0 {
            return Err("integer output modifiers");
        }
        match v.dst {
            Some(Operand::Vector { reg, .. }) => self.write(inst, Reg::V(reg), x, guard),
            Some(Operand::Scalar { code, .. }) => self.write(inst, Reg::S(code), x, None),
            _ => return Err("a result without a register"),
        }
        Ok(())
    }

    /// Starts code only the lanes where `guard` holds run (a store under a
    /// lane guard): returns where it started and the join block.
    fn begin_guarded(&mut self, guard: Option<V>) -> Option<(usize, usize)> {
        let g = guard?;
        let (then, join) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(self.cur, Term::CondBr(g, then, join));
        let head = self.cur;
        self.cur = then;
        Some((head, join))
    }

    /// Ends guarded code; each (value inside, value outside) becomes a phi.
    fn end_guarded(&mut self, at: Option<(usize, usize)>, values: &[(V, V)]) -> Vec<V> {
        let Some((head, join)) = at else {
            return values.iter().map(|v| v.0).collect();
        };
        let then = self.cur;
        self.s.m.terminate(then, Term::Br(join));
        self.cur = join;
        values
            .iter()
            .map(|&(inside, outside)| {
                let ty = self.s.m.type_of(inside);
                let phi = self.s.m.phi(join, ty);
                self.s.m.set_phi(phi, vec![(inside, then), (outside, head)]);
                phi
            })
            .collect()
    }

    /// Mesh: the counts and the primitives no lane exports. The console's
    /// counts come from one lane's allocation request, which DXIL cannot
    /// express (one call, dominating every output write).
    // shortcut: the counts are the declared maxima and each lane first culls
    // its primitive (all indices 0, zero area), so primitives past the
    // program's count are culled and vertices past it unused; this costs
    // the host the unused vertex and primitive slots. Pass the program's own
    // counts if that shows in frame time.
    fn mesh_prologue(&mut self, mut mesh: super::shader::Mesh) -> Result<(), Why> {
        let threads = self.s.threads.expect("mesh")[0];
        if mesh.topology != 2 {
            return Err("a mesh shader not drawing triangles");
        }
        if threads > 128 || mesh.max_vertices > 256 || mesh.max_primitives > 256 {
            return Err("a mesh shader beyond the host's limits");
        }
        // D3D12 caps the outputs of the declared maxima at 32 KiB. Lane n
        // writes only vertex n and primitive n, so maxima past the lanes are
        // never reached: they come down to the lane count when the outputs
        // do not fit.
        use super::container::{MESH_OUTPUT_LIMIT, mesh_output_bytes};
        let (outputs, prims) = (&self.s.outputs, &self.s.prims);
        let fits = |m: super::shader::Mesh| {
            mesh_output_bytes(outputs, prims, m.max_vertices, m.max_primitives) <= MESH_OUTPUT_LIMIT
        };
        if !fits(mesh) {
            mesh.max_vertices = mesh.max_vertices.min(threads);
            mesh.max_primitives = mesh.max_primitives.min(threads);
            // shortcut: outputs past the cap even at the lane count are
            // still emitted with the lower maxima (drivers take them);
            // refuse them here if one hangs.
            // Experiment (`VICTORY_MESH_OUTPUT_STRICT=1`): they are refused.
            if !fits(mesh) && std::env::var_os("VICTORY_MESH_OUTPUT_STRICT").is_some() {
                return Err("a mesh shader whose outputs pass the host's 32 KiB");
            }
            self.s.mesh = Some(mesh);
        }
        let e = self.s.entry;
        let void = self.s.t.void;
        let (v, p) = (
            self.s.m.i32(mesh.max_vertices),
            self.s.m.i32(mesh.max_primitives),
        );
        self.s
            .op(e, "setMeshOutputCounts", Attr::NoUnwind, void, 168, &[v, p]);
        let flat = self.mesh_thread();
        let z = self.s.m.i32(0);
        // Every primitive of the declared count starts culled, also those
        // past the lanes (a count of 256 with 128 lanes): lane n culls
        // primitives n, n + lanes, ... The host draws all it was promised,
        // and one never written has whatever indices its memory held: long
        // triangles over the whole target, each pixel of them shaded.
        let passes = mesh.max_primitives.div_ceil(threads.max(1));
        for pass in 0..passes {
            let index = if pass == 0 {
                flat
            } else {
                let past = self.s.m.i32(pass * threads);
                self.s.m.bin(self.cur, Bin::Add, flat, past)
            };
            let guard = ((pass + 1) * threads > mesh.max_primitives)
                .then(|| self.s.m.cmp(self.cur, pred::IULT, index, p));
            let at = self.begin_guarded(guard);
            self.s.op(
                self.cur,
                "emitIndices",
                Attr::NoUnwind,
                void,
                169,
                &[index, z, z, z],
            );
            self.end_guarded(at, &[]);
        }
        Ok(())
    }

    /// Mesh: the thread's index in the group, which is its vertex and
    /// primitive index.
    fn mesh_thread(&mut self) -> V {
        if let Some(f) = self.flat {
            return f;
        }
        let e = self.s.entry;
        let f = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            self.s.t.i32,
            96,
            &[],
        );
        self.flat = Some(f);
        f
    }

    /// The console wave size.
    fn wave(&self) -> u32 {
        if self.x.native.wave64 { 64 } else { 32 }
    }

    /// `x` of lane `src` (in this lane's quad) through quad reads, whose
    /// lane must be a constant.
    fn quad_pick(&mut self, x: V, src: V) -> V {
        self.s.flags |= flags::WAVE_OPS;
        let c = self.cur;
        let three = self.s.m.i32(3);
        let q = self.s.m.bin(c, Bin::And, src, three);
        let mut r = x;
        for n in 0..4 {
            let kn = self.s.m.i32(n);
            let read = self.s.op(
                c,
                "quadReadLaneAt.i32",
                Attr::NoUnwind,
                self.s.t.i32,
                122,
                &[x, kn],
            );
            let is = self.s.m.cmp(c, pred::EQ, q, kn);
            r = self.s.m.select(c, is, read, r);
        }
        r
    }

    /// Whether a quad-local read must reach helper lanes: in a pixel program
    /// every quad a triangle edge crosses has some (the console's whole-quad
    /// mode runs them; discarded pixels stay helpers too), and only quad
    /// reads see them on the host (wave reads give 0 from them on AMD: wrong
    /// derivatives of computed values along every triangle edge).
    fn helpers_read(&self, quad: bool) -> bool {
        quad && self.s.pixel
    }

    /// `v_readfirstlane_b32`'s result `first` of `own` in a pixel shader:
    /// a helper lane takes its own value instead.
    ///
    /// On the console, whole-quad mode puts helper lanes in EXEC, so a
    /// waterfall loop (read the first lane's value, run the lanes with that
    /// value, take them out of EXEC) serves helper lanes too. On the host,
    /// wave operations leave helper lanes out (DXIL before
    /// `WaveOpsIncludeHelperLanes`): once only helper lanes are left in such
    /// a loop, `WaveReadLaneFirst` reads no lane, no lane matches, and the
    /// loop runs to [`LOOP_LIMIT`] in every such lane: draws of 0.3 to 1.3 s
    /// each, then DEVICE_HUNG. With its own value a helper lane matches in
    /// the first pass and leaves; what a helper lane computes only feeds
    /// derivatives, and its stores are dropped.
    fn helper_reads_own(&mut self, own: V, first: V) -> V {
        if !self.s.pixel {
            return first;
        }
        let c = self.cur;
        let helper = self
            .s
            .op(c, "isHelperLane.i1", Attr::ReadOnly, self.s.t.i1, 221, &[]);
        self.s.m.select(c, helper, own, first)
    }

    fn read_lane(&mut self, x: V, lane: V) -> V {
        self.s.op(
            self.cur,
            "waveReadLaneAt.i32",
            Attr::NoUnwind,
            self.s.t.i32,
            117,
            &[x, lane],
        )
    }

    /// The float value of a register value.
    fn float_of(&mut self, v: Value) -> Result<V, Why> {
        if let Some(&f) = self.floats.get(&v) {
            return Ok(f);
        }
        if let Some(f) = self.raw_float(v) {
            return Ok(f);
        }
        let x = self.i32_of(v)?;
        Ok(self.as_float(x))
    }

    /// The values of VGPRs `reg..reg + n` as `inst` reads them.
    fn vgprs(&mut self, inst: &Inst, op: &Operand, n: u8) -> Result<Vec<V>, Why> {
        let Operand::Vector { reg, .. } = *op else {
            return Err("a vector operand");
        };
        (0..n)
            .map(|k| {
                let v = inst.read(Reg::V(reg + k)).ok_or("an unread operand")?;
                self.source(v)
            })
            .collect()
    }

    /// LDS (thread-group shared memory) and `ds_swizzle`.
    fn ds(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Ds {
            dst,
            addr,
            data,
            offset0,
            offset1,
            gds,
        } = &inst.fields
        else {
            unreachable!()
        };
        if *gds {
            return Err("GDS");
        }
        let name = inst.name();
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let offset = u32::from(*offset0) | u32::from(*offset1) << 8;
        let dst_reg = match dst {
            Some(Operand::Vector { reg, .. }) => Some(*reg),
            _ => None,
        };
        if name == "ds_swizzle_b32" {
            // Bit 15 set: each lane of a quad picks a lane of it; else lanes
            // of each group of 32 combine their index with and/or/xor masks.
            let quad = offset & 0x8000 != 0
                || offset & 0x1C == 0x1C && offset & 0x380 == 0 && offset >> 10 & 0x1F < 4;
            // Mesh: host quads are threads 4n..4n+3 of a 1D group, as the
            // console's, so quad-local swizzles read through quad operations.
            let mesh_quad = self.s.mesh.is_some() && quad;
            if !mesh_quad {
                self.cross_lane(quad)?;
            }
            // The source sits in the address field.
            let x = self.vgprs(inst, addr.as_ref().ok_or("a swizzle source")?, 1)?[0];
            let lane = self.lane_index();
            let k = |l: &mut Self, v: u32| l.s.m.i32(v);
            let src = if offset & 0x8000 != 0 {
                let (three, one) = (k(self, 3), k(self, 1));
                let sel = self.s.m.bin(c, Bin::And, lane, three);
                let sh = self.s.m.bin(c, Bin::Shl, sel, one);
                let pattern = k(self, offset & 0xFF);
                let pick = self.s.m.bin(c, Bin::LShr, pattern, sh);
                let pick = self.s.m.bin(c, Bin::And, pick, three);
                let not3 = k(self, !3);
                let base = self.s.m.bin(c, Bin::And, lane, not3);
                self.s.m.bin(c, Bin::Or, base, pick)
            } else {
                let and = k(self, offset & 0x1F | !0x1F);
                let or = k(self, offset >> 5 & 0x1F);
                let xor = k(self, offset >> 10 & 0x1F);
                let t = self.s.m.bin(c, Bin::And, lane, and);
                let t = self.s.m.bin(c, Bin::Or, t, or);
                self.s.m.bin(c, Bin::Xor, t, xor)
            };
            let r = if mesh_quad || self.helpers_read(quad) {
                self.quad_pick(x, src)
            } else {
                self.read_lane(x, src)
            };
            let reg = dst_reg.ok_or("a swizzle result")?;
            self.write(inst, Reg::V(reg), r, guard);
            return Ok(());
        }
        if name == "ds_bpermute_b32" {
            if self.exchange.as_ref().is_some_and(|e| !e.1) {
                return Err("a lane permute across two host waves");
            }
            // Each lane reads `data` of the lane its address plus the offset
            // names in dwords (modulo the wave); 0 from a lane EXEC leaves out.
            self.cross_lane(false)?;
            let a = self.vgprs(inst, addr.as_ref().ok_or("a permute address")?, 1)?[0];
            let x = self.vgprs(inst, data.first().ok_or("a permute source")?, 1)?[0];
            let k = |l: &mut Self, v: u32| l.s.m.i32(v);
            let (o, two, last) = (k(self, offset), k(self, 2), k(self, self.wave() - 1));
            let at = self.s.m.bin(c, Bin::Add, a, o);
            let lane = self.s.m.bin(c, Bin::LShr, at, two);
            let lane = self.s.m.bin(c, Bin::And, lane, last);
            let r = self.read_lane(x, lane);
            let on = guard.unwrap_or_else(|| self.s.m.bool(true));
            let i32t = self.s.t.i32;
            let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
            let ballot = self
                .s
                .op(c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[on]);
            let (lo, hi) = (
                self.s.m.extract(c, ballot, 0),
                self.s.m.extract(c, ballot, 1),
            );
            let running = self.lane_bit_of(c, lane, lo, hi);
            let z = k(self, 0);
            let r = self.s.m.select(c, running, r, z);
            let reg = dst_reg.ok_or("a permute result")?;
            self.write(inst, Reg::V(reg), r, guard);
            return Ok(());
        }
        if self.s.lds_dwords == 0 {
            return Err("LDS the program does not allocate");
        }
        let a = self.vgprs(inst, addr.as_ref().ok_or("an LDS address")?, 1)?[0];
        // shortcut: LDS addresses are taken as aligned to what they access
        // and in bounds (out-of-bounds console accesses read 0 and drop
        // writes).
        let dword = |l: &mut Self, bytes: u32| -> V {
            let c = l.cur;
            let o = l.s.m.i32(bytes);
            let at = l.s.m.bin(c, Bin::Add, a, o);
            let two = l.s.m.i32(2);
            let index = l.s.m.bin(c, Bin::LShr, at, two);
            let lds = l.s.lds();
            l.s.m.element(c, lds, index)
        };
        let width = |n: &str| match n.rsplit('_').next() {
            Some("b64") => 2,
            Some("b96") => 3,
            Some("b128") => 4,
            _ => 1,
        };
        // (byte offset, dwords) per element; read2/write2 scale their two
        // offsets by the element size (×64 for st64).
        let pair = |n: &str| -> Vec<(u32, u32)> {
            let w = width(n);
            let scale = 4 * w * if n.contains("st64") { 64 } else { 1 };
            vec![
                (u32::from(*offset0) * scale, w),
                (u32::from(*offset1) * scale, w),
            ]
        };
        if let Some(rest) = name.strip_prefix("ds_read") {
            let elements = if rest.starts_with('2') {
                pair(name)
            } else if matches!(rest, "_b32" | "_b64" | "_b96" | "_b128") {
                vec![(offset, width(name))]
            } else {
                // A byte or half, extended (`_d16`: into the low half,
                // `_d16_hi`: the high half, the other half kept).
                let (bits, signed) = match rest.trim_end_matches("_hi").trim_end_matches("_d16") {
                    "_u8" => (8, false),
                    "_i8" => (8, true),
                    "_u16" => (16, false),
                    "_i16" => (16, true),
                    _ => return Err("an LDS read of bytes or halves"),
                };
                let reg = dst_reg.ok_or("an LDS read result")?;
                let p = dword(self, offset);
                let x = self.s.m.load(c, p);
                let sh = self.lds_shift(a, offset);
                let x = self.s.m.bin(c, Bin::LShr, x, sh);
                let left = self.s.m.i32(32 - bits);
                let x = self.s.m.bin(c, Bin::Shl, x, left);
                let x = self
                    .s
                    .m
                    .bin(c, if signed { Bin::AShr } else { Bin::LShr }, x, left);
                let x = if rest.contains("_d16") {
                    let old = match inst.read(Reg::V(reg)) {
                        Some(o) => self.i32_of(o)?,
                        None => self.s.m.i32(0),
                    };
                    let hi = rest.ends_with("_hi");
                    let keep = self.s.m.i32(if hi { 0xFFFF } else { 0xFFFF_0000 });
                    let old = self.s.m.bin(c, Bin::And, old, keep);
                    let x = if hi {
                        let k16 = self.s.m.i32(16);
                        self.s.m.bin(c, Bin::Shl, x, k16)
                    } else {
                        let low = self.s.m.i32(0xFFFF);
                        self.s.m.bin(c, Bin::And, x, low)
                    };
                    self.s.m.bin(c, Bin::Or, old, x)
                } else {
                    x
                };
                self.write(inst, Reg::V(reg), x, guard);
                return Ok(());
            };
            let reg = dst_reg.ok_or("an LDS read result")?;
            let mut n = 0;
            for (off, w) in elements {
                for k in 0..w {
                    let p = dword(self, off + 4 * k);
                    let x = self.s.m.load(self.cur, p);
                    self.write(inst, Reg::V(reg + n), x, guard);
                    n += 1;
                }
            }
            return Ok(());
        }
        if let Some(rest) = name.strip_prefix("ds_write") {
            let elements = if rest.starts_with('2') {
                pair(name)
            } else if matches!(rest, "_b32" | "_b64" | "_b96" | "_b128") {
                vec![(offset, width(name))]
            } else {
                // A byte or half (`_d16_hi`: the high half of the data) into
                // its dword through atomics, so lanes writing the other parts
                // of the dword keep theirs.
                // shortcut: lanes writing the same part in one instruction
                // leave the OR of theirs (the console keeps one).
                let bits = match rest.trim_end_matches("_d16_hi") {
                    "_b8" => 8,
                    "_b16" => 16,
                    _ => return Err("an LDS write of bytes or halves"),
                };
                let x = self.vgprs(inst, data.first().ok_or("an LDS write")?, 1)?[0];
                let x = if rest.ends_with("_d16_hi") {
                    let k16 = self.s.m.i32(16);
                    self.s.m.bin(c, Bin::LShr, x, k16)
                } else {
                    x
                };
                let mask = self.s.m.i32((1 << bits) - 1);
                let x = self.s.m.bin(c, Bin::And, x, mask);
                let sh = self.lds_shift(a, offset);
                let set = self.s.m.bin(c, Bin::Shl, x, sh);
                let part = self.s.m.bin(c, Bin::Shl, mask, sh);
                let ones = self.s.m.i32(u32::MAX);
                let keep = self.s.m.bin(c, Bin::Xor, part, ones);
                let p = dword(self, offset);
                let at = self.begin_guarded(guard);
                use super::bitcode::Rmw;
                self.s.m.atomic(self.cur, Rmw::And, p, keep);
                self.s.m.atomic(self.cur, Rmw::Or, p, set);
                self.end_guarded(at, &[]);
                return Ok(());
            };
            let mut stores = Vec::new();
            for (e, (off, w)) in elements.into_iter().enumerate() {
                let vals = self.vgprs(inst, data.get(e).ok_or("an LDS write")?, w as u8)?;
                for (k, x) in vals.into_iter().enumerate() {
                    stores.push((off + 4 * k as u32, x));
                }
            }
            let at = self.begin_guarded(guard);
            for (off, x) in stores {
                let p = dword(self, off);
                self.s.m.store(self.cur, p, x);
            }
            self.end_guarded(at, &[]);
            return Ok(());
        }
        // Atomics: the old value returns with `_rtn`.
        let op = name
            .strip_prefix("ds_")
            .map(|n| n.replace("_rtn", ""))
            .ok_or("an LDS operation")?;
        use super::bitcode::Rmw;
        let rmw = match op.as_str() {
            "add_u32" => Some(Rmw::Add),
            "sub_u32" => Some(Rmw::Sub),
            "and_b32" => Some(Rmw::And),
            "or_b32" => Some(Rmw::Or),
            "xor_b32" => Some(Rmw::Xor),
            "min_i32" => Some(Rmw::Min),
            "max_i32" => Some(Rmw::Max),
            "min_u32" => Some(Rmw::UMin),
            "max_u32" => Some(Rmw::UMax),
            "wrxchg_b32" => Some(Rmw::Xchg),
            "cmpst_b32" | "max_f32" | "min_f32" => None,
            _ => return Err("an LDS operation"),
        };
        let v0 = self.vgprs(inst, data.first().ok_or("an LDS atomic")?, 1)?[0];
        let v1 = match data.get(1) {
            Some(d) => Some(self.vgprs(inst, d, 1)?[0]),
            None => None,
        };
        let old_reg = dst_reg.and_then(|r| inst.read(Reg::V(r)).map(|v| (r, v)));
        let outside = match old_reg {
            Some((_, v)) => self.i32_of(v).unwrap_or(self.s.m.i32(0)),
            None => self.s.m.i32(0),
        };
        let p = dword(self, offset);
        if let Some(max) = match op.as_str() {
            "max_f32" => Some(true),
            "min_f32" => Some(false),
            _ => None,
        } {
            let at = self.begin_guarded(guard);
            let r = self.lds_float_min_max(p, v0, max);
            let r = self.end_guarded(at, &[(r, outside)])[0];
            if let Some(reg) = dst_reg {
                self.write(inst, Reg::V(reg), r, None);
            }
            return Ok(());
        }
        let at = self.begin_guarded(guard);
        let r = match rmw {
            Some(op) => self.s.m.atomic(self.cur, op, p, v0),
            // Compare with data0, store data1.
            None => self
                .s
                .m
                .cmpxchg(self.cur, p, v0, v1.ok_or("an LDS compare")?),
        };
        let r = self.end_guarded(at, &[(r, outside)])[0];
        if let Some(reg) = dst_reg {
            self.write(inst, Reg::V(reg), r, None);
        }
        Ok(())
    }

    // ---- instructions ----

    /// `ds_max_f32` (`max`) or `ds_min_f32` on the LDS dword at `p` with
    /// the float bits `data`; the old bits return.
    ///
    /// DXIL has no float atomics on group-shared memory, so this is a
    /// compare-exchange loop around [`Lower::lds_float_pick`], which gives
    /// what RDNA2 writes (measured on gfx1036, reported on the pull
    /// request): an sNaN, data first, then memory, is written quieted;
    /// otherwise a NaN loses to a number (two quiet NaNs give the data);
    /// numbers compare as floats with -0 below +0.
    // shortcut: f32 denormals compare as kept; with the console's MODE
    // flushing them they would order as ±0 first (MODE is not modelled).
    fn lds_float_min_max(&mut self, p: V, data: V, max: bool) -> V {
        let pre = self.cur;
        let init = self.s.m.load(pre, p);
        let (lp, exit) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(pre, Term::Br(lp));
        let i32t = self.s.t.i32;
        let old = self.s.m.phi(lp, i32t);
        self.cur = lp;
        let new = self.lds_float_pick(old, data, max);
        let prev = self.s.m.cmpxchg(lp, p, old, new);
        let done = self.s.m.cmp(lp, pred::EQ, prev, old);
        self.s.m.set_phi(old, vec![(init, pre), (prev, lp)]);
        self.s.m.terminate(lp, Term::CondBr(done, exit, lp));
        self.cur = exit;
        old
    }

    /// What `ds_max_f32` (`max`) or `ds_min_f32` writes over the dword
    /// `mem` with `data` (float bits; see [`Lower::lds_float_min_max`]).
    fn lds_float_pick(&mut self, mem: V, data: V, max: bool) -> V {
        let c = self.cur;
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let (abs_mask, inf, quiet, zero) = (
            k(self, 0x7FFF_FFFF),
            k(self, 0x7F80_0000),
            k(self, 0x40_0000),
            k(self, 0),
        );
        let nan = |l: &mut Self, x: V| {
            let m = l.s.m.bin(c, Bin::And, x, abs_mask);
            l.s.m.cmp(c, pred::IUGT, m, inf)
        };
        let snan = |l: &mut Self, x: V| {
            let n = nan(l, x);
            let q = l.s.m.bin(c, Bin::And, x, quiet);
            let sig = l.s.m.cmp(c, pred::EQ, q, zero);
            l.s.m.bin(c, Bin::And, n, sig)
        };
        // A total order of the numbers as signed integers, -0 below +0:
        // negative floats' magnitude bits reversed.
        let key = |l: &mut Self, x: V| {
            let k31 = l.s.m.i32(31);
            let sign = l.s.m.bin(c, Bin::AShr, x, k31);
            let flip = l.s.m.bin(c, Bin::And, sign, abs_mask);
            l.s.m.bin(c, Bin::Xor, x, flip)
        };
        let (dk, mk) = (key(self, data), key(self, mem));
        let take = self
            .s
            .m
            .cmp(c, if max { pred::SGT } else { pred::SLT }, dk, mk);
        let r = self.s.m.select(c, take, data, mem);
        let (dn, mn) = (nan(self, data), nan(self, mem));
        // One NaN: the number; two quiet NaNs: the data.
        let r = self.s.m.select(c, mn, data, r);
        let both = self.s.m.bin(c, Bin::And, dn, mn);
        let only_data = {
            let t = self.s.m.bool(true);
            let not_both = self.s.m.bin(c, Bin::Xor, both, t);
            self.s.m.bin(c, Bin::And, dn, not_both)
        };
        let r = self.s.m.select(c, only_data, mem, r);
        // Signalling NaNs win, quieted: the data's first.
        let qm = self.s.m.bin(c, Bin::Or, mem, quiet);
        let sm = snan(self, mem);
        let r = self.s.m.select(c, sm, qm, r);
        let qd = self.s.m.bin(c, Bin::Or, data, quiet);
        let sd = snan(self, data);
        self.s.m.select(c, sd, qd, r)
    }

    fn inst(&mut self, b: usize, i: usize) -> Result<(), Stop> {
        let inst: &'a Inst = &self.x.p.blocks[b].insts[i];
        let offset = inst.machine.offset;
        let name = inst.name();
        self.local_ok = name.starts_with("s_bcnt1_i32_")
            || name.starts_with("s_ff1_i32_")
            || name.starts_with("v_mbcnt_");
        self.mask_ok = match self.mask_data {
            MaskData::Console => true,
            MaskData::Host => {
                name.starts_with("s_bcnt1_i32_")
                    || name.starts_with("s_ff1_i32_")
                    || name.starts_with("v_mbcnt_")
                    || matches!(
                        name,
                        "s_cmp_eq_u32" | "s_cmp_lg_u32" | "s_cmp_eq_u64" | "s_cmp_lg_u64"
                    )
            }
            // Mesh: only "do all lanes hold the first lane's value?" (`uniform_test`), which
            // each host wave answers for itself.
            MaskData::None => self.uniform_test(inst),
        };
        let effect = matches!(inst.fields, Fields::Exp { .. })
            || matches!(inst.fields, Fields::Flat { .. }) && !name.starts_with("global_load_")
            || name.contains("store")
            || name.contains("atomic")
            || name == "s_barrier"
            || matches!(inst.fields, Fields::Ds { .. })
                && !(name.starts_with("ds_read")
                    || name.starts_with("ds_swizzle")
                    || name.contains("permute"));
        if self.split_lds
            && matches!(inst.fields, Fields::Ds { .. })
            && !(name.starts_with("ds_swizzle") || name.contains("permute"))
            && self.lds_meet.as_ref().is_none_or(|(all, _)| all[b])
        {
            // the two host waves of a console wave meet before each LDS access: what the other
            // half wrote is visible, and what it still reads is not overwritten
            let mode = self.s.m.i32(meeting_mode());
            let void = self.s.t.void;
            self.s
                .op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
        }
        self.raw_use = self.raw_use_for(b, i, inst).unwrap_or(None);
        let r = match &inst.fields {
            Fields::Sop { .. } => self.sop(inst),
            Fields::Sopp { .. } if name == "s_barrier" && self.no_barrier.contains(&b) => Ok(()),
            Fields::Sopp { .. } if name == "s_barrier" => {
                // Thread-group sync with a shared-memory fence, and a UAV
                // fence when the program writes UAVs (the waits before an
                // s_barrier make those writes visible on the console).
                let mut mode = if self.writes_uavs { 1 | 2 | 8 } else { 1 | 8 };
                // Without the wait for the group (bit 0) where not every
                // lane arrives, or where the switch says so.
                let waits = match super::group_sync() {
                    0 => false,
                    1 => !self.fence_only.contains(&b),
                    _ => true,
                };
                if !waits {
                    mode &= !1;
                }
                let mode = self.s.m.i32(mode);
                let void = self.s.t.void;
                self.s
                    .op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
                Ok(())
            }
            Fields::Sopp { .. } => Ok(()),
            Fields::Smem { .. } => self.smem(inst, (b, i)),
            Fields::Valu(_) if name.starts_with("v_interp_") => self.interp(inst, (b, i)),
            Fields::Valu(_) => self.valu(inst, b, i),
            Fields::Vintrp { .. } => self.interp(inst, (b, i)),
            // The ray and node test. A failure is a refusal at once: an
            // instruction without a side effect otherwise only stops the
            // program where its results are needed, and two trace programs
            // came out without their test that way.
            Fields::Image { .. } if name.starts_with("image_bvh") => {
                return self.bvh(inst, b, i).map_err(|why| unsupported(offset, why));
            }
            Fields::Image { .. } => self.image(inst, b, i),
            Fields::Buffer { .. } => self.buffer(inst, b, i),
            Fields::Exp { .. } => self.export(b, i),
            Fields::Ds { .. } => self.ds(inst, b, i),
            Fields::Flat { .. } if name.starts_with("global_") => self.flat(inst, b, i),
            Fields::Flat { .. } => Err("flat memory"),
        };
        self.raw_use = None;
        match r {
            Ok(()) => Ok(()),
            Err(why) if effect => Err(unsupported(offset, why)),
            Err(why) => {
                for &(_, v) in &inst.writes {
                    self.vals.insert(v, Val::Missing(why));
                    self.raw.remove(&v);
                }
                Ok(())
            }
        }
    }

    fn is_mask(&self, v: Option<Value>) -> bool {
        v.is_some_and(|v| self.x.st.masks.contains_key(&v))
    }

    fn sop(&mut self, inst: &Inst) -> Result<(), Why> {
        let Fields::Sop { dst, src, imm } = &inst.fields else {
            unreachable!()
        };
        let name = inst.name();
        let scc = inst.write(Reg::Scc);
        let dst_code = match dst {
            Some(Operand::Scalar { code, .. }) => Some(*code),
            _ => None,
        };
        let lo = dst_code.and_then(|c| inst.write(Reg::S(c)));
        if inst.write(Reg::S(EXEC)).is_some() || self.is_mask(lo) {
            return Err(LANE_MASK);
        }
        if matches!(
            name,
            "s_setreg_imm32_b32" | "s_setreg_b32" | "s_swappc_b64" | "s_version"
        ) || name.starts_with("s_waitcnt")
        {
            // shortcut: the float MODE register is not modelled; s_swappc_b64 is the
            // vertex fetch call, whose results arrive as inputs.
            return Err("a register the emitter does not model");
        }
        let c = self.cur;
        let src32 = |l: &mut Self, k: usize| -> Result<V, Why> {
            // SOPK arithmetic reads its destination as the first source.
            if k == 0 && matches!(name, "s_addk_i32" | "s_mulk_i32") {
                let d = dst.as_ref().ok_or("a missing operand")?;
                return l.operand(inst, d);
            }
            let op = src.get(k).ok_or("a missing operand")?;
            l.operand(inst, op)
        };
        let sext16 = |l: &mut Self| l.s.m.i32(i32::from(imm.unwrap_or(0) as i16) as u32);
        // Dword `h` of 64-bit source `k` (inline constants sign-extend).
        let half = |l: &mut Self, k: usize, h: u8| -> Result<V, Why> {
            match src.get(k) {
                Some(&Operand::Scalar { code, .. }) if code != NULL => {
                    let v = inst.read(Reg::S(code + h)).ok_or("an unread operand")?;
                    l.i32_of(v)
                }
                Some(op) if h == 0 => l.operand(inst, op),
                Some(Operand::Const(k)) => {
                    let x = crate::resource::inline(*k).ok_or("a special operand")?;
                    Ok(l.s.m.i32(if (x as i32) < 0 { u32::MAX } else { 0 }))
                }
                // Of a signed operand, a literal sign-extends too.
                Some(&Operand::Literal(x)) if name.ends_with("_i64") => {
                    Ok(l.s.m.i32(if (x as i32) < 0 { u32::MAX } else { 0 }))
                }
                _ => Ok(l.s.m.i32(0)),
            }
        };
        if let Some(op) = name.strip_prefix("s_bitcmp") {
            // SCC is whether bit src1 (mod 32, or mod 64 for the 64-bit form)
            // of src0 is clear (bitcmp0) or set (bitcmp1).
            let (bit, k31) = (src32(self, 1)?, self.s.m.i32(31));
            let idx = self.s.m.bin(c, Bin::And, bit, k31);
            let word = if op.ends_with("_b64") {
                let (a, b) = (half(self, 0, 0)?, half(self, 0, 1)?);
                let k32 = self.s.m.i32(32);
                let upper = self.s.m.bin(c, Bin::And, bit, k32);
                let z = self.s.m.i32(0);
                let upper = self.s.m.cmp(c, pred::NE, upper, z);
                self.s.m.select(c, upper, b, a)
            } else {
                src32(self, 0)?
            };
            let shifted = self.s.m.bin(c, Bin::LShr, word, idx);
            let one = self.s.m.i32(1);
            let tested = self.s.m.bin(c, Bin::And, shifted, one);
            let want = self.s.m.i32(u32::from(op.starts_with('1')));
            let r = self.s.m.cmp(c, pred::EQ, tested, want);
            self.set(scc, Val::Bit(r));
            return Ok(());
        }
        if let Some(op) = name.strip_prefix("s_cmp_").filter(|o| o.ends_with("_u64")) {
            // eq and lg only: both dwords.
            let mut r = None;
            for h in 0..2 {
                let (a, b) = (half(self, 0, h)?, half(self, 1, h)?);
                let e = self.s.m.cmp(c, pred::EQ, a, b);
                r = Some(match r {
                    Some(x) => self.s.m.bin(c, Bin::And, x, e),
                    None => e,
                });
            }
            let mut r = r.expect("two dwords");
            if op.starts_with("lg") {
                let t = self.s.m.bool(true);
                r = self.s.m.bin(c, Bin::Xor, r, t);
            }
            self.set(scc, Val::Bit(r));
            return Ok(());
        }
        if let Some(op) = name.strip_prefix("s_cmp_").or(name.strip_prefix("s_cmpk_")) {
            let (a, b) = if name.starts_with("s_cmpk_") {
                let b = if op.ends_with("_i32") {
                    sext16(self)
                } else {
                    self.s.m.i32(u32::from(imm.unwrap_or(0)))
                };
                (src32(self, 0)?, b)
            } else {
                if op.ends_with("64") {
                    return Err("a 64-bit scalar compare");
                }
                (src32(self, 0)?, src32(self, 1)?)
            };
            let signed = op.ends_with("_i32");
            let p = match &op[..2] {
                "eq" => pred::EQ,
                "lg" => pred::NE,
                "gt" if signed => pred::SGT,
                "ge" if signed => pred::SGE,
                "lt" if signed => pred::SLT,
                "le" if signed => pred::SLE,
                "gt" => pred::IUGT,
                "ge" => pred::IUGE,
                "lt" => pred::IULT,
                "le" => pred::IULE,
                _ => return Err("a scalar compare"),
            };
            let r = self.s.m.cmp(c, p, a, b);
            self.set(scc, Val::Bit(r));
            return Ok(());
        }
        let wide = name.ends_with("_b64") || name.ends_with("_u64") || name.ends_with("_i64");
        if wide {
            let hi = dst_code.and_then(|c| inst.write(Reg::S(c + 1)));
            let nonzero = |l: &mut Self, a: V, b: V| {
                let o = l.s.m.bin(c, Bin::Or, a, b);
                let z = l.s.m.i32(0);
                l.s.m.cmp(c, pred::NE, o, z)
            };
            return match name {
                "s_quadmask_b64" => {
                    let (a, b) = (half(self, 0, 0)?, half(self, 0, 1)?);
                    let low = self.quad_mask(a);
                    let high = self.quad_mask(b);
                    let k8 = self.s.m.i32(8);
                    let high = self.s.m.bin(c, Bin::Shl, high, k8);
                    let mask = self.s.m.bin(c, Bin::Or, low, high);
                    let zero = self.s.m.i32(0);
                    self.set(lo, Val::I32(mask));
                    self.set(hi, Val::I32(zero));
                    let nonzero = self.s.m.cmp(c, pred::NE, mask, zero);
                    self.set(scc, Val::Bit(nonzero));
                    Ok(())
                }
                "s_bcnt1_i32_b64" => {
                    let (a, b) = (half(self, 0, 0)?, half(self, 0, 1)?);
                    let i32t = self.s.t.i32;
                    let na = self
                        .s
                        .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 31, &[a]);
                    let nb = self
                        .s
                        .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 31, &[b]);
                    let r = self.s.m.bin(c, Bin::Add, na, nb);
                    self.set(lo, Val::I32(r));
                    let z = self.s.m.i32(0);
                    let s = self.s.m.cmp(c, pred::NE, r, z);
                    self.set(scc, Val::Bit(s));
                    Ok(())
                }
                "s_ff1_i32_b64" => {
                    // The lowest set bit; -1 for none (as DXIL's first bit low).
                    let (a, b) = (half(self, 0, 0)?, half(self, 0, 1)?);
                    let i32t = self.s.t.i32;
                    let fa = self
                        .s
                        .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[a]);
                    let fb = self
                        .s
                        .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[b]);
                    let (z, k32) = (self.s.m.i32(0), self.s.m.i32(32));
                    let fb32 = self.s.m.bin(c, Bin::Add, fb, k32);
                    let none = self.s.m.cmp(c, pred::EQ, b, z);
                    let fb = self.s.m.select(c, none, fb, fb32);
                    let low = self.s.m.cmp(c, pred::NE, a, z);
                    let r = self.s.m.select(c, low, fa, fb);
                    self.set(lo, Val::I32(r));
                    Ok(())
                }
                "s_and_b64" | "s_or_b64" | "s_xor_b64" | "s_andn2_b64" | "s_orn2_b64"
                | "s_nand_b64" | "s_nor_b64" | "s_xnor_b64" | "s_not_b64" => {
                    let ones = self.s.m.i32(u32::MAX);
                    let mut r = Vec::new();
                    for h in 0..2u8 {
                        let a = half(self, 0, h)?;
                        let x = if name == "s_not_b64" {
                            self.s.m.bin(c, Bin::Xor, a, ones)
                        } else {
                            let mut b = half(self, 1, h)?;
                            if name.contains("n2") {
                                b = self.s.m.bin(c, Bin::Xor, b, ones);
                            }
                            let op = match &name[2..5] {
                                "and" | "nan" => Bin::And,
                                "or_" | "orn" | "nor" => Bin::Or,
                                _ => Bin::Xor,
                            };
                            let x = self.s.m.bin(c, op, a, b);
                            if matches!(name, "s_nand_b64" | "s_nor_b64" | "s_xnor_b64") {
                                self.s.m.bin(c, Bin::Xor, x, ones)
                            } else {
                                x
                            }
                        };
                        r.push(x);
                    }
                    self.set(lo, Val::I32(r[0]));
                    self.set(hi, Val::I32(r[1]));
                    let s = nonzero(self, r[0], r[1]);
                    self.set(scc, Val::Bit(s));
                    Ok(())
                }
                "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" => {
                    let (a, b) = (half(self, 0, 0)?, half(self, 0, 1)?);
                    let n = src32(self, 1)?;
                    let i64t = self.s.m.int(64);
                    self.s.flags |= flags::INT64;
                    let a = self.s.m.cast(c, Cast::ZExt, a, i64t);
                    let b = self.s.m.cast(c, Cast::ZExt, b, i64t);
                    let k32 = self.s.m.int_const(i64t, 32);
                    let b = self.s.m.bin(c, Bin::Shl, b, k32);
                    let x = self.s.m.bin(c, Bin::Or, a, b);
                    let k63 = self.s.m.i32(63);
                    let n = self.s.m.bin(c, Bin::And, n, k63);
                    let n = self.s.m.cast(c, Cast::ZExt, n, i64t);
                    let op = match name {
                        "s_lshl_b64" => Bin::Shl,
                        "s_lshr_b64" => Bin::LShr,
                        _ => Bin::AShr,
                    };
                    let r = self.s.m.bin(c, op, x, n);
                    let rl = self.s.m.cast(c, Cast::Trunc, r, self.s.t.i32);
                    let rh = self.s.m.bin(c, Bin::LShr, r, k32);
                    let rh = self.s.m.cast(c, Cast::Trunc, rh, self.s.t.i32);
                    self.set(lo, Val::I32(rl));
                    self.set(hi, Val::I32(rh));
                    let s = nonzero(self, rl, rh);
                    self.set(scc, Val::Bit(s));
                    Ok(())
                }
                "s_mov_b64" => {
                    // Dword by dword: one without a value (the high dword of
                    // a 64-bit wave sum) leaves the other usable.
                    for (h, v) in [(0, lo), (1, hi)] {
                        let x = half(self, 0, h).map_or_else(Val::Missing, Val::I32);
                        self.set(v, x);
                    }
                    Ok(())
                }
                // The code is taken to sit at address 0: what the program
                // computes from the counter is a byte offset in the code,
                // which accesses there read through the code table
                // (`resource::CODE` with that offset as its computed term).
                "s_getpc_b64" => {
                    let next = self.s.m.i32(inst.machine.offset as u32 + 4);
                    let zero = self.s.m.i32(0);
                    self.set(lo, Val::I32(next));
                    self.set(hi, Val::I32(zero));
                    Ok(())
                }
                "s_cselect_b64" => {
                    let cond = self.scc_in(inst)?;
                    for (h, v) in [(0, lo), (1, hi)] {
                        let (a, b) = (half(self, 0, h)?, half(self, 1, h)?);
                        let r = self.s.m.select(c, cond, a, b);
                        self.set(v, Val::I32(r));
                    }
                    Ok(())
                }
                _ => Err(name),
            };
        }
        let m = &mut self.s.m;
        let _ = m;
        let (r, carry): (V, Option<V>) = match name {
            "s_mov_b32" => (src32(self, 0)?, None),
            "s_movk_i32" => (sext16(self), None),
            "s_quadmask_b32" => {
                let pixels = src32(self, 0)?;
                (self.quad_mask(pixels), None)
            }
            "s_cselect_b32" => {
                let cond = self.scc_in(inst)?;
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                (self.s.m.select(c, cond, a, b), None)
            }
            "s_brev_b32" => {
                let a = src32(self, 0)?;
                let r = self
                    .s
                    .op(c, "unary.i32", Attr::ReadNone, self.s.t.i32, 30, &[a]);
                (r, None)
            }
            "s_not_b32" => {
                let a = src32(self, 0)?;
                let ones = self.s.m.i32(u32::MAX);
                (self.s.m.bin(c, Bin::Xor, a, ones), None)
            }
            "s_add_u32" | "s_add_i32" | "s_addk_i32" => {
                let a = src32(self, 0)?;
                let b = if name == "s_addk_i32" {
                    sext16(self)
                } else {
                    src32(self, 1)?
                };
                let r = self.s.m.bin(c, Bin::Add, a, b);
                let carry = if name == "s_add_u32" {
                    Some(self.s.m.cmp(c, pred::IULT, r, a))
                } else {
                    None
                };
                (r, carry)
            }
            "s_sub_u32" | "s_sub_i32" => {
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                let r = self.s.m.bin(c, Bin::Sub, a, b);
                let carry = (name == "s_sub_u32").then(|| self.s.m.cmp(c, pred::IULT, a, b));
                (r, carry)
            }
            "s_addc_u32" | "s_subb_u32" => {
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                let cin = self.scc_in(inst)?;
                let (r, carry) = self.add_sub_carry(c, name == "s_addc_u32", a, b, Some(cin));
                (r, Some(carry))
            }
            "s_mul_i32" | "s_mulk_i32" => {
                let a = src32(self, 0)?;
                let b = if name == "s_mulk_i32" {
                    sext16(self)
                } else {
                    src32(self, 1)?
                };
                (self.s.m.bin(c, Bin::Mul, a, b), None)
            }
            "s_mul_hi_u32" | "s_mul_hi_i32" => {
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                (self.mul_hi(a, b, name.ends_with("i32")), None)
            }
            "s_flbit_i32_b32" | "s_flbit_i32" => {
                // The first bit from the top (differing from the sign for
                // the signed form), -1 for none: DXIL's first-bit-high.
                let a = src32(self, 0)?;
                let op = if name == "s_flbit_i32_b32" { 33 } else { 34 };
                let r = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, op, &[a]);
                (r, None)
            }
            "s_ff1_i32_b32" | "s_bcnt1_i32_b32" => {
                // First set bit (-1 for zero, as DXIL's), or the bit count.
                let a = src32(self, 0)?;
                let op = if name == "s_ff1_i32_b32" { 32 } else { 31 };
                let r = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, op, &[a]);
                (r, None)
            }
            "s_bfe_u32" | "s_bfe_i32" => {
                // src1: offset in bits 0-4, width in bits 16-22.
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                let (k16, k127) = (self.s.m.i32(16), self.s.m.i32(0x7F));
                let w = self.s.m.bin(c, Bin::LShr, b, k16);
                let w = self.s.m.bin(c, Bin::And, w, k127);
                (self.bfe(a, b, w, name == "s_bfe_i32"), None)
            }
            "s_and_b32" | "s_or_b32" | "s_xor_b32" | "s_andn2_b32" | "s_orn2_b32"
            | "s_nand_b32" | "s_nor_b32" | "s_xnor_b32" => {
                let (a, mut b) = (src32(self, 0)?, src32(self, 1)?);
                let ones = self.s.m.i32(u32::MAX);
                if name.contains("n2") {
                    b = self.s.m.bin(c, Bin::Xor, b, ones);
                }
                let op = match &name[2..5] {
                    "and" | "nan" => Bin::And,
                    "or_" | "orn" | "nor" => Bin::Or,
                    _ => Bin::Xor,
                };
                let mut r = self.s.m.bin(c, op, a, b);
                if matches!(name, "s_nand_b32" | "s_nor_b32" | "s_xnor_b32") {
                    r = self.s.m.bin(c, Bin::Xor, r, ones);
                }
                (r, None)
            }
            "s_lshl_b32" | "s_lshr_b32" | "s_ashr_i32" => {
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                let k31 = self.s.m.i32(31);
                let b = self.s.m.bin(c, Bin::And, b, k31);
                let op = match name {
                    "s_lshl_b32" => Bin::Shl,
                    "s_lshr_b32" => Bin::LShr,
                    _ => Bin::AShr,
                };
                (self.s.m.bin(c, op, a, b), None)
            }
            "s_lshl1_add_u32" | "s_lshl2_add_u32" | "s_lshl3_add_u32" | "s_lshl4_add_u32" => {
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                let k = self.s.m.i32(u32::from(name.as_bytes()[6] - b'0'));
                let t = self.s.m.bin(c, Bin::Shl, a, k);
                (self.s.m.bin(c, Bin::Add, t, b), None)
            }
            "s_min_u32" | "s_max_u32" | "s_min_i32" | "s_max_i32" => {
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                let p = match name {
                    "s_min_u32" => pred::IULT,
                    "s_max_u32" => pred::IUGT,
                    "s_min_i32" => pred::SLT,
                    _ => pred::SGT,
                };
                let t = self.s.m.cmp(c, p, a, b);
                // SCC: whether the first operand was chosen (RDNA2 ISA).
                (self.s.m.select(c, t, a, b), Some(t))
            }
            "s_abs_i32" => {
                let a = src32(self, 0)?;
                let z = self.s.m.i32(0);
                let neg = self.s.m.cmp(c, pred::SLT, a, z);
                let n = self.s.m.bin(c, Bin::Sub, z, a);
                (self.s.m.select(c, neg, n, a), None)
            }
            "s_bitset0_b32" | "s_bitset1_b32" => {
                // Sets (clears) bit src[4:0] of the destination it reads.
                let old = self.operand(inst, dst.as_ref().ok_or("a missing operand")?)?;
                let a = src32(self, 0)?;
                let (k31, one) = (self.s.m.i32(31), self.s.m.i32(1));
                let a = self.s.m.bin(c, Bin::And, a, k31);
                let bit = self.s.m.bin(c, Bin::Shl, one, a);
                if name == "s_bitset1_b32" {
                    (self.s.m.bin(c, Bin::Or, old, bit), None)
                } else {
                    let ones = self.s.m.i32(u32::MAX);
                    let nb = self.s.m.bin(c, Bin::Xor, bit, ones);
                    (self.s.m.bin(c, Bin::And, old, nb), None)
                }
            }
            "s_bfm_b32" => {
                let (a, b) = (src32(self, 0)?, src32(self, 1)?);
                let k31 = self.s.m.i32(31);
                let one = self.s.m.i32(1);
                let a = self.s.m.bin(c, Bin::And, a, k31);
                let b = self.s.m.bin(c, Bin::And, b, k31);
                let m = self.s.m.bin(c, Bin::Shl, one, a);
                let m = self.s.m.bin(c, Bin::Sub, m, one);
                (self.s.m.bin(c, Bin::Shl, m, b), None)
            }
            _ => return Err(name),
        };
        self.set(lo, Val::I32(r));
        if let Some(scc) = scc {
            let val = match carry {
                Some(k) => Val::Bit(k),
                None if matches!(
                    name,
                    "s_mov_b32"
                        | "s_movk_i32"
                        | "s_cselect_b32"
                        | "s_brev_b32"
                        | "s_mul_i32"
                        | "s_mulk_i32"
                ) =>
                {
                    return Ok(());
                }
                None if name.starts_with("s_add") || name.starts_with("s_sub") => {
                    Val::Missing("the carry of a scalar operation")
                }
                None => {
                    let z = self.s.m.i32(0);
                    Val::Bit(self.s.m.cmp(c, pred::NE, r, z))
                }
            };
            self.vals.insert(scc, val);
        }
        Ok(())
    }

    /// Packs whether each consecutive group of four pixels has a set bit.
    fn quad_mask(&mut self, pixels: V) -> V {
        let c = self.cur;
        let one = self.s.m.i32(1);
        let shifted = self.s.m.bin(c, Bin::LShr, pixels, one);
        let mut mask = self.s.m.bin(c, Bin::Or, pixels, shifted);
        // Fold each nibble into its bottom bit, then gather those bits into
        // the low byte in three stages without needing a 64-bit operation.
        for (distance, keep) in [
            (2, 0x1111_1111),
            (3, 0x0303_0303),
            (6, 0x000f_000f),
            (12, 0x0000_00ff),
        ] {
            let distance = self.s.m.i32(distance);
            let shifted = self.s.m.bin(c, Bin::LShr, mask, distance);
            let combined = self.s.m.bin(c, Bin::Or, mask, shifted);
            let keep = self.s.m.i32(keep);
            mask = self.s.m.bin(c, Bin::And, combined, keep);
        }
        mask
    }

    /// `a + b (+ cin)` or `a - b (- cin)` with the carry (borrow) out, as the
    /// scalar and vector add-with-carry instructions set it.
    fn add_sub_carry(&mut self, c: usize, add: bool, a: V, b: V, cin: Option<V>) -> (V, V) {
        let op = if add { Bin::Add } else { Bin::Sub };
        let mut r = self.s.m.bin(c, op, a, b);
        // Without a carry in: a + b wraps below a; a - b borrows when a < b.
        let mut carry = if add {
            self.s.m.cmp(c, pred::IULT, r, a)
        } else {
            self.s.m.cmp(c, pred::IULT, a, b)
        };
        if let Some(cin) = cin {
            let one = self.s.m.cast(c, Cast::ZExt, cin, self.s.t.i32);
            let t = r;
            r = self.s.m.bin(c, op, t, one);
            // The carry in carries on when the partial result is all ones
            // (add) or zero (sub).
            let edge = self.s.m.i32(if add { u32::MAX } else { 0 });
            let at_edge = self.s.m.cmp(c, pred::EQ, t, edge);
            let more = self.s.m.bin(c, Bin::And, cin, at_edge);
            carry = self.s.m.bin(c, Bin::Or, carry, more);
        }
        (r, carry)
    }

    fn scc_in(&mut self, inst: &Inst) -> Result<V, Why> {
        let v = inst.read(Reg::Scc).ok_or("an unread SCC")?;
        match self.get(v)? {
            Val::Bit(x) => Ok(x),
            _ => Err("SCC without a value"),
        }
    }

    fn smem(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        let Fields::Smem {
            data,
            offset,
            soffset,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let name = inst.name();
        if let Some(a) = self.scalar.get(&site).copied().filter(|a| a.param == CODE) {
            return self.code_smem(inst, a);
        }
        if let Some(a) = self
            .scalar
            .get(&site)
            .copied()
            .filter(|a| a.param == USER_DATA)
        {
            return self.user_data_smem(inst, a);
        }
        if let Some(a) = self
            .scalar
            .get(&site)
            .copied()
            .filter(|a| a.param == MEMORY)
        {
            return self.memory_smem(inst, a);
        }
        let Some(rest) = name.strip_prefix("s_buffer_load_dword") else {
            return self.descriptor_data(inst, site);
        };
        let count = match rest {
            "" => 1,
            "x2" => 2,
            "x4" => 4,
            "x8" => 8,
            "x16" => 16,
            _ => return Err("a scalar buffer load"),
        };
        let Some(Operand::Scalar { code, .. }) = *data else {
            return Err("a scalar buffer load");
        };
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked descriptor")?;
        let kind = self.used_kind(src);
        let c = self.cur;
        let (base, extra) = self.built_offset(src)?;
        let offset = &((*offset as u32).wrapping_add(base) as i32);
        let mut addr = self.s.m.i32(*offset as u32);
        if let Some(x) = extra {
            addr = self.s.m.bin(c, Bin::Add, addr, x);
        }
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            let so = self.operand(inst, soffset)?;
            addr = self.s.m.bin(c, Bin::Add, addr, so);
        }
        if matches!(src, Source::Built { address, .. } if address.param == CODE) {
            // A descriptor built around a table in the code.
            for k in 0..count {
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, addr, off);
                let x = self.code_dword(a)?;
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        // Heap loads must use the installed kind; null/texture slots create no handle.
        let picked = match kind {
            None => self.slot_info(src)?,
            Some(_) => None,
        };
        if let Some(info) = picked {
            let Source::Loaded { load, first } = src else {
                unreachable!("slot_info requires a loaded descriptor")
            };
            let index = self.heap_index(*load, *first, false)?;
            let per_lane = self.x.res.waterfall.loads.contains(load);
            self.heap = [true, true];
            let zero = self.s.m.i32(0);
            let stride = self.buffer_stride(info);
            let is_buffer = self.s.m.cmp(self.cur, pred::NE, stride, zero);
            let flag = self.s.m.i32(super::shader::SLOT_STRIDE_UAV);
            let uav = self.s.m.bin(self.cur, Bin::And, info, flag);
            let mut raw = vec![zero; count as usize];
            for (kind, props) in [(zero, 11), (flag, 0x100b)] {
                let matches = self.s.m.cmp(self.cur, pred::EQ, uav, kind);
                let guard = self.s.m.bin(self.cur, Bin::And, is_buffer, matches);
                let at = self.begin_guarded(Some(guard));
                // Keep handles inside their kind branch; the generic path can hoist them.
                let h = self
                    .s
                    .heap_handle(self.cur, index, false, per_lane, (props, 0));
                let mut values = Vec::new();
                if count > 1 && Self::wide_loads() {
                    values.extend(
                        self.raw_dwords(h, addr, count)
                            .into_iter()
                            .zip(raw.iter().copied()),
                    );
                } else {
                    for (k, &previous) in raw.iter().enumerate() {
                        let b = self.cur;
                        let off = self.s.m.i32(4 * k as u32);
                        let a = self.s.m.bin(b, Bin::Add, addr, off);
                        let mask = self.s.m.i8(1);
                        let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
                        let r = self.s.op(
                            b,
                            "rawBufferLoad.f32",
                            Attr::ReadOnly,
                            self.s.t.res_ret,
                            139,
                            &[h, a, u, mask, al],
                        );
                        let x = self.s.m.extract(b, r, 0);
                        let x = self.as_int(x);
                        values.push((x, previous));
                    }
                }
                raw = self.end_guarded(at, &values);
            }
            let cbv = self.s.m.i32(super::shader::SLOT_STRIDE_CBV);
            let is_cbv = self.s.m.cmp(self.cur, pred::EQ, info, cbv);
            let at = self.begin_guarded(Some(is_cbv));
            let h = self
                .s
                .heap_handle(self.cur, index, false, per_lane, (13, 0x1_0000));
            let mut both = Vec::new();
            for (k, &r) in raw.iter().enumerate() {
                let b = self.cur;
                let off = self.s.m.i32(4 * k as u32);
                let a = self.s.m.bin(b, Bin::Add, addr, off);
                let x = self.cbuffer_dword(h, a);
                both.push((x, r));
            }
            let both = self.end_guarded(at, &both);
            for (k, x) in both.into_iter().enumerate() {
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        let heap_constants = kind.is_none() && self.is_heap_direct(src);
        // So is constant data in a table: the title's slot holds a raw view
        // of it, so tables need no copies of their own per draw.
        let table_constants =
            matches!(kind, Some(u) if u.kind == ResourceType::Cbv && u.root == RootType::Table);
        let srv = heap_constants
            || table_constants
            || matches!(kind, Some(u) if u.kind == ResourceType::Srv);
        // A root UAV is read as title memory in a program that uses it.
        let root_uav = self.root_memory
            && matches!(kind, Some(u) if u.kind == ResourceType::Uav)
            && matches!(src, Source::Built { address, .. }
                if self.x.native.layout.get(address.param).is_some_and(|p| p.root == RootType::Descriptor));
        if srv || root_uav {
            let (h, addr) = self.raw_view(site, src, addr, 4 * count, (11, 0), false, None)?;
            if count > 1 && Self::wide_loads() {
                for (k, x) in self.raw_dwords(h, addr, count).into_iter().enumerate() {
                    self.write(inst, Reg::S(code + k as u8), x, None);
                }
                return Ok(());
            }
            let c = self.cur;
            for k in 0..count {
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, addr, off);
                let mask = self.s.m.i8(1);
                let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
                let r = self.s.op(
                    c,
                    "rawBufferLoad.f32",
                    Attr::ReadOnly,
                    self.s.t.res_ret,
                    139,
                    &[h, a, u, mask, al],
                );
                let x = self.s.m.extract(c, r, 0);
                let x = self.as_int(x);
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        let h = self.handle(site, src, false, (13, 0x1_0000))?;
        let constant = match addr {
            V::Const(_)
                if matches!(soffset, Operand::Scalar { code: NULL, .. }) && extra.is_none() =>
            {
                Some(*offset as u32)
            }
            _ => None,
        };
        let mut rows: HashMap<u32, V> = HashMap::new();
        for k in 0..count {
            let x = match constant {
                Some(base) => {
                    let byte = base + 4 * k;
                    let row = match rows.get(&(byte >> 4)) {
                        Some(&r) => r,
                        None => {
                            let ri = self.s.m.i32(byte >> 4);
                            let r = self.s.op(
                                c,
                                "cbufferLoadLegacy.i32",
                                Attr::ReadOnly,
                                self.s.t.cbuf_ret,
                                59,
                                &[h, ri],
                            );
                            rows.insert(byte >> 4, r);
                            r
                        }
                    };
                    self.s.m.extract(c, row, (byte >> 2) & 3)
                }
                None => {
                    let off = self.s.m.i32(4 * k);
                    let a = self.s.m.bin(c, Bin::Add, addr, off);
                    self.cbuffer_dword(h, a)
                }
            };
            self.write(inst, Reg::S(code + k as u8), x, None);
        }
        Ok(())
    }

    /// A descriptor load (`s_load_dword*` from a table or heap) whose dwords
    /// the code also reads as numbers: the console compiler computes sizes
    /// from descriptor bits (an image's width, height, depth, levels; a
    /// buffer's record count) instead of querying them. Those fields are
    /// rebuilt from the host view's dimensions in the GFX10 layout (image
    /// dword 1 bits 30-31 and dword 2 bits 0-13 width - 1, dword 2 bits
    /// 14-29 height - 1, dword 3 identity swizzle, last level at 16, type at
    /// 28, dword 4 depth or layers - 1; buffer dword 2 the record count).
    // shortcut: the base level reads 0 (the host view starts at its own
    // level 0, so sizes the code shifts by it come out right) and the other
    // dwords (addresses, formats, tiling) have no host value: reading them
    // stops the program.
    fn descriptor_data(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        const OTHER: Why = "a descriptor's address or format read as data";
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar memory read as data");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u32,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar memory read as data"),
        };
        // How the program uses the descriptor tells the view's kind; a load
        // read only as data takes it from a load of the same descriptor that
        // the program uses (the compiler loads a descriptor again rather than
        // keep it, and queries sizes in loops of their own).
        let use_of = |site: Site| {
            self.x
                .res
                .descriptors
                .iter()
                .filter(|d| d.role == Role::Resource)
                .find_map(|d| {
                    d.source.leaves().into_iter().find_map(|l| match *l {
                        Source::Loaded { load, first } if load == site => Some((d.site, first)),
                        _ => None,
                    })
                })
        };
        // The VGPR a waterfall loop reads the first lane of.
        let lane_source = |v: Value| {
            let i = self.def_inst(v)?;
            match i.fields {
                Fields::Valu(ref f) if i.name() == "v_readfirstlane_b32" => match f.src[..] {
                    [Operand::Vector { reg, .. }] => i.read(Reg::V(reg)),
                    _ => None,
                },
                _ => None,
            }
        };
        let same = |a: &Address, b: &Address| {
            a.param == b.param
                && a.offset == b.offset
                && (a.dynamic == b.dynamic
                    || a.dynamic.and_then(lane_source).is_some()
                        && a.dynamic.and_then(lane_source) == b.dynamic.and_then(lane_source))
        };
        let address = self.scalar.get(&site);
        let use_at = use_of(site).or_else(|| {
            self.x
                .res
                .scalar
                .iter()
                .filter(|&(s, a)| *s != site && address.is_some_and(|b| same(a, b)))
                .find_map(|&(s, _)| use_of(s))
        });
        let Some((at, first)) = use_at else {
            return self.view_addresses(inst, site, code, count);
        };
        let src = Source::Loaded { load: site, first };
        let used = self.used_kind(&src);
        let ui: &Inst = &self.x.p.blocks[at.0].insts[at.1];
        let un = ui.name();
        let uav = un.contains("store")
            || un.contains("atomic")
            || used.is_some_and(|u| u.kind == ResourceType::Uav);
        let u = if uav { 0x1000 } else { 0 };
        let (props, image) = match &ui.fields {
            Fields::Image { dim, .. } => {
                let kind = match dim {
                    0 => 1,
                    1 => 2,
                    2 => 4,
                    4 => 6,
                    5 => 7,
                    _ => return Err("an image dimension (cube or MSAA)"),
                };
                ((kind | u, 0x409), Some(kind))
            }
            Fields::Buffer { idxen, format, .. } => {
                let stride = used.map_or(0, |u| u.stride);
                let props = if un.starts_with("buffer_load_format")
                    || un.starts_with("buffer_store_format")
                {
                    // (Unsigned integers where the access moves halves.)
                    let halves = self
                        .table_place(&Source::Loaded { load: site, first })
                        .is_some_and(|p| {
                            self.halves_only.contains(&p)
                                && (un.contains("_format_d16_")
                                    || self.format_load_read_as_halves(ui))
                        });
                    (10 | u, if halves { 0x405 } else { 0x409 })
                } else if *idxen && format.is_none() && stride >= 4 && stride.is_multiple_of(4) {
                    (12 | u, stride)
                } else {
                    (11 | u, 0)
                };
                if used.is_some_and(|u| u.kind == ResourceType::Cbv) {
                    return Err("a constant buffer's descriptor read as data");
                }
                (props, None)
            }
            // A buffer the code reads with scalar loads (`buf[buf.Length - 1]`
            // at a uniform index): structured by the used-resource entry's
            // stride, so its record count is in elements, else raw (bytes).
            Fields::Smem { .. } => {
                let used = used.ok_or("a scalar-loaded buffer of unknown kind read as data")?;
                if used.kind == ResourceType::Cbv {
                    return Err("a constant buffer's descriptor read as data");
                }
                let props = match used.stride {
                    0 | 1 => (11 | u, 0),
                    s if s.is_multiple_of(4) => (12 | u, s),
                    _ => return Err("a scalar-loaded buffer of odd stride read as data"),
                };
                (props, None)
            }
            _ => return Err("a scalar memory read as data"),
        };
        let h = self.handle(site, &src, false, props)?;
        let c = self.cur;
        let i32t = self.s.t.i32;
        let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
        let mip = if image.is_some() && !uav {
            self.s.m.i32(0)
        } else {
            self.s.m.undef(i32t)
        };
        let d = self
            .s
            .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, mip]);
        let field = |l: &mut Self, k: u32| l.s.m.extract(c, d, k);
        let words: Vec<(u32, V)> = match image {
            Some(kind) => {
                let one = self.s.m.i32(1);
                let w = field(self, 0);
                let (height, depth) = match kind {
                    1 => (one, one),
                    2 => (field(self, 1), one),
                    6 => (one, field(self, 1)),
                    _ => (field(self, 1), field(self, 2)),
                };
                let levels = if uav { one } else { field(self, 3) };
                let less = |l: &mut Self, x: V| l.s.m.bin(c, Bin::Sub, x, one);
                let (w, height, depth, levels) = (
                    less(self, w),
                    less(self, height),
                    less(self, depth),
                    less(self, levels),
                );
                let k = |l: &mut Self, v: u32| l.s.m.i32(v);
                let (k30, k2, k14, k16, mask) = (
                    k(self, 30),
                    k(self, 2),
                    k(self, 14),
                    k(self, 16),
                    k(self, 0x3FFF),
                );
                let d1 = self.s.m.bin(c, Bin::Shl, w, k30);
                let hi = self.s.m.bin(c, Bin::LShr, w, k2);
                let hi = self.s.m.bin(c, Bin::And, hi, mask);
                let ht = self.s.m.bin(c, Bin::Shl, height, k14);
                let d2 = self.s.m.bin(c, Bin::Or, hi, ht);
                let ty = [0, 8, 9, 0, 10, 0, 12, 13][kind as usize];
                let base = k(self, 0xFAC | ty << 28);
                let last = self.s.m.bin(c, Bin::Shl, levels, k16);
                let d3 = self.s.m.bin(c, Bin::Or, base, last);
                vec![(1, d1), (2, d2), (3, d3), (4, depth)]
            }
            None => vec![(2, field(self, 0))],
        };
        for k in 0..count {
            let rel = k.wrapping_sub(u32::from(first));
            match words.iter().find(|w| w.0 == rel) {
                Some(&(_, x)) => self.write(inst, Reg::S(code + k as u8), x, None),
                None => {
                    if let Some(v) = inst.write(Reg::S(code + k as u8)) {
                        self.vals.insert(v, Val::Missing(OTHER));
                    }
                }
            }
        }
        Ok(())
    }

    /// `s_load_dword*` from the root signature's memory: user-data dwords.
    fn user_data_smem(&mut self, inst: &Inst, a: Address) -> Result<(), Why> {
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar load");
        };
        let count = smem_dwords(inst.name()).ok_or("a scalar load from the root signature")?;
        for k in 0..count {
            let index = a.offset / 4 + k;
            let held = self.x.native.layout.iter().enumerate().find_map(|(i, p)| {
                let dword = index.checked_sub(p.user_data.into())?;
                (dword < p.sgprs).then_some((i, dword))
            });
            let val = match held {
                Some((param, dword)) => self.user(param, dword),
                None => Val::Missing("a root-signature dword no parameter holds"),
            };
            if let Some(v) = inst.write(Reg::S(code + k as u8)) {
                self.vals.insert(v, val);
            }
        }
        Ok(())
    }

    /// Split wide transfers at independently mapped 64 KiB pages. Each lookup
    /// marks only its own page and may change the current DXIL block.
    fn memory_transfer(
        &mut self,
        lo: V,
        hi: V,
        active: Option<V>,
        count: u8,
        write: bool,
        mut transfer: impl FnMut(&mut Self, V, V, std::ops::Range<u8>) -> Vec<V>,
    ) -> Vec<V> {
        if count == 1 {
            let (h, byte) = self.memory(lo, hi, 4, write, active);
            return transfer(self, h, byte, 0..count);
        }
        let head = self.cur;
        let page_mask = self.s.m.i32(0xFFFF);
        let within = self.s.m.bin(head, Bin::And, lo, page_mask);
        let limit = self.s.m.i32(0x10000 - 4 * u32::from(count));
        let fits = self.s.m.cmp(head, pred::IULE, within, limit);
        let (whole, split, join) = (
            self.s.m.new_block(),
            self.s.m.new_block(),
            self.s.m.new_block(),
        );
        self.s.m.terminate(head, Term::CondBr(fits, whole, split));
        self.cur = whole;
        let (h, byte) = self.memory(lo, hi, 4 * u32::from(count), write, active);
        let values = transfer(self, h, byte, 0..count);
        let tail = self.cur;
        self.s.m.terminate(tail, Term::Br(join));
        let mut incoming: Vec<_> = values.into_iter().map(|v| vec![(v, tail)]).collect();
        self.cur = split;
        let last = self.s.m.i32(4 * u32::from(count - 1));
        let (end_lo, end_hi) = self.add64(lo, hi, last, false);
        let page_base = self.s.m.i32(!0xFFFF);
        let next_lo = self.s.m.bin(self.cur, Bin::And, end_lo, page_base);
        for first in (1..count).rev() {
            let rest = if first > 1 {
                let (case, rest) = (self.s.m.new_block(), self.s.m.new_block());
                let boundary = self.s.m.i32(0x10000 - 4 * u32::from(first));
                let fits = self.s.m.cmp(self.cur, pred::IULE, within, boundary);
                self.s.m.terminate(self.cur, Term::CondBr(fits, case, rest));
                self.cur = case;
                rest
            } else {
                join
            };
            let (h, byte) = self.memory(lo, hi, 4 * u32::from(first), write, active);
            let mut values = transfer(self, h, byte, 0..first);
            let (h, byte) =
                self.memory(next_lo, end_hi, 4 * u32::from(count - first), write, active);
            values.extend(transfer(self, h, byte, first..count));
            for (edges, value) in incoming.iter_mut().zip(values) {
                edges.push((value, self.cur));
            }
            self.s.m.terminate(self.cur, Term::Br(join));
            self.cur = rest;
        }
        incoming
            .into_iter()
            .map(|edges| {
                let phi = self.s.m.phi(join, self.s.t.i32);
                self.s.m.set_phi(phi, edges);
                phi
            })
            .collect()
    }

    fn memory_load_words(&mut self, handle: V, byte: V, count: u8) -> Vec<V> {
        let c = self.cur;
        let iu = self.s.m.undef(self.s.t.i32);
        let (mask, align) = (self.s.m.i8((1u8 << count) - 1), self.s.m.i32(4));
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[handle, byte, iu, mask, align],
        );
        (0..count)
            .map(|k| {
                let x = self.s.m.extract(c, r, u32::from(k));
                self.as_int(x)
            })
            .collect()
    }

    fn memory_store_words(&mut self, handle: V, byte: V, words: &[V]) -> Vec<V> {
        let iu = self.s.m.undef(self.s.t.i32);
        let mut args = [iu; 9];
        args[0] = handle;
        args[1] = byte;
        args[3..3 + words.len()].copy_from_slice(words);
        args[7] = self.s.m.i8((1u8 << words.len()) - 1);
        args[8] = self.s.m.i32(4);
        self.s.op(
            self.cur,
            "rawBufferStore.i32",
            Attr::NoUnwind,
            self.s.t.void,
            140,
            &args,
        );
        Vec::new()
    }

    /// `s_load_dword*` from title memory at an address the program computes.
    fn memory_smem(&mut self, inst: &Inst, _a: Address) -> Result<(), Why> {
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            base: Operand::Scalar { code: base, .. },
            offset,
            soffset,
            ..
        } = inst.fields
        else {
            return Err("a scalar load");
        };
        let count = smem_dwords(inst.name()).ok_or("a scalar load from title memory")?;
        let word = |l: &mut Self, k: u8| -> Result<V, Why> {
            let v = inst
                .read(Reg::S(base + k))
                .ok_or("a scalar memory address")?;
            l.i32_of(v)
        };
        let (lo, hi) = (word(self, 0)?, word(self, 1)?);
        // The base's high dword holds address bits 32-47.
        let mask = self.s.m.i32(0xFFFF);
        let hi = self.s.m.bin(self.cur, Bin::And, hi, mask);
        // The immediate is signed (21 bits), the offset register unsigned.
        let mut extra = vec![(self.s.m.i32(offset as u32), offset < 0)];
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            extra.push((self.operand(inst, &soffset)?, false));
        }
        let (mut lo, mut hi) = (lo, hi);
        for (x, negative) in extra {
            (lo, hi) = self.add64(lo, hi, x, negative);
        }
        for first in (0..count).step_by(4) {
            let n = (count - first).min(4) as u8;
            let offset = self.s.m.i32(4 * first);
            let (lo, hi) = self.add64(lo, hi, offset, false);
            let words = self.memory_transfer(lo, hi, None, n, false, |l, h, byte, range| {
                l.memory_load_words(h, byte, range.end - range.start)
            });
            for (k, word) in (0..n).zip(words) {
                self.write(inst, Reg::S(code + first as u8 + k), word, None);
            }
        }
        Ok(())
    }

    /// `(hi:lo) + x` with `x` zero-extended, or sign-extended when
    /// `negative` says its top bit means it.
    fn add64(&mut self, lo: V, hi: V, x: V, negative: bool) -> (V, V) {
        let c = self.cur;
        let sum = self.s.m.bin(c, Bin::Add, lo, x);
        let carry = self.s.m.cmp(c, pred::IULT, sum, lo);
        let carry = self.s.m.cast(c, Cast::ZExt, carry, self.s.t.i32);
        let hi = self.s.m.bin(c, Bin::Add, hi, carry);
        let hi = if negative {
            let all = self.s.m.i32(u32::MAX);
            self.s.m.bin(c, Bin::Add, hi, all)
        } else {
            hi
        };
        (sum, hi)
    }

    /// The dword at byte `at` of raw buffer `h`.
    fn raw_dword(&mut self, h: V, at: V) -> V {
        let c = self.cur;
        let (u, mask, al) = (
            self.s.m.undef(self.s.t.i32),
            self.s.m.i8(1),
            self.s.m.i32(4),
        );
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[h, at, u, mask, al],
        );
        let x = self.s.m.extract(c, r, 0);
        self.as_int(x)
    }

    /// Marks the pages a write of `bytes` bytes at byte `x` of the unordered
    /// buffer view `h` touches, in the lanes `guard` lets through (module
    /// notes, "Write marks"): the bits of its first and last byte, each set
    /// only where still clear. Nothing for handles that are no heap view, or
    /// without [`Parts::view_marks`].
    fn mark_view_write(&mut self, h: V, x: V, bytes: u32, guard: Option<V>) {
        if !self.x.view_marks {
            return;
        }
        let Some(&index) = self.s.view_index.get(&h) else {
            return;
        };
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let table = self.s.view_marks_handle();
        let c = self.cur;
        let (eight, four) = (k(self, 8), k(self, 4));
        let at = self.s.m.bin(c, Bin::Mul, index, eight);
        let base = self.raw_dword(table, at);
        let at = self.s.m.bin(c, Bin::Add, at, four);
        let phase = self.raw_dword(table, at);
        let none = k(self, super::VIEW_MARK_NONE);
        let valid = self.s.m.cmp(c, pred::NE, base, none);
        let active = match guard {
            Some(g) => self.s.m.bin(c, Bin::And, g, valid),
            None => valid,
        };
        let first = self.s.m.bin(c, Bin::Add, phase, x);
        let span = k(self, bytes.max(1) - 1);
        let last = self.s.m.bin(c, Bin::Add, first, span);
        let (k16, cap) = (k(self, 16), k(self, super::VIEW_WRITE_BITS - 1));
        let mut bits = Vec::new();
        for byte in [first, last] {
            let page = self.s.m.bin(c, Bin::LShr, byte, k16);
            let n = self.s.m.bin(c, Bin::Add, base, page);
            let over = self.s.m.cmp(c, pred::IUGT, n, cap);
            bits.push(self.s.m.select(c, over, cap, n));
        }
        let bitmap = self.s.view_writes_handle();
        let outer = self.begin_guarded(Some(active));
        for (i, &n) in bits.iter().enumerate() {
            let c = self.cur;
            let (k5, k2, k31, one, zero) =
                (k(self, 5), k(self, 2), k(self, 31), k(self, 1), k(self, 0));
            let word = self.s.m.bin(c, Bin::LShr, n, k5);
            let word = self.s.m.bin(c, Bin::Shl, word, k2);
            let bit = self.s.m.bin(c, Bin::And, n, k31);
            let bit = self.s.m.bin(c, Bin::Shl, one, bit);
            // Most writes find their page marked: a load, not an atomic.
            let old = self.raw_dword(bitmap, word);
            let set = self.s.m.bin(c, Bin::And, old, bit);
            let mut clear = self.s.m.cmp(c, pred::EQ, set, zero);
            if i == 1 {
                let other = self.s.m.cmp(c, pred::NE, n, bits[0]);
                clear = self.s.m.bin(c, Bin::And, clear, other);
            }
            let inner = self.begin_guarded(Some(clear));
            let iu = self.s.m.undef(self.s.t.i32);
            // Operation 2 is OR.
            self.s.op(
                self.cur,
                "atomicBinOp.i32",
                Attr::NoUnwind,
                self.s.t.i32,
                78,
                &[bitmap, k2, word, iu, iu, bit],
            );
            self.end_guarded(inner, &[]);
        }
        self.end_guarded(outer, &[]);
    }

    /// The byte stride of the buffer view at heap index `index`, from the
    /// view-stride table (`t`[`VIEW_STRIDES`]).
    fn stride_at(&mut self, index: V) -> V {
        let word = self.view_word(index);
        let bits = self.s.m.i32(VIEW_STRIDE_BITS);
        self.s.m.bin(self.cur, Bin::And, word, bits)
    }

    /// Dword `index` of the view-stride table: the stride and the format
    /// kind bits ([`VIEW_INTEGER`], [`VIEW_SIGNED`]).
    fn view_word(&mut self, index: V) -> V {
        let c = self.cur;
        let four = self.s.m.i32(4);
        let at = self.s.m.bin(c, Bin::Mul, index, four);
        let table = self.s.root_srv_handle(VIEW_STRIDES);
        self.raw_dword(table, at)
    }

    /// Whether the typed buffer view a handle names has an integer format,
    /// and whether a signed one, from the view-stride table; `None` if the
    /// handle's heap index is not known.
    fn integer_view(&mut self, h: V) -> Option<(V, V)> {
        let index = *self.s.view_index.get(&h)?;
        let word = self.view_word(index);
        let c = self.cur;
        let zero = self.s.m.i32(0);
        let [int, signed] = [VIEW_INTEGER, VIEW_SIGNED].map(|bit| {
            let bit = self.s.m.i32(bit);
            let x = self.s.m.bin(c, Bin::And, word, bit);
            self.s.m.cmp(c, pred::NE, x, zero)
        });
        Some((int, signed))
    }

    /// Title memory at a raw GPU address (decision 0005: the unified-memory
    /// item). Every access to an address the program computes goes through
    /// here: scalar and global loads, stores and atomics, the BVH node
    /// fetch, and in a program that uses title memory its root SRVs and UAVs
    /// ([`Self::raw_view`]). `(hi:lo)` is the 64-bit address of the access's
    /// first byte (`hi` holds bits 32-47), `bytes` its size, `write` whether
    /// it stores or does atomics, `active` the lanes that access (`None`:
    /// all; the others look up but mark nothing). Returns a raw buffer handle
    /// (read-write with `write`) and the address's byte offset in it; the
    /// access's dwords follow at `+ 4k`. Memory no host copy holds is the
    /// null view: reads give 0, writes are dropped. Contract (`emit` "Title
    /// memory"): a page table in the root SRV `t0` of
    /// [`super::shader::MEMORY_SPACE`]; the root UAV `u0` there takes a bit
    /// per page written and per page or 256 MiB region missed. May end in a
    /// new block: callers take [`Lower::cur`] afresh.
    // shortcut: one lookup per access, so its bytes are taken to lie in one
    // host copy of title memory (any access within a 64 KiB page does).
    pub(crate) fn memory(
        &mut self,
        lo: V,
        hi: V,
        bytes: u32,
        write: bool,
        active: Option<V>,
    ) -> (V, V) {
        let e = self.memory_entry(lo, hi, write);
        let c = self.cur;
        // Writes always mark (written or missed pages), reads only misses.
        let marks = match (write, active) {
            (true, Some(a)) => Some(a),
            (true, None) => None,
            (false, Some(a)) => Some(self.s.m.bin(c, Bin::And, e.miss, a)),
            (false, None) => Some(e.miss),
        };
        let k = |l: &mut Self, x: u32| l.s.m.i32(x);
        // A miss marks its page's entry, or with no block its region's
        // level-1 index (bits 0-4095, block 0's entries, which no copy has).
        let zero = k(self, 0);
        let empty = self.s.m.cmp(c, pred::EQ, e.block, zero);
        let missed = self.s.m.select(c, empty, e.l1, e.entry);
        let first = self.s.m.select(c, e.miss, missed, e.entry);
        let mut bits = vec![first];
        if write {
            // A write marks the pages of its first and last byte (one block).
            let (last, k16) = (k(self, bytes - 1), k(self, 16));
            let end = self.s.m.bin(c, Bin::Add, lo, last);
            let a = self.s.m.bin(c, Bin::LShr, lo, k16);
            let b = self.s.m.bin(c, Bin::LShr, end, k16);
            let pages = self.s.m.bin(c, Bin::Sub, b, a);
            let next = self.s.m.bin(c, Bin::Add, e.entry, pages);
            bits.push(self.s.m.select(c, e.miss, first, next));
        }
        let bitmap = self.s.memory_dirty_handle();
        let at = self.begin_guarded(marks);
        let c = self.cur;
        let (k5, k31, k2, one) = (k(self, 5), k(self, 31), k(self, 2), k(self, 1));
        let iu = self.s.m.undef(self.s.t.i32);
        for n in bits {
            // Byte offset of the bitmap dword; operation 2 is OR.
            let word = self.s.m.bin(c, Bin::LShr, n, k5);
            let word = self.s.m.bin(c, Bin::Shl, word, k2);
            let bit = self.s.m.bin(c, Bin::And, n, k31);
            let bit = self.s.m.bin(c, Bin::Shl, one, bit);
            self.s.op(
                c,
                "atomicBinOp.i32",
                Attr::NoUnwind,
                self.s.t.i32,
                78,
                &[bitmap, k2, word, iu, iu, bit],
            );
        }
        self.end_guarded(at, &[]);
        (e.handle, e.offset)
    }

    /// [`Self::memory`]'s lookup in the page table.
    fn memory_entry(&mut self, lo: V, hi: V, write: bool) -> MemoryEntry {
        let c = self.cur;
        let map = self.s.memory_handle();
        let k = |l: &mut Self, x: u32| l.s.m.i32(x);
        // Level 1: (hi:lo) >> 28, 4096 entries for 40-bit addresses.
        let (k4, k28, k256) = (k(self, 4), k(self, 28), k(self, 256));
        let top = self.s.m.bin(c, Bin::Shl, hi, k4);
        let low = self.s.m.bin(c, Bin::LShr, lo, k28);
        let l1 = self.s.m.bin(c, Bin::Or, top, low);
        let inside = self.s.m.cmp(c, pred::IULT, hi, k256);
        let zero = k(self, 0);
        let l1 = self.s.m.select(c, inside, l1, zero);
        let two = k(self, 2);
        let at = self.s.m.bin(c, Bin::Shl, l1, two);
        let block = self.raw_dword(map, at);
        let block = self.s.m.select(c, inside, block, zero);
        // Level 2: block b holds 4096 entries of two dwords after level 1.
        let (k16, k12, mask, k3, first) = (
            k(self, 16),
            k(self, 12),
            k(self, 0xFFF),
            k(self, 3),
            k(self, 4096 * 4),
        );
        let page = self.s.m.bin(c, Bin::LShr, lo, k16);
        let page = self.s.m.bin(c, Bin::And, page, mask);
        let entry = self.s.m.bin(c, Bin::Shl, block, k12);
        let entry = self.s.m.bin(c, Bin::Or, entry, page);
        let at = self.s.m.bin(c, Bin::Shl, entry, k3);
        let at = self.s.m.bin(c, Bin::Add, at, first);
        let view = self.raw_dword(map, at);
        let four = k(self, 4);
        let at = self.s.m.bin(c, Bin::Add, at, four);
        let start = self.raw_dword(map, at);
        // Block 0's first entry names the null pair: no copy holds the page.
        let null = self.raw_dword(map, first);
        let miss = self.s.m.cmp(c, pred::EQ, view, null);
        // Views come in pairs: the raw SRV, then the raw UAV of one copy.
        let (view, props) = if write {
            let one = k(self, 1);
            (self.s.m.bin(c, Bin::Add, view, one), (11 | 0x1000, 0))
        } else {
            (view, (11, 0))
        };
        let handle = self.s.heap_handle(c, view, false, true, props);
        // Pages written here are marked in the page table's bitmap.
        self.s.view_index.remove(&handle);
        MemoryEntry {
            handle,
            offset: self.s.m.bin(c, Bin::Sub, lo, start),
            entry,
            block,
            l1,
            miss,
        }
    }

    /// A descriptor load from a table or heap whose dwords the program only
    /// reads as numbers: the base address of a buffer view (dwords 0 and 1,
    /// as a pointer to title memory: the BVH's address in its SRV) or, for
    /// a whole image descriptor (eight dwords), also the size fields of
    /// dwords 1-3 (width, height, first and last level, type: size queries
    /// of a texture the program never samples). They come from the host's
    /// view addresses, `t1` in [`super::shader::MEMORY_SPACE`]: four dwords
    /// per heap index.
    // shortcut: dword 1 holds the address bits only (stride and swizzle read
    // 0) or a texture's width bits, dword 3 a texture's size fields (format,
    // tiling and swizzle choice read 0 or the identity); dword 4 on and a
    // buffer's dwords 2-3 (record count, format) stop the program when read;
    // the table offset's computed part is taken as whole descriptors
    // (32-byte multiples).
    fn view_addresses(&mut self, inst: &Inst, site: Site, code: u8, count: u32) -> Result<(), Why> {
        const OTHER: Why = "a descriptor read only as data";
        let a = *self
            .scalar
            .get(&site)
            .ok_or("a descriptor read only as data")?;
        if !matches!(
            self.x.native.layout.get(a.param).map(|p| p.root),
            Some(RootType::Table | RootType::ResourceHeap)
        ) {
            return Err("a descriptor read only as data");
        }
        // The compiler loads image descriptors whole, buffer ones as four
        // dwords.
        let known = if count == 8 && a.offset.is_multiple_of(32) {
            4
        } else {
            2
        };
        let views = self.s.view_addresses_handle();
        for k in 0..count {
            let word = (a.offset / 4 + k) % 8;
            let val = if word < known {
                let index = self.heap_index(site, k as u8, false)?;
                let c = self.cur;
                let four = self.s.m.i32(4);
                let at = self.s.m.bin(c, Bin::Shl, index, four);
                let w = self.s.m.i32(4 * word);
                let at = self.s.m.bin(c, Bin::Add, at, w);
                Val::I32(self.raw_dword(views, at))
            } else {
                Val::Missing(OTHER)
            };
            if let Some(v) = inst.write(Reg::S(code + k as u8)) {
                self.vals.insert(v, val);
            }
        }
        Ok(())
    }

    /// `s_load_dword*` from the program's constant table.
    fn code_smem(&mut self, inst: &Inst, a: Address) -> Result<(), Why> {
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar load");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u32,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar load from the code"),
        };
        let c = self.cur;
        let mut addr = self.s.m.i32(a.offset);
        if let Some(d) = a.dynamic {
            let d = self.i32_of(d)?;
            addr = self.s.m.bin(c, Bin::Add, addr, d);
        }
        for k in 0..count {
            let off = self.s.m.i32(4 * k);
            let at = self.s.m.bin(c, Bin::Add, addr, off);
            let x = self.code_dword(at)?;
            self.write(inst, Reg::S(code + k as u8), x, None);
        }
        Ok(())
    }

    /// The dword at a computed byte address of a constant buffer.
    fn cbuffer_dword(&mut self, h: V, a: V) -> V {
        let c = self.cur;
        let four = self.s.m.i32(4);
        let ri = self.s.m.bin(c, Bin::LShr, a, four);
        let r = self.s.op(
            c,
            "cbufferLoadLegacy.i32",
            Attr::ReadOnly,
            self.s.t.cbuf_ret,
            59,
            &[h, ri],
        );
        let two = self.s.m.i32(2);
        let three = self.s.m.i32(3);
        let comp = self.s.m.bin(c, Bin::LShr, a, two);
        let comp = self.s.m.bin(c, Bin::And, comp, three);
        let mut x = self.s.m.extract(c, r, 0);
        for j in 1..4 {
            let e = self.s.m.extract(c, r, j);
            let jc = self.s.m.i32(j);
            let is = self.s.m.cmp(c, pred::EQ, comp, jc);
            x = self.s.m.select(c, is, e, x);
        }
        x
    }

    /// `channels` channels of the element at byte `off` of raw buffer `h`,
    /// converted from the GFX10 buffer format `format` as the hardware's
    /// typed loads do (channels past the format's read 0, 0, 0, 1).
    // shortcut: the descriptor's channel swizzle is taken as the identity
    // (it is for raw views and the descriptors code builds), elements of 4
    // bytes or more as dword-aligned.
    fn format_load(&mut self, h: V, off: V, format: u8, channels: u32) -> Result<Vec<V>, Why> {
        let (bits, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let total: u32 = bits.iter().sum();
        let (u, align) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let load = |l: &mut Self, at: V, n: u32| -> Vec<V> {
            let mask = l.s.m.i8((1u8 << n) - 1);
            let r = l.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                l.s.t.res_ret,
                139,
                &[h, at, u, mask, align],
            );
            (0..n)
                .map(|k| {
                    let x = l.s.m.extract(c, r, k);
                    l.as_int(x)
                })
                .collect()
        };
        let words = if total < 32 {
            // A byte or half inside the dword that holds it.
            let (not3, three) = (self.s.m.i32(!3), self.s.m.i32(3));
            let at = self.s.m.bin(c, Bin::And, off, not3);
            let w = load(self, at, 1)[0];
            let byte = self.s.m.bin(c, Bin::And, off, three);
            let sh = self.s.m.bin(c, Bin::Shl, byte, three);
            vec![self.s.m.bin(c, Bin::LShr, w, sh)]
        } else {
            load(self, off, total / 32)
        };
        let float = |l: &mut Self, f: V| l.as_int(f);
        let mut out = Vec::new();
        let mut at = 0;
        for &w in bits {
            let word = words[(at / 32) as usize];
            let field = if w == 32 {
                word
            } else {
                let sh = self.s.m.i32(at % 32);
                let t = self.s.m.bin(c, Bin::LShr, word, sh);
                let m = self.s.m.i32((1 << w) - 1);
                self.s.m.bin(c, Bin::And, t, m)
            };
            at += w;
            // Sign-extended from the field's top bit.
            let signed = |l: &mut Self| {
                if w == 32 {
                    return field;
                }
                let k = l.s.m.i32(32 - w);
                let t = l.s.m.bin(c, Bin::Shl, field, k);
                l.s.m.bin(c, Bin::AShr, t, k)
            };
            let f32t = self.s.t.f32;
            // A normalized format's ends are exactly 1.0 and -1.0. The host
            // may divide through a reciprocal and land just below 1.0: RDNA
            // 4 loaded 16_16_UNORM's 65535 as the half 0x3BFF, not 0x3C00.
            let x = match numeric {
                Numeric::Unorm => {
                    let top = (1u64 << w) - 1;
                    let q = if w <= 16 {
                        let q = self.norm_quotient(field, w);
                        self.as_float(q)
                    } else {
                        let f = self.s.m.cast(c, Cast::UiToFp, field, f32t);
                        let max = self.s.m.f32((top as f32).to_bits());
                        self.s.m.bin(c, Bin::SDiv, f, max)
                    };
                    let (high, one) = (self.s.m.i32(top as u32), self.s.m.f32(1.0f32.to_bits()));
                    let at_top = self.s.m.cmp(c, pred::EQ, field, high);
                    let q = self.s.m.select(c, at_top, one, q);
                    float(self, q)
                }
                Numeric::Snorm => {
                    let s = signed(self);
                    let top = (1u32 << (w - 1)) - 1;
                    let q = if w <= 17 {
                        // The magnitude's quotient, signed (codes at and
                        // below -top are -1.0 below).
                        let (zero, sign) = (self.s.m.i32(0), self.s.m.i32(0x8000_0000));
                        let neg = self.s.m.cmp(c, pred::SLT, s, zero);
                        let minus = self.s.m.bin(c, Bin::Sub, zero, s);
                        let m = self.s.m.select(c, neg, minus, s);
                        let q = self.norm_quotient(m, w - 1);
                        let sign = self.s.m.select(c, neg, sign, zero);
                        let q = self.s.m.bin(c, Bin::Or, q, sign);
                        self.as_float(q)
                    } else {
                        let f = self.s.m.cast(c, Cast::SiToFp, s, f32t);
                        let max = self.s.m.f32((top as f32).to_bits());
                        self.s.m.bin(c, Bin::SDiv, f, max)
                    };
                    // The lowest value is -1.0 too, as is the one above it.
                    let (high, one) = (self.s.m.i32(top), self.s.m.f32(1.0f32.to_bits()));
                    let at_top = self.s.m.cmp(c, pred::SGE, s, high);
                    let q = self.s.m.select(c, at_top, one, q);
                    let low = self.s.m.i32(top.wrapping_neg());
                    let minus_one = self.s.m.f32((-1.0f32).to_bits());
                    let at_bottom = self.s.m.cmp(c, pred::SLE, s, low);
                    let q = self.s.m.select(c, at_bottom, minus_one, q);
                    float(self, q)
                }
                Numeric::Uscaled => {
                    let f = self.s.m.cast(c, Cast::UiToFp, field, f32t);
                    float(self, f)
                }
                Numeric::Sscaled => {
                    let s = signed(self);
                    let f = self.s.m.cast(c, Cast::SiToFp, s, f32t);
                    float(self, f)
                }
                Numeric::Uint => field,
                Numeric::Sint => signed(self),
                Numeric::Float if w == 32 => field,
                Numeric::Float => {
                    let h = match half_shift(w) {
                        0 => field,
                        k => {
                            let sh = self.s.m.i32(k);
                            self.s.m.bin(c, Bin::Shl, field, sh)
                        }
                    };
                    let f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
                    float(self, f)
                }
            };
            out.push(x);
        }
        let one = match numeric {
            Numeric::Uint | Numeric::Sint => 1,
            _ => 1.0f32.to_bits(),
        };
        for k in out.len() as u32..channels {
            out.push(self.s.m.i32(if k == 3 { one } else { 0 }));
        }
        out.truncate(channels as usize);
        Ok(out)
    }

    /// The bits of the float nearest to `a / d`, `d = 2^w - 1`, for codes
    /// `0 <= a < d` and `w <= 16`: what the console's buffer unit gives for
    /// a normalized code. The host's division is not exact (the RX 6700 XT
    /// returns 8-bit codes one ulp high and some 16-bit codes one ulp low),
    /// and titles truncate `x * d` back to the code (a material index in a
    /// vertex colour's alpha), which a quotient one ulp low turns into the
    /// code below.
    ///
    /// `Q = floor(a * 2^32 / d) = a * K + floor(a * r / d)` with `K =
    /// floor(2^32 / d)` and `r = 2^32 mod d` (no division when `r` is 1:
    /// widths dividing 32). `a / d` is never exact for `0 < a < d` (`d`
    /// odd, `r` coprime to it), so `Q` rounds to 24 bits up exactly when
    /// its highest dropped bit is set, and the result is that integer (at
    /// most 2^24, so its conversion is exact on any host) scaled by a power
    /// of two; `Q` itself when it fits, the quotient's further bits then
    /// lying below half an ulp. That holds for every code of every width but
    /// 11 and 12 (checked for all codes of widths 2 to 16), which take
    /// [`Self::exact_norm`]. The host's conversion of a larger integer is
    /// not relied on: WARP rounds those above 2^31 otherwise.
    fn norm_quotient(&mut self, a: V, w: u32) -> V {
        if matches!(w, 11 | 12) {
            return self.exact_norm(a, w);
        }
        let c = self.cur;
        let d = (1u64 << w) - 1;
        let (kk, r) = ((1u64 << 32) / d, (1u64 << 32) % d);
        let k = |l: &mut Self, v: u64| l.s.m.i32(v as u32);
        let (one, kv) = (k(self, 1), k(self, kk));
        let mut q = self.s.m.bin(c, Bin::Mul, a, kv);
        if r != 1 {
            let (rv, dv) = (k(self, r), k(self, d));
            let t = self.s.m.bin(c, Bin::Mul, a, rv);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            q = self.s.m.bin(c, Bin::Add, q, more);
        }
        // The bits to drop (1 to 8) from the exponent of `q >> 8`, which
        // converts exactly (fewer than 25 bits); none when `q` fits.
        let k8 = k(self, 8);
        let top = self.s.m.bin(c, Bin::LShr, q, k8);
        let top = self.s.m.cast(c, Cast::UiToFp, top, self.s.t.f32);
        let top = self.as_int(top);
        let k23 = k(self, 23);
        let e = self.s.m.bin(c, Bin::LShr, top, k23);
        let bias = k(self, 127 + 15);
        let shift = self.s.m.bin(c, Bin::Sub, e, bias);
        let (zero, b24) = (k(self, 0), k(self, 1 << 24));
        let fits = self.s.m.cmp(c, pred::IULT, q, b24);
        let shift = self.s.m.select(c, fits, zero, shift);
        // Rounded up when the highest dropped bit is set: `a / d` is never
        // exact, so a dropped half is more than half, never a tie.
        let up = self.s.m.bin(c, Bin::Shl, q, one);
        let up = self.s.m.bin(c, Bin::LShr, up, shift);
        let up = self.s.m.bin(c, Bin::And, up, one);
        let m = self.s.m.bin(c, Bin::LShr, q, shift);
        let m = self.s.m.bin(c, Bin::Add, m, up);
        // At most 2^24: exact as a float, and so its scale by 2^(shift - 32).
        let f = self.s.m.cast(c, Cast::UiToFp, m, self.s.t.f32);
        let b95 = k(self, 127 - 32);
        let scale = self.s.m.bin(c, Bin::Add, shift, b95);
        let scale = self.s.m.bin(c, Bin::Shl, scale, k23);
        let scale = self.as_float(scale);
        let f = self.s.m.bin(c, Bin::Mul, f, scale);
        self.as_int(f)
    }

    /// [`Self::norm_quotient`] in integers only, for any width up to 16: `Q`
    /// as there, 8 more bits when that leaves fewer than 25 (codes of wide
    /// formats near 0), then rounded to 24 bits to nearest even with the
    /// remainder as sticky bit, and assembled as float bits.
    fn exact_norm(&mut self, a: V, w: u32) -> V {
        let c = self.cur;
        let d = (1u64 << w) - 1;
        let (kk, r) = ((1u64 << 32) / d, (1u64 << 32) % d);
        let k = |l: &mut Self, v: u64| l.s.m.i32(v as u32);
        let (one, zero, dv, k8) = (k(self, 1), k(self, 0), k(self, d), k(self, 8));
        let kv = k(self, kk);
        let mut q = self.s.m.bin(c, Bin::Mul, a, kv);
        let mut rem = a;
        if r != 1 {
            let rv = k(self, r);
            let t = self.s.m.bin(c, Bin::Mul, a, rv);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            q = self.s.m.bin(c, Bin::Add, q, more);
            rem = self.s.m.bin(c, Bin::URem, t, dv);
        }
        // The exponent's bias less the 32 (or 40) fraction bits of `q`; the
        // shift below is added.
        let mut bias = k(self, 127 + 23 - 32);
        if kk < 1 << 24 {
            // Fewer than 25 bits for the smallest codes: 8 more.
            let t = self.s.m.bin(c, Bin::Shl, rem, k8);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            let rest = self.s.m.bin(c, Bin::URem, t, dv);
            let wide = self.s.m.bin(c, Bin::Shl, q, k8);
            let wide = self.s.m.bin(c, Bin::Or, wide, more);
            let limit = k(self, 1 << 24);
            let small = self.s.m.cmp(c, pred::IULT, q, limit);
            q = self.s.m.select(c, small, wide, q);
            rem = self.s.m.select(c, small, rest, rem);
            let b40 = k(self, 127 + 23 - 40);
            bias = self.s.m.select(c, small, b40, bias);
        }
        // Bits to drop: the bit length less 24, 1 to 8.
        let mut shift = one;
        for bit in 25..32 {
            let at = k(self, 1 << bit);
            let ge = self.s.m.cmp(c, pred::IUGE, q, at);
            let add = self.s.m.select(c, ge, one, zero);
            shift = self.s.m.bin(c, Bin::Add, shift, add);
        }
        let mant = self.s.m.bin(c, Bin::LShr, q, shift);
        let below = self.s.m.bin(c, Bin::Sub, shift, one);
        let half = self.s.m.bin(c, Bin::LShr, q, below);
        let half = self.s.m.bin(c, Bin::And, half, one);
        let low = self.s.m.bin(c, Bin::Shl, one, below);
        let low = self.s.m.bin(c, Bin::Sub, low, one);
        let low = self.s.m.bin(c, Bin::And, q, low);
        let rest = self.s.m.bin(c, Bin::Or, low, rem);
        let sticky = self.s.m.cmp(c, pred::NE, rest, zero);
        let odd = self.s.m.bin(c, Bin::And, mant, one);
        let tie = self.s.m.select(c, sticky, one, odd);
        let up = self.s.m.bin(c, Bin::And, half, tie);
        let mant = self.s.m.bin(c, Bin::Add, mant, up);
        // Rounding up to 2^24 moves to the next exponent.
        let k24 = k(self, 24);
        let carry = self.s.m.bin(c, Bin::LShr, mant, k24);
        let mant = self.s.m.bin(c, Bin::LShr, mant, carry);
        let e = self.s.m.bin(c, Bin::Add, bias, shift);
        let e = self.s.m.bin(c, Bin::Add, e, carry);
        let k23 = k(self, 23);
        let e = self.s.m.bin(c, Bin::Shl, e, k23);
        let fraction = k(self, 0x7F_FFFF);
        let m = self.s.m.bin(c, Bin::And, mant, fraction);
        let bits = self.s.m.bin(c, Bin::Or, e, m);
        let is_zero = self.s.m.cmp(c, pred::EQ, a, zero);
        self.s.m.select(c, is_zero, zero, bits)
    }

    /// Channels of buffer format `format`, as [`Self::format_load`] gives
    /// them, as d16 data: 16 bits each, two per dword, low half first (an
    /// odd last high half 0). Integer formats keep their low 16 bits; the
    /// others become f16.
    fn d16_channels(&mut self, format: u8, channels: Vec<V>) -> Result<Vec<V>, Why> {
        let (_, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let mut halves = Vec::new();
        for x in channels {
            halves.push(if matches!(numeric, Numeric::Uint | Numeric::Sint) {
                let m = self.s.m.i32(0xFFFF);
                self.s.m.bin(c, Bin::And, x, m)
            } else {
                let (f, i32t) = (self.as_float(x), self.s.t.i32);
                self.s
                    .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
            });
        }
        let mut out = Vec::new();
        for pair in halves.chunks(2) {
            out.push(match *pair {
                [lo, hi] => {
                    let k16 = self.s.m.i32(16);
                    let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                    self.s.m.bin(c, Bin::Or, lo, hi)
                }
                [lo] => lo,
                _ => unreachable!("chunks of one or two"),
            });
        }
        Ok(out)
    }

    /// The byte offset a buffer descriptor built around a root descriptor adds
    /// (constant and computed parts); none for other descriptors.
    fn built_offset(&mut self, src: &Source) -> Result<(u32, Option<V>), Why> {
        match src {
            Source::Built { address, .. } => {
                let x = match address.dynamic {
                    Some(v) => Some(self.i32_of(v)?),
                    None => None,
                };
                Ok((address.offset, x))
            }
            _ => Ok((0, None)),
        }
    }

    /// Decode a table entry for byte arithmetic. CBVs and invalid slots
    /// have no buffer stride; the UAV flag is never part of an offset.
    fn buffer_stride(&mut self, info: V) -> V {
        let cbv = self.s.m.i32(super::shader::SLOT_STRIDE_CBV);
        let mask = self.s.m.i32(!super::shader::SLOT_STRIDE_UAV);
        let zero = self.s.m.i32(0);
        let is_cbv = self.s.m.cmp(self.cur, pred::EQ, info, cbv);
        let stride = self.s.m.bin(self.cur, Bin::And, info, mask);
        self.s.m.select(self.cur, is_cbv, zero, stride)
    }

    /// Untagged bytes per element for vector buffer address arithmetic.
    fn slot_stride(&mut self, src: &Source) -> Result<Option<V>, Why> {
        Ok(self.slot_info(src)?.map(|info| self.buffer_stride(info)))
    }

    /// The host's stride/kind entry for a computed ResourceHeap index.
    fn slot_info(&mut self, src: &Source) -> Result<Option<V>, Why> {
        let Source::Loaded { load, first } = src else {
            return Ok(None);
        };
        let heap = self
            .scalar
            .get(load)
            .and_then(|a| self.x.native.layout.get(a.param))
            .is_some_and(|p| p.root == RootType::ResourceHeap);
        if !heap {
            return Ok(None);
        }
        let key = (self.cur, *load, *first);
        if let Some(&v) = self.strides.get(&key) {
            return Ok(Some(v));
        }
        let c = self.cur;
        let slot = self.heap_index(*load, *first, false)?;
        // Inside the table whatever the index (lanes that do not run
        // compute one too): a root view has no bounds of its own.
        let last = self.s.m.i32(super::shader::SLOT_STRIDES_COUNT - 1);
        let i32t = self.s.t.i32;
        let slot = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[slot, last]);
        self.s.m.tag(slot, Tag::Clamp);
        let two = self.s.m.i32(2);
        let byte = self.s.m.bin(c, Bin::Shl, slot, two);
        let h = self.s.root_srv_handle(super::shader::SLOT_STRIDES);
        let iu = self.s.m.undef(self.s.t.i32);
        let mask = self.s.m.i8(1);
        let align = self.s.m.i32(4);
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[h, byte, iu, mask, align],
        );
        let f = self.s.m.extract(c, r, 0);
        let v = self.as_int(f);
        self.strides.insert(key, v);
        Ok(Some(v))
    }

    /// The untagged byte stride from the host's descriptor metadata.
    fn view_stride(&mut self, src: &Source) -> Result<V, Why> {
        if let Some(stride) = self.slot_stride(src)? {
            return Ok(stride);
        }
        const WHY: Why = "a structured buffer of unknown stride";
        let heap = |load: &Site| {
            self.scalar
                .get(load)
                .and_then(|a| self.x.native.layout.get(a.param))
                .is_some_and(|p| p.root == RootType::ResourceHeap)
        };
        let leaves = src.leaves();
        if leaves.is_empty()
            || !leaves.iter().all(|l| match l {
                Source::Loaded { load, .. } | Source::Fetched { load, .. } => heap(load),
                _ => false,
            })
        {
            return Err(WHY);
        }
        let index = match src {
            Source::Loaded { load, first } => self.heap_index(*load, *first, false)?,
            Source::Phi { .. } => return Err(WHY),
            _ => self.lane_heap_index(src, false)?,
        };
        let c = self.cur;
        let four = self.s.m.i32(4);
        let at = self.s.m.bin(c, Bin::Mul, index, four);
        let table = self.s.root_srv_handle(VIEW_STRIDES);
        let iu = self.s.m.undef(self.s.t.i32);
        let mask = self.s.m.i8(1);
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[table, at, iu, mask, four],
        );
        let x = self.s.m.extract(c, r, 0);
        let word = self.as_int(x);
        let bits = self.s.m.i32(VIEW_STRIDE_BITS);
        Ok(self.s.m.bin(c, Bin::And, word, bits))
    }

    /// The used-resource entry a descriptor loaded from a table falls in.
    fn table_place(&self, src: &Source) -> Option<(u8, u32)> {
        let Source::Loaded { load, first } = src else {
            return None;
        };
        let a = self.scalar.get(load)?;
        let rp = self.x.native.layout.get(a.param)?;
        (rp.root == RootType::Table && a.dynamic.is_none())
            .then(|| (rp.slot, a.offset / 4 + u32::from(*first)))
    }

    fn used_kind(&self, src: &Source) -> Option<&'a UsedResource> {
        if let Source::Phi { args, .. } | Source::Select { args, .. } | Source::Lanes { args, .. } =
            src
        {
            return self.used_kind(args.first()?);
        }
        if let Source::LanePhi { args, .. } = src {
            return args.iter().flatten().find_map(|a| self.used_kind(a));
        }
        if let Source::Built { address, .. } = src {
            let rp = self.x.native.layout.get(address.param)?;
            return self
                .x
                .native
                .used
                .iter()
                .find(|u| u.root == RootType::Descriptor && u.user_data == rp.user_data);
        }
        let (Source::Loaded { load, first } | Source::Fetched { load, first }) = src else {
            return None;
        };
        let a = self.scalar.get(load)?;
        let rp = self.x.native.layout.get(a.param)?;
        if rp.root != RootType::Table {
            return None;
        }
        let dword = (a.offset / 4) + u32::from(*first);
        self.x
            .native
            .used
            .iter()
            .filter(|u| {
                u.root == RootType::Table
                    && u.user_data == rp.user_data
                    && u.table_offset_dwords <= dword
            })
            .max_by_key(|u| u.table_offset_dwords)
    }

    /// A handle for the descriptor `src` read at `site`; the heap index of
    /// a texture shader view the program takes from the heap itself is noted
    /// for the instruction's read mark (handles are made once per block,
    /// often before the instruction). A view read through a table needs
    /// none: the host makes the slot for the draw.
    fn handle(
        &mut self,
        site: Site,
        src: &Source,
        sampler: bool,
        props: (u32, u32),
    ) -> Result<V, Why> {
        let h = self.handle_made(site, src, sampler, props)?;
        if let Some(&index) = self.s.texture_index.get(&h)
            && self.is_heap_direct(src)
        {
            self.s.texture_reads.push(index);
        }
        Ok(h)
    }

    fn handle_made(
        &mut self,
        site: Site,
        src: &Source,
        sampler: bool,
        props: (u32, u32),
    ) -> Result<V, Why> {
        let props = if self.pops && props.0 & 0x1000 != 0 {
            self.s.flags |= flags::ROVS;
            (props.0 | 0x2000, props.1)
        } else {
            props
        };
        // A descriptor from the resource heap itself: the host cannot tell
        // which slots the program reads, or writes through unordered access.
        if !sampler && self.is_heap_direct(src) {
            self.heap[0] = true;
            self.heap[1] |= props.0 & 0x1000 != 0;
        }
        match src {
            Source::Loaded { load, first } if let Some(address) = self.entry_address(*load) => {
                // The same descriptor wherever it is loaded: the console
                // compiler reloads one before every use rather than keep it
                // (ActivateRays: 2,399 loads of one), and one handle made in
                // the entry block serves them all.
                let key = (address, *first, sampler, props);
                if let Some(&h) = self.entry_handles.get(&key) {
                    return Ok(h);
                }
                let here = std::mem::replace(&mut self.cur, self.s.entry);
                let index = self.entry_heap_index(address, *first, sampler);
                let index = self.buffer_guard(src, index, sampler, props);
                let h = self.s.heap_handle(self.cur, index, sampler, false, props);
                self.cur = here;
                self.entry_handles.insert(key, h);
                Ok(h)
            }
            Source::Loaded { load, first } => {
                let key = (self.cur, *load, *first, sampler, props);
                if let Some(&h) = self.handles.get(&key) {
                    return Ok(h);
                }
                let index = self.heap_index(*load, *first, sampler)?;
                let index = self.buffer_guard(src, index, sampler, props);
                // A waterfall loop's load at the first lane's offset reads
                // each lane's own entry (see `resource::Waterfall`).
                let per_lane = self.x.res.waterfall.loads.contains(load);
                let h = self
                    .s
                    .heap_handle(self.cur, index, sampler, per_lane, props);
                self.handles.insert(key, h);
                Ok(h)
            }
            Source::Constant(words) if sampler => {
                let words: [u32; 4] = words
                    .get(..4)
                    .and_then(|w| w.try_into().ok())
                    .ok_or("a sampler constant")?;
                let comparison = props.0 & 0x8000 != 0;
                let slot = self.samplers.iter().enumerate().position(|(n, w)| {
                    *w == words && (self.s.cmp_samplers >> n & 1 != 0) == comparison
                });
                let n = match slot {
                    Some(n) => n,
                    None => {
                        let n = self.samplers.len();
                        if n >= u64::BITS as usize {
                            return Err("more than 64 immediate sampler slots");
                        }
                        self.samplers.push(words);
                        self.s.cmp_samplers |= u64::from(comparison) << n;
                        n
                    }
                };
                Ok(self.s.sampler_handle(self.cur, n as u32))
            }
            Source::Constant(_) => Err("a constant resource descriptor"),
            Source::Built { address, .. } if address.param == MEMORY => {
                Err("a buffer descriptor built around a computed address")
            }
            Source::Built { address, .. } => {
                let rp = *self
                    .x
                    .native
                    .layout
                    .get(address.param)
                    .ok_or("a descriptor built around a table in the code")?;
                let kind = self
                    .x
                    .native
                    .used
                    .iter()
                    .find(|u| u.user_data == rp.user_data)
                    .map(|u| u.kind);
                match (rp.root, kind, props.0) {
                    (RootType::Descriptor, Some(ResourceType::Cbv) | None, 13) => {
                        Ok(self.s.root_cbv_handle(rp.slot))
                    }
                    (RootType::Descriptor, Some(ResourceType::Srv), 11) if !self.root_memory => {
                        Ok(self.root_view(address.param, false))
                    }
                    (RootType::Descriptor, Some(ResourceType::Srv), 11) => {
                        Ok(self.s.root_srv_handle(rp.slot))
                    }
                    (RootType::Descriptor, Some(ResourceType::Uav) | None, 0x100B)
                    | (RootType::Descriptor, Some(ResourceType::Uav), 11)
                        if !self.root_memory =>
                    {
                        Ok(self.root_view(address.param, true))
                    }
                    (RootType::Descriptor, Some(ResourceType::Uav) | None, 0x100B)
                    | (RootType::Descriptor, Some(ResourceType::Uav), 11) => {
                        Ok(self.s.root_uav_handle(rp.slot))
                    }
                    _ => Err("a root UAV or typed root view"),
                }
            }
            Source::Phi { block, args }
                if sampler && matches!(args[..], [Source::Constant(_), Source::Constant(_)]) =>
            {
                // One of two immediate samplers, chosen by uniform control
                // flow: what reads it runs with both, the edge taken picks
                // the result (static samplers cannot be indexed).
                let [a, b] = &args[..] else { unreachable!() };
                let ha = self.handle(site, a, sampler, props)?;
                let hb = self.handle(site, b, sampler, props)?;
                let join = self.block[*block].ok_or("a descriptor chosen by control flow")?;
                let mut incoming = Vec::new();
                for (j, &q) in self.x.p.blocks[*block].preds.iter().enumerate() {
                    if self.block[q].is_none() {
                        continue;
                    }
                    let tail = self.tail[q].ok_or("a descriptor chosen by control flow")?;
                    let at = self.edges.get(&(q, *block)).copied().unwrap_or(tail);
                    incoming.push((self.s.m.bool(j == 0), at));
                }
                let phi = self.s.m.phi(join, self.s.t.i1);
                self.s.m.set_phi(phi, incoming);
                self.s.sampler_choices.push((ha, hb, phi));
                Ok(ha)
            }
            Source::Phi { block, .. } => {
                // One handle at the join for every access the choice reaches
                // (each is in a block the join dominates).
                let _ = site;
                let key = (format!("{src:?}"), sampler, props);
                if let Some(&h) = self.join_handles.get(&key) {
                    return Ok(h);
                }
                let join = self.block[*block].ok_or("a descriptor chosen by control flow")?;
                let here = std::mem::replace(&mut self.cur, join);
                let index = self.uniform_heap_index(src, sampler);
                let h = index.map(|index| {
                    let index = self.buffer_guard(src, index, sampler, props);
                    self.s.heap_handle(self.cur, index, sampler, false, props)
                });
                self.cur = here;
                let h = h?;
                self.join_handles.insert(key, h);
                Ok(h)
            }
            Source::Select { scc, args } => {
                // The heap index of each choice, picked by SCC (uniform).
                let [
                    Source::Loaded { load: a, first: fa },
                    Source::Loaded { load: b, first: fb },
                ] = args[..]
                else {
                    return Err("a descriptor chosen by a condition");
                };
                let cond = match self.get(*scc)? {
                    Val::Bit(x) => x,
                    _ => return Err("SCC without a value"),
                };
                let set = self.heap_index(a, fa, sampler)?;
                let clear = self.heap_index(b, fb, sampler)?;
                let index = self.s.m.select(self.cur, cond, set, clear);
                let index = self.buffer_guard(src, index, sampler, props);
                Ok(self.s.heap_handle(self.cur, index, sampler, false, props))
            }
            Source::Fetched { .. } | Source::Lanes { .. } | Source::LanePhi { .. } => {
                // A descriptor each lane has its own of (a waterfall loop
                // reads it from the first active lane): each lane's heap
                // index, non-uniform.
                let index = self.lane_heap_index(src, sampler)?;
                let index = self.buffer_guard(src, index, sampler, props);
                Ok(self.s.heap_handle(self.cur, index, sampler, true, props))
            }
        }
    }

    /// The heap index to read a buffer view at, for a descriptor `src` from
    /// the resource heap itself (module notes, "Buffer views at computed
    /// heap indices"): `index`, or the host's null raw buffer view when the
    /// view-stride table says the slot holds no buffer view. Indices come
    /// from data; one naming a texture slot read a texture descriptor as a
    /// buffer one (a wild address: the texture's address shifted 8 bits
    /// down) and faulted the device.
    fn buffer_guard(&mut self, src: &Source, index: V, sampler: bool, props: (u32, u32)) -> V {
        // Typed, raw and structured buffers (DXIL resource kinds 10-12).
        if sampler || !(10..=12).contains(&(props.0 & 0xFF)) || !self.is_heap_direct(src) {
            return index;
        }
        let c = self.cur;
        // Offline translation has no device-provided cap (zero): retain
        // the full heap ABI instead of underflowing that sentinel.
        let last = super::shader::view_heap_limit()
            .checked_sub(1)
            .unwrap_or(super::HEAP_VIEWS - 1);
        let last = self.s.m.i32(last);
        let over = self.s.m.cmp(c, pred::IUGT, index, last);
        let index = self.s.m.select(c, over, last, index);
        let four = self.s.m.i32(4);
        let at = self.s.m.bin(c, Bin::Mul, index, four);
        let table = self.s.root_srv_handle(VIEW_STRIDES);
        let (iu, mask) = (self.s.m.undef(self.s.t.i32), self.s.m.i8(1));
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[table, at, iu, mask, four],
        );
        let x = self.s.m.extract(c, r, 0);
        let stride = self.as_int(x);
        let zero = self.s.m.i32(0);
        let none = self.s.m.cmp(c, pred::EQ, stride, zero);
        // The null unordered access view sits just below the null SRV.
        let null = super::shader::null_buffer_view() - u32::from(props.0 & 0x1000 != 0);
        let null = self.s.m.i32(null);
        self.s.m.select(c, none, null, index)
    }

    /// Whether a descriptor comes from the resource heap itself (root type
    /// 5, not a table) on some path.
    fn is_heap_direct(&self, src: &Source) -> bool {
        src.leaves().iter().any(|l| match l {
            Source::Loaded { load, .. } | Source::Fetched { load, .. } => self
                .scalar
                .get(load)
                .and_then(|a| self.x.native.layout.get(a.param))
                .is_some_and(|p| p.root == RootType::ResourceHeap),
            _ => false,
        })
    }

    /// The address of the descriptor load at `load` when root parameters
    /// alone give it (a table or a heap, at a fixed offset: resource
    /// tracking found no other term), and not per lane.
    fn entry_address(&self, load: Site) -> Option<EntryAddress> {
        let a = self.scalar.get(&load)?;
        let fixed = a.dynamic.is_none() && !self.x.res.waterfall.loads.contains(&load);
        let root = self.x.native.layout.get(a.param)?.root;
        let heap = matches!(
            root,
            RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap
        );
        (fixed && heap).then_some((a.param, a.offset))
    }

    /// [`Lower::heap_index`] of a descriptor at a fixed address
    /// ([`Lower::entry_address`]), in the entry block.
    fn entry_heap_index(&mut self, (param, offset): EntryAddress, first: u8, sampler: bool) -> V {
        let entry = self.s.entry;
        let base = match self.x.native.layout[param].root {
            RootType::Table => self.user_dword(param, 0),
            _ => self.s.m.i32(0),
        };
        let k = self.s.m.i32(offset.wrapping_add(4 * u32::from(first)));
        let addr = self.s.m.bin(entry, Bin::Add, base, k);
        let shift = self.s.m.i32(if sampler { 4 } else { 5 });
        self.s.m.bin(entry, Bin::LShr, addr, shift)
    }

    /// The heap index of a descriptor every lane shares, computed in the
    /// current block. One chosen by control flow is a phi at its join of the
    /// index each predecessor computes at its end; an edge may bring another
    /// join's (nested if/else).
    fn uniform_heap_index(&mut self, src: &Source, sampler: bool) -> Result<V, Why> {
        match src {
            Source::Loaded { load, first } => self.heap_index(*load, *first, sampler),
            Source::Phi { block, args } => {
                let mut incoming = Vec::new();
                for (&q, arg) in self.x.p.blocks[*block].preds.iter().zip(args) {
                    if self.block[q].is_none() {
                        continue;
                    }
                    let tail = self.tail[q].ok_or("a descriptor chosen by control flow")?;
                    let at = self.edges.get(&(q, *block)).copied().unwrap_or(tail);
                    // No lane comes this way (`resource` gave it another
                    // edge's source): any index will do.
                    if self.never_taken(q, *block) {
                        incoming.push((self.s.m.i32(0), at));
                        continue;
                    }
                    let here = std::mem::replace(&mut self.cur, at);
                    let index = self.uniform_heap_index(arg, sampler);
                    self.cur = here;
                    incoming.push((index?, at));
                }
                let join = self.block[*block].ok_or("a descriptor chosen by control flow")?;
                let phi = self.s.m.phi(join, self.s.t.i32);
                self.s.m.set_phi(phi, incoming);
                Ok(phi)
            }
            _ => Err("a descriptor chosen by control flow"),
        }
    }

    /// The heap index of a descriptor that may differ between lanes,
    /// computed in the current block (or, chosen by control flow, at the
    /// join).
    fn lane_heap_index(&mut self, src: &Source, sampler: bool) -> Result<V, Why> {
        match src {
            Source::Loaded { load, first } => self.heap_index(*load, *first, sampler),
            Source::Fetched { load, first } => self.fetched_index(*load, *first, sampler),
            Source::Lanes { mask, args } | Source::Select { scc: mask, args } => {
                let [a, b] = &args[..] else {
                    return Err("a descriptor chosen per lane");
                };
                let cond = if matches!(src, Source::Lanes { .. }) {
                    let &m = self
                        .x
                        .st
                        .masks
                        .get(mask)
                        .ok_or("a descriptor chosen per lane by an untracked mask")?;
                    self.boolean(m, self.cur)?
                } else {
                    match self.get(*mask)? {
                        Val::Bit(x) => x,
                        _ => return Err("SCC without a value"),
                    }
                };
                let set = self.lane_heap_index(a, sampler)?;
                let clear = self.lane_heap_index(b, sampler)?;
                Ok(self.s.m.select(self.cur, cond, set, clear))
            }
            Source::LanePhi { .. } if self.twin.is_some() => {
                Err("a per-lane descriptor chosen by the lanes' ways, in paired lanes")
            }
            Source::LanePhi { block, args } => {
                // Each predecessor computes its heap index at its end (what
                // it reads is there: the dwords are, on that edge); the join
                // picks the one of the edge the lane came by. An edge no
                // lane reading it takes gives index 0.
                let join =
                    self.block[*block].ok_or("a per-lane descriptor chosen by control flow")?;
                let mut incoming = Vec::new();
                for (&q, arg) in self.x.p.blocks[*block].preds.iter().zip(args) {
                    if self.block[q].is_none() {
                        continue;
                    }
                    let tail =
                        self.tail[q].ok_or("a per-lane descriptor joined from a later block")?;
                    let at = self.edges.get(&(q, *block)).copied().unwrap_or(tail);
                    let index = match arg {
                        Some(arg) => {
                            let here = std::mem::replace(&mut self.cur, at);
                            let index = self.lane_heap_index(arg, sampler);
                            self.cur = here;
                            index?
                        }
                        None => self.s.m.i32(0),
                    };
                    incoming.push((index, at));
                }
                let phi = self.s.m.phi(join, self.s.t.i32);
                self.s.m.set_phi(phi, incoming);
                Ok(phi)
            }
            _ => Err("a per-lane descriptor from neither a heap load nor a choice of them"),
        }
    }

    /// The heap index of the descriptor at dword `first` of what the global
    /// load at `load` reads, in each lane: its address (base plus offsets)
    /// over the descriptor size.
    fn fetched_index(&mut self, load: Site, first: u8, sampler: bool) -> Result<V, Why> {
        const WHY: Why = "a per-lane descriptor load";
        let a = *self
            .scalar
            .get(&load)
            .ok_or("an untracked descriptor load")?;
        match self.x.native.layout.get(a.param).map(|p| p.root) {
            Some(RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap) => {}
            _ => return Err("a descriptor outside the heaps"),
        }
        let li: &Inst = &self.x.p.blocks[load.0].insts[load.1];
        let Fields::Flat {
            addr: Some(Operand::Vector { reg, .. }),
            saddr,
            offset,
            ..
        } = li.fields
        else {
            return Err(WHY);
        };
        let c = self.cur;
        // The low dword of the address: SGPR base plus VGPR offset, or the
        // VGPR pair's low half (heap addresses fit in 32 bits).
        let mut at = self.i32_of(li.read(Reg::V(reg)).ok_or(WHY)?)?;
        if let Some(Operand::Scalar { code, .. }) = saddr
            && code != NULL
        {
            let base = self.i32_of(li.read(Reg::S(code)).ok_or(WHY)?)?;
            at = self.s.m.bin(c, Bin::Add, base, at);
        }
        let k = self
            .s
            .m
            .i32((offset as i32 as u32).wrapping_add(4 * u32::from(first)));
        at = self.s.m.bin(c, Bin::Add, at, k);
        let shift = self.s.m.i32(if sampler { 4 } else { 5 });
        Ok(self.s.m.bin(c, Bin::LShr, at, shift))
    }

    /// The heap index of the descriptor at dword `first` of what the scalar
    /// load at `load` reads, computed in the current block.
    fn heap_index(&mut self, load: Site, first: u8, sampler: bool) -> Result<V, Why> {
        let a = *self
            .scalar
            .get(&load)
            .ok_or("an untracked descriptor load")?;
        match self.x.native.layout.get(a.param).map(|p| p.root) {
            Some(RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap) => {}
            _ => return Err("a descriptor outside the heaps"),
        }
        let li: &Inst = &self.x.p.blocks[load.0].insts[load.1];
        let Fields::Smem {
            base,
            offset,
            soffset,
            ..
        } = &li.fields
        else {
            return Err("a descriptor load");
        };
        let Operand::Scalar { code, .. } = *base else {
            return Err("a descriptor load");
        };
        let c = self.cur;
        let base = li.read(Reg::S(code)).ok_or("a descriptor load")?;
        let mut addr = self.i32_of(base)?;
        let k = self
            .s
            .m
            .i32((*offset as u32).wrapping_add(4 * u32::from(first)));
        addr = self.s.m.bin(c, Bin::Add, addr, k);
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            let so = self.operand(li, soffset)?;
            addr = self.s.m.bin(c, Bin::Add, addr, so);
        }
        let shift = self.s.m.i32(if sampler { 4 } else { 5 });
        Ok(self.s.m.bin(c, Bin::LShr, addr, shift))
    }

    fn valu(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let name = inst.name();
        if self.x.res.waterfall.compares.contains(&(b, i)) {
            // The first lane's dwords of a waterfall loop are each lane's
            // own (see `resource::Waterfall`).
            self.lane(inst, Some(true));
            return Ok(());
        }
        if let Some(rest) = name.strip_prefix("v_cmp_").or(name.strip_prefix("v_cmpx_")) {
            let r = if v.dpp.is_some() {
                Err("a compare with DPP")
            } else {
                self.compare(inst, rest)
            };
            if let Err(why) = r {
                for &(_, w) in &inst.writes {
                    self.lane_fail.insert(w, why);
                }
            }
            return r;
        }
        // A scalar result (`v_readlane`, `v_readfirstlane`) is written
        // whatever EXEC holds.
        let mut guard = match v.dst {
            Some(Operand::Scalar { .. }) => None,
            _ => self.guard(b, i)?,
        };
        if let Some(d) = v.dpp {
            let (x, enable) = self.dpp(inst, d)?;
            guard = match (guard, enable) {
                (Some(g), Some(e)) => Some(self.s.m.bin(self.cur, Bin::And, g, e)),
                (g, e) => g.or(e),
            };
            self.src0 = Some(x);
        }
        let r = self.valu_op(inst, guard);
        self.src0 = None;
        r
    }

    /// DPP: src0 as read from another lane, and which lanes write their
    /// result (none: all).
    fn dpp(&mut self, inst: &Inst, dpp: Dpp) -> Result<(V, Option<V>), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let Some(&Operand::Vector { reg, .. }) = v.src.first() else {
            return Err("a DPP operand");
        };
        let x = self.i32_of(inst.read(Reg::V(reg)).ok_or("a DPP operand")?)?;
        // Quad permutations and DPP8 stay in groups the pixel layout shares.
        let quad = matches!(dpp, Dpp::Row { ctrl: 0..=0xFF, .. });
        let mesh_quad = self.s.mesh.is_some() && quad;
        if !mesh_quad {
            self.cross_lane(quad)?;
        }
        let c = self.cur;
        let lane = self.lane_index();
        let k = |l: &mut Self, n: u32| l.s.m.i32(n);
        let (sixteen_mask, low) = (k(self, !15), k(self, 15));
        let row = self.s.m.bin(c, Bin::And, lane, sixteen_mask);
        let pos = self.s.m.bin(c, Bin::And, lane, low);
        // Source lane, and whether it lies in the row.
        let (src, inside, masks, zero) = match dpp {
            Dpp::Lanes { sel, .. } => {
                let (seven, not7, three) = (k(self, 7), k(self, !7), k(self, 3));
                let p = self.s.m.bin(c, Bin::And, lane, seven);
                let sh = self.s.m.bin(c, Bin::Mul, p, three);
                let sel = k(self, sel);
                let pick = self.s.m.bin(c, Bin::LShr, sel, sh);
                let pick = self.s.m.bin(c, Bin::And, pick, seven);
                let base = self.s.m.bin(c, Bin::And, lane, not7);
                (self.s.m.bin(c, Bin::Or, base, pick), None, None, false)
            }
            Dpp::Row {
                ctrl,
                row_mask,
                bank_mask,
                bound_ctrl,
                ..
            } => {
                let n = u32::from(ctrl & 15);
                let (src, inside) = match ctrl {
                    0..=0xFF => {
                        let (three, one, not3) = (k(self, 3), k(self, 1), k(self, !3));
                        let p = self.s.m.bin(c, Bin::And, lane, three);
                        let sh = self.s.m.bin(c, Bin::Shl, p, one);
                        let perm = k(self, u32::from(ctrl));
                        let pick = self.s.m.bin(c, Bin::LShr, perm, sh);
                        let pick = self.s.m.bin(c, Bin::And, pick, three);
                        let base = self.s.m.bin(c, Bin::And, lane, not3);
                        (self.s.m.bin(c, Bin::Or, base, pick), None)
                    }
                    0x101..=0x10F | 0x111..=0x11F => {
                        let nn = k(self, n);
                        let left = ctrl < 0x110;
                        let (op, p) = if left {
                            (Bin::Add, pred::IULT)
                        } else {
                            (Bin::Sub, pred::IUGE)
                        };
                        let s = self.s.m.bin(c, op, lane, nn);
                        // Left: pos + n < 16; right: pos >= n.
                        let bound = if left { k(self, 16 - n) } else { nn };
                        (s, Some(self.s.m.cmp(c, p, pos, bound)))
                    }
                    0x121..=0x12F => {
                        let nn = k(self, n);
                        let t = self.s.m.bin(c, Bin::Sub, pos, nn);
                        let t = self.s.m.bin(c, Bin::And, t, low);
                        (self.s.m.bin(c, Bin::Or, row, t), None)
                    }
                    0x140 | 0x141 => {
                        let m = k(self, if ctrl == 0x140 { 15 } else { 7 });
                        let t = self.s.m.bin(c, Bin::Xor, lane, m);
                        (t, None)
                    }
                    0x150..=0x15F => {
                        let nn = k(self, n);
                        (self.s.m.bin(c, Bin::Or, row, nn), None)
                    }
                    0x160..=0x16F => {
                        let nn = k(self, n);
                        (self.s.m.bin(c, Bin::Xor, lane, nn), None)
                    }
                    _ => return Err("a DPP lane pattern"),
                };
                let masks = (row_mask != 0xF || bank_mask != 0xF).then_some((row_mask, bank_mask));
                (src, inside, masks, bound_ctrl)
            }
        };
        let mut r = if mesh_quad || self.helpers_read(quad) {
            self.quad_pick(x, src)
        } else {
            self.read_lane(x, src)
        };
        let mut enable = None;
        if let Some(inside) = inside {
            if zero {
                let z = k(self, 0);
                r = self.s.m.select(c, inside, r, z);
            } else {
                enable = Some(inside);
            }
        }
        if let Some((rows, banks)) = masks {
            // Row (16 lanes) and bank (4 lanes of a row) write masks.
            let (four, two, three) = (k(self, 4), k(self, 2), k(self, 3));
            let row_lane = if self.twin.is_some() {
                self.console_lane()
            } else {
                lane
            };
            let ri = self.s.m.bin(c, Bin::LShr, row_lane, four);
            let ri = self.s.m.bin(c, Bin::And, ri, three);
            let bi = self.s.m.bin(c, Bin::LShr, lane, two);
            let bi = self.s.m.bin(c, Bin::And, bi, three);
            let (rm, bm) = (k(self, u32::from(rows)), k(self, u32::from(banks)));
            let rb = self.s.m.bin(c, Bin::LShr, rm, ri);
            let bb = self.s.m.bin(c, Bin::LShr, bm, bi);
            let both = self.s.m.bin(c, Bin::And, rb, bb);
            let (one, z) = (k(self, 1), k(self, 0));
            let both = self.s.m.bin(c, Bin::And, both, one);
            let on = self.s.m.cmp(c, pred::NE, both, z);
            enable = Some(match enable {
                Some(e) => self.s.m.bin(c, Bin::And, e, on),
                None => on,
            });
        }
        Ok((r, enable))
    }

    fn valu_op(&mut self, inst: &Inst, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let name = inst.name();
        let c = self.cur;
        let f = |l: &mut Self, k| l.fsrc(inst, k);
        let s = |l: &mut Self, k| l.src(inst, k);
        let float = |l: &mut Self, r: V| l.write_f(inst, r, guard);
        let int = |l: &mut Self, r: V| l.write_i(inst, r, guard);
        if let Some(r) = self.valu16(inst, name, guard) {
            return r;
        }
        if let Some(op) = name.strip_prefix("v_pk_") {
            return self.packed16(inst, op, guard);
        }
        match name {
            "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => {
                let n = s(self, 0)?;
                let x = self.src64(inst, 1)?;
                let i64t = self.s.m.int(64);
                let k63 = self.s.m.i32(63);
                let n = self.s.m.bin(c, Bin::And, n, k63);
                let n = self.s.m.cast(c, Cast::ZExt, n, i64t);
                let op = match name {
                    "v_lshlrev_b64" => Bin::Shl,
                    "v_lshrrev_b64" => Bin::LShr,
                    _ => Bin::AShr,
                };
                let r = self.s.m.bin(c, op, x, n);
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("a 64-bit result");
                };
                let (lo, hi) = self.split64(r);
                self.write(inst, Reg::V(reg), lo, guard);
                self.write(inst, Reg::V(reg + 1), hi, guard);
                Ok(())
            }
            "v_mad_u64_u32" | "v_mad_i64_i32" => {
                // 32 × 32 → 64-bit product plus a 64-bit addend (address
                // arithmetic); the carry out is a lane mask.
                let signed = name == "v_mad_i64_i32";
                let (a, bb) = (s(self, 0)?, s(self, 1)?);
                let addend = self.src64(inst, 2)?;
                let i64t = self.s.m.int(64);
                let ext = if signed { Cast::SExt } else { Cast::ZExt };
                let a = self.s.m.cast(c, ext, a, i64t);
                let bb = self.s.m.cast(c, ext, bb, i64t);
                let p = self.s.m.bin(c, Bin::Mul, a, bb);
                let r = self.s.m.bin(c, Bin::Add, p, addend);
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("a 64-bit result");
                };
                let (lo, hi) = self.split64(r);
                self.write(inst, Reg::V(reg), lo, guard);
                self.write(inst, Reg::V(reg + 1), hi, guard);
                if !signed
                    && let Some(Operand::Scalar { code, .. }) = v.carry
                    && let Some(w) = inst.write(Reg::S(code))
                {
                    let carry = self.s.m.cmp(c, pred::IULT, r, addend);
                    self.lane_bits.insert(w, carry);
                }
                for &(_, w) in &inst.writes {
                    self.vals
                        .entry(w)
                        .or_insert(Val::Missing("a lane mask read as data"));
                }
                Ok(())
            }
            "v_cubeid_f32" | "v_cubesc_f32" | "v_cubetc_f32" | "v_cubema_f32" => {
                // Cube-map face coordinates (AMD's ISA): the major axis is z
                // when |z| >= |x| and |z| >= |y|, else y when |y| >= |x|,
                // else x.
                let (x, y, z) = (f(self, 0)?, f(self, 1)?, f(self, 2)?);
                let (ax, ay, az) = (self.unary(6, x), self.unary(6, y), self.unary(6, z));
                let zx = self.s.m.cmp(c, pred::OGE, az, ax);
                let zy = self.s.m.cmp(c, pred::OGE, az, ay);
                let zmaj = self.s.m.bin(c, Bin::And, zx, zy);
                let ymaj = self.s.m.cmp(c, pred::OGE, ay, ax);
                let zero = self.s.m.f32(0);
                let neg = |l: &mut Self, v: V| l.s.m.cmp(c, pred::OLT, v, zero);
                let minus = |l: &mut Self, v: V| {
                    let z = l.s.m.f32(0x8000_0000);
                    l.s.m.bin(c, Bin::Sub, z, v)
                };
                let pick = |l: &mut Self, zv: V, yv: V, xv: V| {
                    let t = l.s.m.select(c, ymaj, yv, xv);
                    l.s.m.select(c, zmaj, zv, t)
                };
                let r = match name {
                    "v_cubeid_f32" => {
                        let k = |l: &mut Self, v: f32| l.s.m.f32(v.to_bits());
                        let (nz, ny, nx) = (neg(self, z), neg(self, y), neg(self, x));
                        let (f5, f4, f3, f2, f1, f0) = (
                            k(self, 5.0),
                            k(self, 4.0),
                            k(self, 3.0),
                            k(self, 2.0),
                            k(self, 1.0),
                            k(self, 0.0),
                        );
                        let zf = self.s.m.select(c, nz, f5, f4);
                        let yf = self.s.m.select(c, ny, f3, f2);
                        let xf = self.s.m.select(c, nx, f1, f0);
                        pick(self, zf, yf, xf)
                    }
                    "v_cubesc_f32" => {
                        let (nz, nx) = (neg(self, z), neg(self, x));
                        let (mx, mz) = (minus(self, x), minus(self, z));
                        let zf = self.s.m.select(c, nz, mx, x);
                        let xf = self.s.m.select(c, nx, z, mz);
                        pick(self, zf, x, xf)
                    }
                    "v_cubetc_f32" => {
                        let ny = neg(self, y);
                        let (my, mz) = (minus(self, y), minus(self, z));
                        let yf = self.s.m.select(c, ny, mz, z);
                        pick(self, my, yf, my)
                    }
                    _ => {
                        let t = pick(self, z, y, x);
                        let two = self.s.m.f32(2.0f32.to_bits());
                        self.s.m.bin(c, Bin::Mul, t, two)
                    }
                };
                float(self, r)
            }
            "v_movrels_b32" | "v_movreld_b32" | "v_movrelsd_b32" => {
                // M0 indexes the source (movrels), the destination (movreld)
                // or both, from the encoded register up to the program's
                // VGPR count (the registers the instruction reads or writes).
                let m0 = self.i32_of(inst.read(Reg::S(crate::ir::M0)).ok_or("M0")?)?;
                let (
                    Some(&Operand::Vector { reg: sreg, .. }),
                    Some(Operand::Vector { reg: dreg, .. }),
                ) = (v.src.first(), v.dst)
                else {
                    return Err("an M0-relative move");
                };
                let indexed_src = name != "v_movreld_b32";
                let value = if indexed_src {
                    let mut r = self.s.m.i32(0);
                    for k in 0..=255u8.saturating_sub(sreg) {
                        let Some(x) = inst.read(Reg::V(sreg + k)) else {
                            break;
                        };
                        let x = self.i32_of(x)?;
                        let kk = self.s.m.i32(u32::from(k));
                        let is = self.s.m.cmp(c, pred::EQ, m0, kk);
                        r = self.s.m.select(c, is, x, r);
                    }
                    r
                } else {
                    s(self, 0)?
                };
                if name == "v_movrels_b32" {
                    return int(self, value);
                }
                for k in 0..=255u8.saturating_sub(dreg) {
                    let Some(_) = inst.write(Reg::V(dreg + k)) else {
                        break;
                    };
                    let old = match inst.read(Reg::V(dreg + k)) {
                        Some(o) => self.i32_of(o)?,
                        None => self.s.m.i32(0),
                    };
                    let kk = self.s.m.i32(u32::from(k));
                    let is = self.s.m.cmp(c, pred::EQ, m0, kk);
                    let x = self.s.m.select(c, is, value, old);
                    self.write_placed(inst, Reg::V(dreg + k), x, guard);
                }
                Ok(())
            }
            "v_swap_b32" => {
                let (
                    Some(&Operand::Vector { reg: sreg, .. }),
                    Some(Operand::Vector { reg: dreg, .. }),
                ) = (v.src.first(), v.dst)
                else {
                    return Err("v_swap_b32");
                };
                let a = self.i32_of(inst.read(Reg::V(dreg)).ok_or("v_swap_b32")?)?;
                let b = self.i32_of(inst.read(Reg::V(sreg)).ok_or("v_swap_b32")?)?;
                self.write_placed(inst, Reg::V(dreg), b, guard);
                if dreg != sreg {
                    self.write_placed(inst, Reg::V(sreg), a, guard);
                }
                Ok(())
            }
            "v_ffbh_u32" | "v_ffbh_i32" => {
                // Counted from the top bit, -1 when none (DXIL's too).
                let a = s(self, 0)?;
                let op = if name == "v_ffbh_u32" { 33 } else { 34 };
                let r = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, op, &[a]);
                int(self, r)
            }
            "v_dot4c_i32_i8" => {
                // D += the dot product of four signed bytes, wrapping: DXIL's
                // Dot4AddI8Packed.
                let (a, bb) = (s(self, 0)?, s(self, 1)?);
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("an accumulator");
                };
                let acc = self.i32_of(inst.read(Reg::V(reg)).ok_or("an accumulator")?)?;
                let i32_ty = self.s.t.i32;
                let r = self.s.op(
                    c,
                    "dot4AddPacked.i32",
                    Attr::ReadNone,
                    i32_ty,
                    163,
                    &[acc, a, bb],
                );
                int(self, r)
            }
            "v_dot4_i32_i8" => {
                // VOP3P: op_sel picks the half each multiplicand's low half
                // reads, op_sel_hi its high half's; the accumulator is read
                // whole (as AMD's compilers fold constants into it).
                if v.clamp || v.neg_hi != 0 {
                    return Err("a saturating or negated dot product");
                }
                let mut xs = Vec::new();
                for k in 0..2 {
                    let x = s(self, k)?;
                    let (lo, hi) = (v.opsel >> k & 1 != 0, v.opsel_hi >> k & 1 != 0);
                    if (lo, hi) == (false, true) {
                        xs.push(x);
                        continue;
                    }
                    let (k16, low) = (self.s.m.i32(16), self.s.m.i32(0xFFFF));
                    let top = self.s.m.bin(c, Bin::LShr, x, k16);
                    let bottom = self.s.m.bin(c, Bin::And, x, low);
                    let hi = self
                        .s
                        .m
                        .bin(c, Bin::Shl, if hi { top } else { bottom }, k16);
                    xs.push(self.s.m.bin(c, Bin::Or, if lo { top } else { bottom }, hi));
                }
                let acc = s(self, 2)?;
                let i32_ty = self.s.t.i32;
                let r = self.s.op(
                    c,
                    "dot4AddPacked.i32",
                    Attr::ReadNone,
                    i32_ty,
                    163,
                    &[acc, xs[0], xs[1]],
                );
                int(self, r)
            }
            "v_mov_b32" => {
                if v.abs != 0 || v.neg != 0 || v.clamp || v.omod != 0 {
                    let a = f(self, 0)?;
                    return float(self, a);
                }
                let a = s(self, 0)?;
                int(self, a)
            }
            "v_add_f32" | "v_sub_f32" | "v_subrev_f32" | "v_mul_f32" | "v_mul_legacy_f32"
            | "v_max_f32" | "v_min_f32" => {
                let (a, bb) = (f(self, 0)?, f(self, 1)?);
                let r = match name {
                    "v_add_f32" => self.s.m.bin(c, Bin::Add, a, bb),
                    "v_sub_f32" => self.s.m.bin(c, Bin::Sub, a, bb),
                    "v_subrev_f32" => self.s.m.bin(c, Bin::Sub, bb, a),
                    "v_mul_f32" => self.s.m.bin(c, Bin::Mul, a, bb),
                    "v_mul_legacy_f32" => self.legacy_mul(a, bb),
                    "v_max_f32" => self.max_min(true, a, bb),
                    _ => self.max_min(false, a, bb),
                };
                float(self, r)
            }
            "v_fma_f32" | "v_fmaak_f32" | "v_fmamk_f32" => {
                // shortcut: DXIL has no fused f32 multiply-add; FMad lets the driver fuse.
                let (a, bb, cc) = (f(self, 0)?, f(self, 1)?, f(self, 2)?);
                let r = self.fmad(a, bb, cc);
                float(self, r)
            }
            "v_fmac_f32" | "v_mac_f32" | "v_mac_legacy_f32" => {
                let (a, bb) = (f(self, 0)?, f(self, 1)?);
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("an accumulator");
                };
                let acc = inst.read(Reg::V(reg)).ok_or("an accumulator")?;
                let acc = self.float_of(acc)?;
                let r = match name {
                    "v_fmac_f32" => self.fmad(a, bb, acc),
                    "v_mac_f32" => {
                        let m = self.s.m.bin(c, Bin::Mul, a, bb);
                        self.s.m.bin(c, Bin::Add, m, acc)
                    }
                    _ => {
                        let m = self.legacy_mul(a, bb);
                        self.s.m.bin(c, Bin::Add, m, acc)
                    }
                };
                float(self, r)
            }
            "v_mad_f32" | "v_madak_f32" | "v_madmk_f32" | "v_mad_legacy_f32" => {
                let (a, bb, cc) = (f(self, 0)?, f(self, 1)?, f(self, 2)?);
                let m = if name == "v_mad_legacy_f32" {
                    self.legacy_mul(a, bb)
                } else {
                    self.s.m.bin(c, Bin::Mul, a, bb)
                };
                let r = self.s.m.bin(c, Bin::Add, m, cc);
                float(self, r)
            }
            "v_max3_f32" | "v_min3_f32" | "v_med3_f32" => {
                let (a, bb, cc) = (f(self, 0)?, f(self, 1)?, f(self, 2)?);
                let r = match name {
                    "v_max3_f32" => {
                        let t = self.max_min(true, a, bb);
                        self.max_min(true, t, cc)
                    }
                    "v_min3_f32" => {
                        let t = self.max_min(false, a, bb);
                        self.max_min(false, t, cc)
                    }
                    _ => self.med3(a, bb, cc),
                };
                float(self, r)
            }
            "v_rcp_f32" | "v_rcp_iflag_f32" => {
                // shortcut: the hardware reciprocal is approximate (1 ULP); this divides.
                let a = f(self, 0)?;
                let one = self.s.m.f32(1.0f32.to_bits());
                let r = self.s.m.bin(c, Bin::SDiv, one, a);
                float(self, r)
            }
            "v_rsq_f32" | "v_sqrt_f32" | "v_log_f32" | "v_exp_f32" | "v_fract_f32"
            | "v_trunc_f32" | "v_floor_f32" | "v_ceil_f32" | "v_rndne_f32" | "v_sin_f32"
            | "v_cos_f32" => {
                let a = f(self, 0)?;
                let op = match name {
                    "v_rsq_f32" => 25,
                    "v_sqrt_f32" => 24,
                    "v_log_f32" => 23,
                    "v_exp_f32" => 21,
                    "v_fract_f32" => 22,
                    "v_trunc_f32" => 29,
                    "v_floor_f32" => 27,
                    "v_ceil_f32" => 28,
                    "v_rndne_f32" => 26,
                    "v_sin_f32" => 13,
                    _ => 12,
                };
                // These rounding instructions read f32 denormals as signed zero.
                let a = if matches!(op, 22 | 26..=29) {
                    self.ftz(a)
                } else {
                    a
                };
                let r = match op {
                    12 | 13 => self.trig_turns(a, op == 12),
                    22 => self.fract(a, 1.0, f32::from_bits(0x3F7F_FFFF)),
                    _ => self.unary(op, a),
                };
                float(self, r)
            }
            "v_cvt_i32_f32" | "v_cvt_u32_f32" | "v_cvt_flr_i32_f32" | "v_cvt_rpi_i32_f32" => {
                let mut a = f(self, 0)?;
                let floor = name != "v_cvt_i32_f32" && name != "v_cvt_u32_f32";
                let x = a;
                if floor {
                    // floor(x), or floor(x + 0.5) with the half added exactly
                    // (x - floor(x) is exact in f32), of x with denormals read
                    // as zeros (measured on RDNA2 by the Leonida project).
                    let flushed = self.ftz(a);
                    let fl = self.unary(27, flushed);
                    a = fl;
                    if name == "v_cvt_rpi_i32_f32" {
                        let frac = self.s.m.bin(c, Bin::Sub, flushed, fl);
                        let h = self.s.m.f32(0.5f32.to_bits());
                        let up = self.s.m.cmp(c, pred::OGE, frac, h);
                        let one = self.s.m.f32(1.0f32.to_bits());
                        let next = self.s.m.bin(c, Bin::Add, fl, one);
                        a = self.s.m.select(c, up, next, fl);
                    }
                }
                // D3D's conversions saturate and give 0 for NaN, as the
                // hardware does (checked on the RX 6700 XT and WARP).
                let op = if name == "v_cvt_u32_f32" {
                    Cast::FpToUi
                } else {
                    Cast::FpToSi
                };
                let mut r = self.s.m.cast(c, op, a, self.s.t.i32);
                if floor {
                    // These saturate a NaN by its sign (measured on RDNA2).
                    let xi = self.as_int(x);
                    let zero = self.s.m.i32(0);
                    let neg = self.s.m.cmp(c, pred::SLT, xi, zero);
                    let (lo, hi) = (self.s.m.i32(0x8000_0000), self.s.m.i32(0x7FFF_FFFF));
                    let sat = self.s.m.select(c, neg, lo, hi);
                    let nan = self.s.m.cmp(c, pred::UNO, x, x);
                    r = self.s.m.select(c, nan, sat, r);
                }
                int(self, r)
            }
            "v_permlane16_b32" | "v_permlanex16_b32" => {
                // Each lane reads src0 of a lane of its own 32-lane half,
                // picked by nibble (lane & 15) of {src2, src1}: within its
                // own row of 16 (`v_permlane16`) or the other row of its
                // half (`v_permlanex16`). The pick stays inside a half, so a
                // console wave of 64 run as two host waves of 32 needs no
                // exchange.
                // shortcut: a source lane with EXEC clear is read as the host
                // gives it (the console keeps vdst unless op_sel's fetch
                // inactive bit is set).
                self.cross_lane(false)?;
                let x = s(self, 0)?;
                let (s1, s2) = (s(self, 1)?, s(self, 2)?);
                let i32t = self.s.t.i32;
                let lane = self
                    .s
                    .op(c, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
                let [k15, k8, k7, k2, k16, not31, not15] =
                    [15, 8, 7, 2, 16, !31, !15].map(|x| self.s.m.i32(x));
                let low = self.s.m.bin(c, Bin::And, lane, k15);
                // nibble `low` of the 64-bit selector: src1 holds lanes 0-7
                let hi_word = self.s.m.cmp(c, pred::IUGE, low, k8);
                let word = self.s.m.select(c, hi_word, s2, s1);
                let n = self.s.m.bin(c, Bin::And, low, k7);
                let sh = self.s.m.bin(c, Bin::Shl, n, k2);
                let pick = self.s.m.bin(c, Bin::LShr, word, sh);
                let pick = self.s.m.bin(c, Bin::And, pick, k15);
                let row = if name == "v_permlanex16_b32" {
                    // the other row of this half
                    let half = self.s.m.bin(c, Bin::And, lane, not31);
                    let other = self.s.m.bin(c, Bin::And, lane, k16);
                    let other = self.s.m.bin(c, Bin::Xor, other, k16);
                    self.s.m.bin(c, Bin::Or, half, other)
                } else {
                    self.s.m.bin(c, Bin::And, lane, not15)
                };
                let from = self.s.m.bin(c, Bin::Or, row, pick);
                let r = self.read_lane(x, from);
                int(self, r)
            }
            "v_cvt_f32_i32" | "v_cvt_f32_u32" | "v_cvt_f32_ubyte0" | "v_cvt_f32_ubyte1"
            | "v_cvt_f32_ubyte2" | "v_cvt_f32_ubyte3" => {
                let mut a = s(self, 0)?;
                if let Some(n) = name.strip_prefix("v_cvt_f32_ubyte") {
                    let sh = self.s.m.i32(8 * u32::from(n.as_bytes()[0] - b'0'));
                    let ff = self.s.m.i32(0xff);
                    a = self.s.m.bin(c, Bin::LShr, a, sh);
                    a = self.s.m.bin(c, Bin::And, a, ff);
                }
                let op = if name == "v_cvt_f32_i32" {
                    Cast::SiToFp
                } else {
                    Cast::UiToFp
                };
                let r = self.s.m.cast(c, op, a, self.s.t.f32);
                float(self, r)
            }
            "v_cvt_off_f32_i4" => {
                // The low nibble as a signed 4-bit integer, over 16.
                let a = s(self, 0)?;
                let k28 = self.s.m.i32(28);
                let a = self.s.m.bin(c, Bin::Shl, a, k28);
                let a = self.s.m.bin(c, Bin::AShr, a, k28);
                let f = self.s.m.cast(c, Cast::SiToFp, a, self.s.t.f32);
                let k = self.s.m.f32(0.0625f32.to_bits());
                let r = self.s.m.bin(c, Bin::Mul, f, k);
                float(self, r)
            }
            "v_cvt_f32_f64" => {
                let (lo, hi) = self.f64_pair(inst, 0)?;
                let bits = self.f64_to_f32_bits(lo, hi);
                let r = self.as_float(bits);
                float(self, r)
            }
            "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                // Each source is f32, or with op_sel_hi the low (op_sel: high)
                // f16 half; neg_hi is the absolute value here.
                let mut xs = Vec::new();
                for i in 0..3 {
                    let op = v.src.get(i).ok_or("a missing operand")?;
                    let low = v.opsel_hi >> i & 1 != 0 && v.opsel >> i & 1 == 0;
                    let x = match self.low16_src(inst, i).filter(|_| low) {
                        Some(x) => x,
                        None => self.operand(inst, op)?,
                    };
                    let mut x = if v.opsel_hi >> i & 1 != 0 {
                        let h = if v.opsel >> i & 1 != 0 {
                            let k16 = self.s.m.i32(16);
                            self.s.m.bin(c, Bin::LShr, x, k16)
                        } else {
                            x
                        };
                        self.s
                            .op(c, "legacyF16ToF32", Attr::ReadNone, self.s.t.f32, 131, &[h])
                    } else {
                        self.as_float(x)
                    };
                    if v.neg_hi >> i & 1 != 0 {
                        x = self.unary(6, x);
                    }
                    if v.neg >> i & 1 != 0 {
                        let z = self.s.m.f32(0x8000_0000);
                        x = self.s.m.bin(c, Bin::Sub, z, x);
                    }
                    xs.push(x);
                }
                let mut r = self.fmad(xs[0], xs[1], xs[2]);
                if name == "v_fma_mix_f32" {
                    return float(self, r);
                }
                if v.clamp {
                    r = self.unary(7, r);
                }
                // The f16 result goes to one half; the other half stays.
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("a mixed-precision result");
                };
                // A high half kept from a value with no DXIL form (a first
                // interpolation half) stays unknown: only the low half is.
                let mut unknown = None;
                let old = match inst.read(Reg::V(reg)) {
                    Some(o) => match self.i32_of(o) {
                        Ok(x) => x,
                        Err(w) if name == "v_fma_mixlo_f16" && guard.is_none() => {
                            unknown = Some(w);
                            self.s.m.i32(0)
                        }
                        Err(w) => return Err(w),
                    },
                    None => self.s.m.i32(0),
                };
                let h = self.narrow16(r);
                let (keep, h) = if name == "v_fma_mixhi_f16" {
                    let k16 = self.s.m.i32(16);
                    (0xFFFF, self.s.m.bin(c, Bin::Shl, h, k16))
                } else {
                    (0xFFFF_0000, h)
                };
                let keep = self.s.m.i32(keep);
                let old = self.s.m.bin(c, Bin::And, old, keep);
                let x = self.s.m.bin(c, Bin::Or, old, h);
                self.write(inst, Reg::V(reg), x, guard);
                if let (Some(w), Some(new)) = (unknown, inst.write(Reg::V(reg))) {
                    self.vals.insert(new, Val::Missing(w));
                    self.low16.insert(new, x);
                }
                Ok(())
            }
            "v_cvt_pkrtz_f16_f32" => {
                let (a, bb) = (f(self, 0)?, f(self, 1)?);
                let lo = self.f16_toward_zero(a);
                let hi = self.f16_toward_zero(bb);
                let sh = self.s.m.i32(16);
                let hi = self.s.m.bin(c, Bin::Shl, hi, sh);
                let r = self.s.m.bin(c, Bin::Or, lo, hi);
                int(self, r)
            }
            "v_cvt_f16_f32" if v.opsel == 0 => {
                // Rounded as MODE says (nearest even by default) into the low half.
                let a = f(self, 0)?;
                let r = self.narrow16(a);
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("a 16-bit result outside a VGPR");
                };
                let r = self.low_half(inst, reg, r);
                int(self, r)
            }
            "v_pack_b32_f16" => {
                // Two f16 sources (op_sel: their high halves; abs and neg on
                // the sign bit) into the low and high half.
                let mut halves = Vec::new();
                for k in 0..2 {
                    let op = v.src.get(k).ok_or("a missing operand")?;
                    let mut x = self.operand(inst, op)?;
                    if v.opsel >> k & 1 != 0 {
                        let k16 = self.s.m.i32(16);
                        x = self.s.m.bin(c, Bin::LShr, x, k16);
                    }
                    let mask = if v.abs >> k & 1 != 0 { 0x7FFF } else { 0xFFFF };
                    let mask = self.s.m.i32(mask);
                    x = self.s.m.bin(c, Bin::And, x, mask);
                    if v.neg >> k & 1 != 0 {
                        let sign = self.s.m.i32(0x8000);
                        x = self.s.m.bin(c, Bin::Xor, x, sign);
                    }
                    halves.push(x);
                }
                let k16 = self.s.m.i32(16);
                let hi = self.s.m.bin(c, Bin::Shl, halves[1], k16);
                let r = self.s.m.bin(c, Bin::Or, halves[0], hi);
                int(self, r)
            }
            "v_cvt_f32_f16" if v.opsel == 0 => {
                let a = s(self, 0)?;
                let r = self
                    .s
                    .op(c, "legacyF16ToF32", Attr::ReadNone, self.s.t.f32, 131, &[a]);
                float(self, r)
            }
            "v_cndmask_b32" => {
                let cond = match v.src.get(2) {
                    Some(&Operand::Scalar { code, .. }) => {
                        let m = inst
                            .read(Reg::S(code))
                            .and_then(|m| {
                                self.x
                                    .st
                                    .masks
                                    .get(&m)
                                    .or_else(|| self.x.st.other.get(&m))
                                    .copied()
                            })
                            .ok_or("a select on an untracked mask")?;
                        self.boolean(m, c)?
                    }
                    Some(&Operand::Const(k)) => {
                        let x =
                            crate::resource::inline(k).ok_or("a select on an untracked mask")?;
                        if x == 0 {
                            self.s.m.bool(false)
                        } else if x == u32::MAX {
                            self.s.m.bool(true)
                        } else if self.console_lanes {
                            let lo = self.s.m.i32(x);
                            let hi = self.s.m.i32(if (x as i32) < 0 { u32::MAX } else { 0 });
                            self.lane_bit(c, lo, Some(hi))
                        } else {
                            return Err("a constant lane mask");
                        }
                    }
                    _ => return Err("a select on an untracked mask"),
                };
                if v.abs != 0 || v.neg != 0 {
                    // Sign-bit moves on the bits a select passes through
                    // (a host `fabs`/`fneg` may flush a denormal).
                    let mut xs = Vec::new();
                    for k in 0..2 {
                        let mut x = match self.src0 {
                            Some(x) if k == 0 => x,
                            _ => {
                                let op = *v.src.get(k).ok_or("a missing operand")?;
                                let x = self.operand(inst, &op)?;
                                match Self::sdwa_src(inst, k) {
                                    Some((sel, sext)) => self.select_bits(x, sel, sext),
                                    None => x,
                                }
                            }
                        };
                        if v.abs >> k & 1 != 0 {
                            let m = self.s.m.i32(0x7FFF_FFFF);
                            x = self.s.m.bin(c, Bin::And, x, m);
                        }
                        if v.neg >> k & 1 != 0 {
                            let m = self.s.m.i32(0x8000_0000);
                            x = self.s.m.bin(c, Bin::Xor, x, m);
                        }
                        xs.push(x);
                    }
                    let r = self.s.m.select(c, cond, xs[1], xs[0]);
                    return int(self, r);
                }
                let (a, bb) = (s(self, 0)?, s(self, 1)?);
                let r = self.s.m.select(c, cond, bb, a);
                int(self, r)
            }
            "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32" | "v_add_co_ci_u32"
            | "v_sub_co_ci_u32" | "v_subrev_co_ci_u32" => {
                // The carry (borrow) out is a lane mask: this lane's bit.
                let (mut a, mut bb) = (s(self, 0)?, s(self, 1)?);
                if name.contains("subrev") {
                    std::mem::swap(&mut a, &mut bb);
                }
                let cin = if name.ends_with("_ci_u32") {
                    let m = match v.src.get(2) {
                        Some(&Operand::Scalar { code, .. }) => inst.read(Reg::S(code)),
                        _ => None,
                    };
                    let m = m
                        .and_then(|m| self.x.st.masks.get(&m).copied())
                        .ok_or("a carry-in from an untracked mask")?;
                    Some(self.boolean(m, c)?)
                } else {
                    None
                };
                let (r, carry) = self.add_sub_carry(c, name.starts_with("v_add"), a, bb, cin);
                int(self, r)?;
                if let Some(Operand::Scalar { code, .. }) = v.carry
                    && let Some(w) = inst.write(Reg::S(code))
                {
                    self.lane_bits.insert(w, carry);
                }
                for &(_, w) in &inst.writes {
                    self.vals
                        .entry(w)
                        .or_insert(Val::Missing("a lane mask read as data"));
                }
                Ok(())
            }
            // An SGPR spilled to a VGPR lane and read back is the SGPR, in
            // every stage (in compute the VGPR's other lanes may hold what has
            // no host value, a descriptor's address say).
            "v_readlane_b32"
                if inst
                    .writes
                    .first()
                    .is_some_and(|w| self.x.st.restored.contains_key(&w.1)) =>
            {
                let a = self.i32_of(self.x.st.restored[&inst.writes[0].1])?;
                int(self, a)
            }
            "v_writelane_b32"
                if !self.console_lanes
                    && v.src
                        .get(1)
                        .and_then(|&l| crate::resource::lane_of(l))
                        .is_some() =>
            {
                Err("a VGPR of spilled SGPRs read per lane")
            }
            "v_readlane_b32" => {
                let (a, lane) = (s(self, 0)?, s(self, 1)?);
                // A computed lane in a pixel or vertex program is a host lane
                // (from a mask or `v_mbcnt`); a constant one is the console's.
                if self.mask_data == MaskData::Host && !matches!(lane, V::Const(_)) {
                    self.s.flags |= flags::WAVE_OPS;
                } else {
                    self.cross_lane(false)?;
                }
                if let Some(slot) = self.exchange.as_ref().map(|e| e.0) {
                    if self.local && self.local_any {
                        // this host wave's lane of that number (lanes 32..63 are 0..31 here)
                        let k31 = self.s.m.i32(31);
                        let here = self.s.m.bin(self.cur, Bin::And, lane, k31);
                        let r = self.read_lane(a, here);
                        return int(self, r);
                    }
                    if self.local {
                        return Err("a lane read in a half-wave");
                    }
                    if self.exchange.as_ref().is_some_and(|e| e.1) {
                        // a group of 32 or fewer: the lane is in this host wave
                        let r = self.read_lane(a, lane);
                        return int(self, r);
                    }
                    // the lane that holds it may be in the other host wave: it gives it through LDS
                    if !self.all_lanes(self.cur_ir) {
                        need(self.cur_ir);
                        return Err("a lane read where not every lane runs");
                    }
                    let cl = self.console_lane();
                    let is = self.s.m.cmp(self.cur, pred::EQ, cl, lane);
                    let k = self.exchange_pair(slot);
                    let at = self.begin_guarded(Some(is));
                    let lds = self.s.lds();
                    let ptr = self.s.m.element(self.cur, lds, k);
                    self.s.m.store(self.cur, ptr, a);
                    self.end_guarded(at, &[]);
                    let c2 = self.cur;
                    self.group_sync(c2);
                    let ptr = self.s.m.element(c2, lds, k);
                    let r = self.s.m.load(c2, ptr);
                    self.group_sync(c2);
                    return int(self, r);
                }
                let r = self.read_lane(a, lane);
                int(self, r)
            }
            "v_writelane_b32" => {
                // The lane given takes the scalar; the others keep their value.
                let (a, lane) = (s(self, 0)?, s(self, 1)?);
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("a lane write");
                };
                let old = match inst.read(Reg::V(reg)) {
                    Some(o) => self.i32_of(o)?,
                    None => self.s.m.i32(0),
                };
                self.cross_lane(false)?;
                if self.local && !self.local_any {
                    return Err("a lane write in a half-wave");
                }
                let me = self.console_lane();
                let lane = if self.twin.is_some() {
                    let k63 = self.s.m.i32(63);
                    self.s.m.bin(c, Bin::And, lane, k63)
                } else {
                    lane
                };
                let is = self.s.m.cmp(c, pred::EQ, me, lane);
                let r = self.s.m.select(c, is, a, old);
                int(self, r)
            }
            "v_readfirstlane_b32"
                if inst
                    .writes
                    .first()
                    .is_some_and(|w| self.x.res.waterfall.reads.contains(&w.1)) =>
            {
                // A waterfall loop's read of a descriptor dword: the lane's own.
                let a = s(self, 0)?;
                int(self, a)
            }
            "v_readfirstlane_b32"
                if matches!(v.src.first(), Some(&Operand::Vector { reg, .. })
                    if inst.read(Reg::V(reg)).is_some_and(|x| self.own_bits.alike.contains(&x))) =>
            {
                // The bits a loop over a wave OR has still to visit (`own_bits`): every lane of
                // the host wave holds the same, so the first lane's is the lane's own.
                let a = s(self, 0)?;
                int(self, a)
            }
            "v_readfirstlane_b32" => {
                let a = s(self, 0)?;
                let (mine, any) = self.first_lane(a, guard);
                // two host waves that exchange: the console's first lane may be in the other
                // one, unless every lane holds the same value
                let alike = match v.src.first() {
                    Some(&Operand::Vector { reg, .. }) => inst
                        .read(Reg::V(reg))
                        .is_some_and(|x| self.uniform.contains(&x)),
                    _ => true,
                };
                let halves = self.exchange.as_ref().is_some_and(|e| !e.1) && !self.local;
                let r = if halves && !alike {
                    if !self.all_lanes(self.cur_ir) {
                        // shortcut: `VICTORY_FIRSTLANE_OWN=1` takes the host wave's own
                        // first lane, exact when the value is the same in every lane of
                        // the console wave (the ray-tracing builder's scans read a sum of
                        // scalars this way, inside a loop whose exit is per console wave).
                        static OWN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                        if *OWN.get_or_init(|| {
                            std::env::var_os("VICTORY_FIRSTLANE_OWN").is_some_and(|v| v == "1")
                        }) {
                            return int(self, mine);
                        }
                        need(self.cur_ir);
                        return Err("a first-lane read where not every lane runs");
                    }
                    let c = self.cur;
                    if guard.is_none() && super::shader_opt() & super::bitcode::opt::FIRST != 0 {
                        // Every lane is in EXEC: the console's first lane is
                        // lane 0, which the lower half of every console wave
                        // has. No half needs to ask the other whether it has
                        // a lane (an exchange of the constant 1: two group
                        // barriers), and the lower half's value is the answer.
                        let (lo, _) = self.exchange_halves(c, mine);
                        lo
                    } else {
                        let i32t = self.s.t.i32;
                        let has = self.s.m.cast(c, Cast::ZExt, any, i32t);
                        let (lo_has, hi_has) = self.exchange_halves(c, has);
                        let (lo, hi) = self.exchange_halves(c, mine);
                        let z = self.s.m.i32(0);
                        let in_lo = self.s.m.cmp(c, pred::NE, lo_has, z);
                        let in_hi = self.s.m.cmp(c, pred::NE, hi_has, z);
                        // neither half has a lane in EXEC: the console reads lane 0
                        let hi = self.s.m.select(c, in_hi, hi, lo);
                        self.s.m.select(c, in_lo, lo, hi)
                    }
                } else {
                    mine
                };
                let r = self.helper_reads_own(a, r);
                int(self, r)
            }
            _ => {
                let r = self.integer(inst, name)?;
                int(self, r)
            }
        }
    }

    /// A 64-bit source (a register pair; inline constants sign-extend).
    fn src64(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        if v.abs >> k & 1 != 0 || v.neg >> k & 1 != 0 || v.sdwa.is_some() || v.dpp.is_some() {
            return Err("64-bit operand modifiers");
        }
        let c = self.cur;
        let i64t = self.s.m.int(64);
        self.s.flags |= flags::INT64;
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let pair = |l: &mut Self, r: Reg, r1: Reg| -> Result<V, Why> {
            let lo = l.i32_of(inst.read(r).ok_or("an unread operand")?)?;
            let hi = l.i32_of(inst.read(r1).ok_or("an unread operand")?)?;
            Ok(l.join64(lo, hi))
        };
        match op {
            Operand::Vector { reg, .. } => pair(self, Reg::V(reg), Reg::V(reg + 1)),
            Operand::Scalar { code, .. } if code != NULL => {
                pair(self, Reg::S(code), Reg::S(code + 1))
            }
            Operand::Const(k @ 128..=208) => {
                let x = crate::resource::inline(k).ok_or("a special operand")?;
                Ok(self.s.m.int_const(i64t, i64::from(x as i32)))
            }
            // A signed 64-bit operand sign-extends its 32-bit literal, as
            // the hardware does (LLVM's AMDGPU operand rules).
            Operand::Literal(x) if inst.name().ends_with("_i64") => {
                Ok(self.s.m.int_const(i64t, i64::from(x as i32)))
            }
            _ => {
                // Literals (of unsigned and bit operands) and the null
                // register: zero-extended.
                let x = self.operand(inst, &op)?;
                Ok(self.s.m.cast(c, Cast::ZExt, x, i64t))
            }
        }
    }

    /// The low and high dwords of an i64.
    fn split64(&mut self, x: V) -> (V, V) {
        let c = self.cur;
        let i64t = self.s.m.int(64);
        let k32 = self.s.m.int_const(i64t, 32);
        let lo = self.s.m.cast(c, Cast::Trunc, x, self.s.t.i32);
        let hi = self.s.m.bin(c, Bin::LShr, x, k32);
        let hi = self.s.m.cast(c, Cast::Trunc, hi, self.s.t.i32);
        (lo, hi)
    }

    /// Source `k` of a 16-bit operation in the low 16 bits (the rest 0):
    /// op_sel (or an SDWA word select) picks the high half; inline float
    /// constants are f16 values here.
    fn half_src(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let mut high = v.opsel >> k & 1 != 0;
        if let Some(sd) = v.sdwa {
            match sd.src_sel.get(k).copied().unwrap_or(6) {
                4 => {}
                5 => high = true,
                6 => {}
                _ => return Err("a byte select on a 16-bit operation"),
            }
        }
        let x = match self.src0 {
            Some(x) if k == 0 => x,
            _ => match self.low16_src(inst, k).filter(|_| !high) {
                Some(x) => x,
                None => self.f16_operand(inst, &op)?,
            },
        };
        let c = self.cur;
        let x = if high {
            let k16 = self.s.m.i32(16);
            self.s.m.bin(c, Bin::LShr, x, k16)
        } else {
            x
        };
        let m = self.s.m.i32(0xFFFF);
        Ok(self.s.m.bin(c, Bin::And, x, m))
    }

    /// An f16 source as an f32 (exact), with its abs/neg modifiers.
    fn half_fsrc(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let h = self.half_src(inst, k)?;
        let f32t = self.s.t.f32;
        let mut f = self
            .s
            .op(self.cur, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
        if v.abs >> k & 1 != 0 {
            f = self.unary(6, f);
        }
        if v.neg >> k & 1 != 0 {
            let z = self.s.m.f32(0x8000_0000);
            f = self.s.m.bin(self.cur, Bin::Sub, z, f);
        }
        Ok(f)
    }

    /// Writes a 16-bit result (low bits of `x`) into the destination's low
    /// half, or its high half with op_sel bit 3 (or an SDWA WORD_1 select);
    /// the other half is kept (GFX10).
    fn write_half(&mut self, inst: &Inst, x: V, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a 16-bit result outside a VGPR");
        };
        let mut high = v.opsel >> 3 & 1 != 0;
        // SDWA: the word written, the other one kept (preserve) or zero
        // (pad; a sign extension above the high word adds nothing).
        let mut keep = true;
        if let Some(sd) = v.sdwa {
            match (sd.dst_sel, sd.dst_unused) {
                (4, 2) => {}
                (5, 2) => high = true,
                (4 | 6, _) => keep = false,
                (5, _) => (high, keep) = (true, false),
                _ => return Err("an SDWA destination of a 16-bit operation"),
            }
        }
        let c = self.cur;
        let mut unknown = None;
        let old = match inst.read(Reg::V(reg)).filter(|_| keep) {
            Some(o) => match self.i32_of(o) {
                Ok(x) => x,
                Err(_) if high && guard.is_none() && self.low16.contains_key(&o) => self.low16[&o],
                Err(why) if !high && guard.is_none() => {
                    unknown = Some(why);
                    self.s.m.i32(0)
                }
                Err(why) => return Err(why),
            },
            None => self.s.m.i32(0),
        };
        let (k16, low) = (self.s.m.i32(16), self.s.m.i32(0xFFFF));
        let x = self.s.m.bin(c, Bin::And, x, low);
        let r = if high {
            let o = self.s.m.bin(c, Bin::And, old, low);
            let x = self.s.m.bin(c, Bin::Shl, x, k16);
            self.s.m.bin(c, Bin::Or, o, x)
        } else {
            let keep = self.s.m.i32(0xFFFF_0000);
            let o = self.s.m.bin(c, Bin::And, old, keep);
            self.s.m.bin(c, Bin::Or, o, x)
        };
        self.write_placed(inst, Reg::V(reg), r, guard);
        if let (Some(why), Some(new)) = (unknown, inst.write(Reg::V(reg))) {
            self.vals.insert(new, Val::Missing(why));
            self.low16.insert(new, r);
        }
        Ok(())
    }

    /// 16-bit operations, computed in f32 or i32: f16 sums, differences and
    /// products are exact there and round once to f16 (f32 has the 2 x 11 +
    /// 2 bits that make double rounding harmless); `None` if `name` is none.
    fn valu16(&mut self, inst: &Inst, name: &str, guard: Option<V>) -> Option<Result<(), Why>> {
        const FLOAT2: &[&str] = &[
            "v_add_f16",
            "v_sub_f16",
            "v_subrev_f16",
            "v_mul_f16",
            "v_max_f16",
            "v_min_f16",
        ];
        const FLOAT3: &[&str] = &[
            "v_fma_f16",
            "v_mad_f16",
            "v_fmac_f16",
            "v_mac_f16",
            "v_max3_f16",
            "v_min3_f16",
            "v_med3_f16",
        ];
        const FLOAT1: &[&str] = &[
            "v_rcp_f16",
            "v_sqrt_f16",
            "v_rsq_f16",
            "v_log_f16",
            "v_exp_f16",
            "v_fract_f16",
            "v_trunc_f16",
            "v_floor_f16",
            "v_ceil_f16",
            "v_rndne_f16",
            "v_sin_f16",
            "v_cos_f16",
            "v_cvt_f16_u16",
            "v_cvt_f16_i16",
        ];
        const INT: &[&str] = &[
            "v_add_nc_u16",
            "v_sub_nc_u16",
            "v_subrev_nc_u16",
            "v_mul_lo_u16",
            "v_lshlrev_b16",
            "v_lshrrev_b16",
            "v_ashrrev_i16",
            "v_max_u16",
            "v_max_i16",
            "v_min_u16",
            "v_min_i16",
            "v_cvt_u16_f16",
            "v_cvt_i16_f16",
            "v_cvt_norm_i16_f16",
            "v_cvt_norm_u16_f16",
        ];
        let all = [FLOAT1, FLOAT2, FLOAT3, INT];
        if !all.iter().any(|l| l.contains(&name))
            && !matches!(name, "v_cvt_f16_f32" | "v_cvt_f32_f16")
        {
            return None;
        }
        Some(self.valu16_op(inst, name, guard, FLOAT1, FLOAT2, FLOAT3))
    }

    #[allow(clippy::too_many_arguments)]
    fn valu16_op(
        &mut self,
        inst: &Inst,
        name: &str,
        guard: Option<V>,
        float1: &[&str],
        float2: &[&str],
        float3: &[&str],
    ) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let c = self.cur;
        let i32t = self.s.t.i32;
        let f32t = self.s.t.f32;
        if name == "v_cvt_f32_f16" {
            let f = self.half_fsrc(inst, 0)?;
            return self.write_f(inst, f, guard);
        }
        let float_result = |l: &mut Self, mut f: V| -> Result<(), Why> {
            if v.omod != 0 {
                let factor = [1.0f32, 2.0, 4.0, 0.5][usize::from(v.omod)];
                let factor = l.s.m.f32(factor.to_bits());
                f = l.s.m.bin(l.cur, Bin::Mul, f, factor);
            }
            if v.clamp {
                f = l.unary(7, f);
            }
            let h = l.narrow16(f);
            l.write_half(inst, h, guard)
        };
        if name == "v_cvt_f16_f32" {
            let f = self.fsrc(inst, 0)?;
            return float_result(self, f);
        }
        if float2.contains(&name) {
            let (a, b) = (self.half_fsrc(inst, 0)?, self.half_fsrc(inst, 1)?);
            let r = match name {
                "v_add_f16" => self.s.m.bin(c, Bin::Add, a, b),
                "v_sub_f16" => self.s.m.bin(c, Bin::Sub, a, b),
                "v_subrev_f16" => self.s.m.bin(c, Bin::Sub, b, a),
                "v_mul_f16" => self.s.m.bin(c, Bin::Mul, a, b),
                "v_max_f16" => self.max_min(true, a, b),
                _ => self.max_min(false, a, b),
            };
            return float_result(self, r);
        }
        if float3.contains(&name) {
            let (a, b) = (self.half_fsrc(inst, 0)?, self.half_fsrc(inst, 1)?);
            let acc = if matches!(name, "v_fmac_f16" | "v_mac_f16") {
                // The accumulator is the destination's half that is written.
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("an accumulator");
                };
                let x = self.i32_of(inst.read(Reg::V(reg)).ok_or("an accumulator")?)?;
                let x = if v.opsel >> 3 & 1 != 0 {
                    let k16 = self.s.m.i32(16);
                    self.s.m.bin(c, Bin::LShr, x, k16)
                } else {
                    x
                };
                let m = self.s.m.i32(0xFFFF);
                let x = self.s.m.bin(c, Bin::And, x, m);
                self.s
                    .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x])
            } else {
                self.half_fsrc(inst, 2)?
            };
            let r = match name {
                // shortcut: fused f16 multiply-adds as FMad in f32 (the
                // product is exact; the sum rounds twice).
                "v_fma_f16" | "v_fmac_f16" | "v_mad_f16" | "v_mac_f16" => self.fmad(a, b, acc),
                "v_max3_f16" => {
                    let t = self.max_min(true, a, b);
                    self.max_min(true, t, acc)
                }
                "v_min3_f16" => {
                    let t = self.max_min(false, a, b);
                    self.max_min(false, t, acc)
                }
                _ => self.med3(a, b, acc),
            };
            return float_result(self, r);
        }
        if float1.contains(&name) {
            let r = match name {
                "v_cvt_f16_u16" | "v_cvt_f16_i16" => {
                    let mut x = self.half_src(inst, 0)?;
                    let op = if name == "v_cvt_f16_i16" {
                        let k16 = self.s.m.i32(16);
                        let t = self.s.m.bin(c, Bin::Shl, x, k16);
                        x = self.s.m.bin(c, Bin::AShr, t, k16);
                        Cast::SiToFp
                    } else {
                        Cast::UiToFp
                    };
                    self.s.m.cast(c, op, x, f32t)
                }
                "v_rcp_f16" => {
                    let a = self.half_fsrc(inst, 0)?;
                    let one = self.s.m.f32(1.0f32.to_bits());
                    self.s.m.bin(c, Bin::SDiv, one, a)
                }
                _ => {
                    let a = self.half_fsrc(inst, 0)?;
                    let op = match name {
                        "v_sqrt_f16" => 24,
                        "v_rsq_f16" => 25,
                        "v_log_f16" => 23,
                        "v_exp_f16" => 21,
                        "v_fract_f16" => 22,
                        "v_trunc_f16" => 29,
                        "v_floor_f16" => 27,
                        "v_ceil_f16" => 28,
                        "v_rndne_f16" => 26,
                        "v_sin_f16" => 13,
                        _ => 12,
                    };
                    match op {
                        12 | 13 => self.trig_turns(a, op == 12),
                        // Any f16 in (-2^-11, 0) has fract >= 1 - 2^-12; clamp to 0x3bff (1 - 2^-11).
                        22 => self.fract(a, 1.0 - 1.0 / 4096.0, 1.0 - 1.0 / 2048.0),
                        _ => self.unary(op, a),
                    }
                }
            };
            return float_result(self, r);
        }
        if matches!(name, "v_cvt_norm_i16_f16" | "v_cvt_norm_u16_f16") {
            if v.clamp || v.omod != 0 {
                return Err("modifiers on a 16-bit integer operation");
            }
            let r = self.cvt_norm16(inst, name == "v_cvt_norm_i16_f16")?;
            return self.write_half(inst, r, guard);
        }
        // 16-bit integers: computed on the low halves, the low 16 bits kept.
        let float_input = matches!(name, "v_cvt_u16_f16" | "v_cvt_i16_f16");
        if v.clamp || v.omod != 0 || (!float_input && (v.abs != 0 || v.neg != 0)) {
            return Err("modifiers on a 16-bit integer operation");
        }
        let r = match name {
            "v_cvt_u16_f16" | "v_cvt_i16_f16" => {
                // Toward zero, saturating at the 16-bit range (infinities
                // included); NaN gives 0. Clamped in f32 first, where every
                // f16 is exact, so the conversion never leaves i32's range.
                let a = self.half_fsrc(inst, 0)?;
                let (lo, hi) = if name == "v_cvt_u16_f16" {
                    (0.0f32, 65535.0f32)
                } else {
                    (-32768.0, 32767.0)
                };
                let (lo, hi) = (self.s.m.f32(lo.to_bits()), self.s.m.f32(hi.to_bits()));
                let t = self.binary(35, a, lo);
                let t = self.binary(36, t, hi);
                let r = self.s.m.cast(c, Cast::FpToSi, t, i32t);
                let nan = self.s.m.cmp(c, pred::UNO, a, a);
                let z = self.s.m.i32(0);
                self.s.m.select(c, nan, z, r)
            }
            _ => {
                let (mut a, mut b) = (self.half_src(inst, 0)?, self.half_src(inst, 1)?);
                let k16 = self.s.m.i32(16);
                let sext = |l: &mut Self, x: V| {
                    let t = l.s.m.bin(c, Bin::Shl, x, k16);
                    l.s.m.bin(c, Bin::AShr, t, k16)
                };
                if name.ends_with("_i16") {
                    a = sext(self, a);
                    b = sext(self, b);
                }
                let k15 = self.s.m.i32(15);
                match name {
                    "v_add_nc_u16" => self.s.m.bin(c, Bin::Add, a, b),
                    "v_sub_nc_u16" => self.s.m.bin(c, Bin::Sub, a, b),
                    "v_subrev_nc_u16" => self.s.m.bin(c, Bin::Sub, b, a),
                    "v_mul_lo_u16" => self.s.m.bin(c, Bin::Mul, a, b),
                    "v_lshlrev_b16" | "v_lshrrev_b16" | "v_ashrrev_i16" => {
                        // src0 is the shift (4 bits), src1 the value.
                        let sh = self.s.m.bin(c, Bin::And, a, k15);
                        let x = if name == "v_ashrrev_i16" {
                            sext(self, b)
                        } else {
                            b
                        };
                        let op = match name {
                            "v_lshlrev_b16" => Bin::Shl,
                            "v_lshrrev_b16" => Bin::LShr,
                            _ => Bin::AShr,
                        };
                        self.s.m.bin(c, op, x, sh)
                    }
                    _ => {
                        let p = match name {
                            "v_max_u16" => pred::IUGT,
                            "v_min_u16" => pred::IULT,
                            "v_max_i16" => pred::SGT,
                            _ => pred::SLT,
                        };
                        let t = self.s.m.cmp(c, p, a, b);
                        self.s.m.select(c, t, a, b)
                    }
                }
            }
        };
        self.write_half(inst, r, guard)
    }

    /// `v_cvt_norm_i16_f16` / `v_cvt_norm_u16_f16`: the f16 source (with its
    /// abs/neg modifiers) clamped to [-1, 1] or [0, 1], times 32767 or 65535,
    /// rounded to nearest even once from the exact product; NaN gives 0.
    /// Worked on the f16's bits in integers, because in f32 the product
    /// (up to 26 significant bits) would round before the rounding to an
    /// integer.
    fn cvt_norm16(&mut self, inst: &Inst, signed: bool) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let c = self.cur;
        let k = |l: &mut Self, x: u32| l.s.m.i32(x);
        let mut h = self.half_src(inst, 0)?;
        if v.abs & 1 != 0 {
            let m = k(self, 0x7FFF);
            h = self.s.m.bin(c, Bin::And, h, m);
        }
        if v.neg & 1 != 0 {
            let m = k(self, 0x8000);
            h = self.s.m.bin(c, Bin::Xor, h, m);
        }
        let scale = if signed { 0x7FFF } else { 0xFFFF };
        let (k0, k1, k10) = (k(self, 0), k(self, 1), k(self, 10));
        let mag_mask = k(self, 0x7FFF);
        let mag = self.s.m.bin(c, Bin::And, h, mag_mask);
        let k15 = k(self, 15);
        let neg = self.s.m.bin(c, Bin::LShr, h, k15);
        let neg = self.s.m.cmp(c, pred::NE, neg, k0);
        // |x| < 1: x = m * 2^-s with m < 2^11 and 11 <= s <= 24.
        let exp = self.s.m.bin(c, Bin::LShr, mag, k10);
        let man_mask = k(self, 0x3FF);
        let man = self.s.m.bin(c, Bin::And, mag, man_mask);
        let implicit = k(self, 0x400);
        let normal = self.s.m.bin(c, Bin::Or, man, implicit);
        let sub = self.s.m.cmp(c, pred::EQ, exp, k0);
        let m = self.s.m.select(c, sub, man, normal);
        let k25 = k(self, 25);
        let s_norm = self.s.m.bin(c, Bin::Sub, k25, exp);
        let k24 = k(self, 24);
        let sh = self.s.m.select(c, sub, k24, s_norm);
        let ks = k(self, scale);
        let p = self.s.m.bin(c, Bin::Mul, m, ks);
        let q = self.s.m.bin(c, Bin::LShr, p, sh);
        let one_s = self.s.m.bin(c, Bin::Shl, k1, sh);
        let low_mask = self.s.m.bin(c, Bin::Sub, one_s, k1);
        let rem = self.s.m.bin(c, Bin::And, p, low_mask);
        let half = self.s.m.bin(c, Bin::LShr, one_s, k1);
        let above = self.s.m.cmp(c, pred::IUGT, rem, half);
        let tie = self.s.m.cmp(c, pred::EQ, rem, half);
        let odd = self.s.m.bin(c, Bin::And, q, k1);
        let odd = self.s.m.cmp(c, pred::NE, odd, k0);
        let tie_up = self.s.m.bin(c, Bin::And, tie, odd);
        let up = self.s.m.bin(c, Bin::Or, above, tie_up);
        let r = self.s.m.select(c, up, k1, k0);
        let r = self.s.m.bin(c, Bin::Add, q, r);
        // |x| >= 1 (and infinity) saturates; NaN gives 0.
        let one_h = k(self, 0x3C00);
        let big = self.s.m.cmp(c, pred::IUGE, mag, one_h);
        let r = self.s.m.select(c, big, ks, r);
        let inf = k(self, 0x7C00);
        let nan = self.s.m.cmp(c, pred::IUGT, mag, inf);
        let r = self.s.m.select(c, nan, k0, r);
        let r = if signed {
            let n = self.s.m.bin(c, Bin::Sub, k0, r);
            self.s.m.select(c, neg, n, r)
        } else {
            self.s.m.select(c, neg, k0, r)
        };
        let m16 = k(self, 0xFFFF);
        Ok(self.s.m.bin(c, Bin::And, r, m16))
    }

    /// Half `high` of source `k` of a packed (VOP3P) operation, in the low
    /// 16 bits: op_sel picks the half the low result reads, op_sel_hi the
    /// half the high result reads. Packed integer instructions see the f32
    /// bits of an inline float constant.
    fn packed_src(&mut self, inst: &Inst, k: usize, high: bool, float: bool) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let sel = if high { v.opsel_hi } else { v.opsel } >> k & 1 != 0;
        let x = if float {
            self.f16_operand(inst, &op)?
        } else {
            self.operand(inst, &op)?
        };
        let c = self.cur;
        let x = if sel {
            let k16 = self.s.m.i32(16);
            self.s.m.bin(c, Bin::LShr, x, k16)
        } else {
            x
        };
        let m = self.s.m.i32(0xFFFF);
        Ok(self.s.m.bin(c, Bin::And, x, m))
    }

    /// Half `high` of source `k` of `v_pk_fmac_f16`, in the low 16 bits: a
    /// VOP2 operation (no op_sel) whose third source is its destination.
    fn fmac_half(&mut self, inst: &Inst, k: usize, high: bool) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = if k == 2 { v.dst } else { v.src.get(k).copied() };
        let op = op.ok_or("a missing operand")?;
        if matches!(op, Operand::Const(_) | Operand::Literal(_)) {
            // shortcut: no constant operand seen; its high half is not known.
            return Err("a constant operand of v_pk_fmac_f16");
        }
        let x = self.operand(inst, &op)?;
        let c = self.cur;
        let x = if high {
            let k16 = self.s.m.i32(16);
            self.s.m.bin(c, Bin::LShr, x, k16)
        } else {
            x
        };
        let m = self.s.m.i32(0xFFFF);
        Ok(self.s.m.bin(c, Bin::And, x, m))
    }

    /// Packed 16-bit operations (`v_pk_*`): each half computed as the
    /// 16-bit operation does, f16 through f32 as in [`Self::valu16`].
    fn packed16(&mut self, inst: &Inst, op: &str, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let float = op.ends_with("_f16");
        let n = match op {
            "fma_f16" | "fmac_f16" | "mad_u16" | "mad_i16" => 3,
            "add_f16" | "mul_f16" | "min_f16" | "max_f16" | "add_u16" | "sub_u16" | "add_i16"
            | "sub_i16" | "mul_lo_u16" | "lshlrev_b16" | "lshrrev_b16" | "ashrrev_i16"
            | "max_u16" | "min_u16" | "max_i16" | "min_i16" => 2,
            _ => return Err("a packed 16-bit operation"),
        };
        if v.omod != 0 || !float && (v.clamp || v.neg != 0 || v.neg_hi != 0) {
            return Err("modifiers on a packed integer operation");
        }
        let c = self.cur;
        let f32t = self.s.t.f32;
        let mut halves = Vec::new();
        for high in [false, true] {
            let mut xs = Vec::new();
            for k in 0..n {
                let h = if op == "fmac_f16" {
                    self.fmac_half(inst, k, high)?
                } else {
                    self.packed_src(inst, k, high, float)?
                };
                if float {
                    let mut f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
                    let neg = if high { v.neg_hi } else { v.neg };
                    if neg >> k & 1 != 0 {
                        let z = self.s.m.f32(0x8000_0000);
                        f = self.s.m.bin(c, Bin::Sub, z, f);
                    }
                    xs.push(f);
                } else {
                    xs.push(h);
                }
            }
            let r = if float {
                let mut f = match op {
                    "add_f16" => self.s.m.bin(c, Bin::Add, xs[0], xs[1]),
                    "mul_f16" => self.s.m.bin(c, Bin::Mul, xs[0], xs[1]),
                    "min_f16" => self.max_min(false, xs[0], xs[1]),
                    "max_f16" => self.max_min(true, xs[0], xs[1]),
                    // shortcut: FMad in f32, as v_fma_f16.
                    _ => self.fmad(xs[0], xs[1], xs[2]),
                };
                if v.clamp {
                    f = self.unary(7, f);
                }
                self.narrow16(f)
            } else {
                let k16 = self.s.m.i32(16);
                let sext = |l: &mut Self, x: V| {
                    let t = l.s.m.bin(c, Bin::Shl, x, k16);
                    l.s.m.bin(c, Bin::AShr, t, k16)
                };
                if op.ends_with("_i16") {
                    for x in &mut xs {
                        *x = sext(self, *x);
                    }
                }
                let k15 = self.s.m.i32(15);
                match op {
                    "add_u16" | "add_i16" => self.s.m.bin(c, Bin::Add, xs[0], xs[1]),
                    "sub_u16" | "sub_i16" => self.s.m.bin(c, Bin::Sub, xs[0], xs[1]),
                    "mul_lo_u16" => self.s.m.bin(c, Bin::Mul, xs[0], xs[1]),
                    "mad_u16" | "mad_i16" => {
                        let m = self.s.m.bin(c, Bin::Mul, xs[0], xs[1]);
                        self.s.m.bin(c, Bin::Add, m, xs[2])
                    }
                    "lshlrev_b16" | "lshrrev_b16" | "ashrrev_i16" => {
                        let sh = self.s.m.bin(c, Bin::And, xs[0], k15);
                        let o = match op {
                            "lshlrev_b16" => Bin::Shl,
                            "lshrrev_b16" => Bin::LShr,
                            _ => Bin::AShr,
                        };
                        let x = if op == "ashrrev_i16" {
                            sext(self, xs[1])
                        } else {
                            xs[1]
                        };
                        self.s.m.bin(c, o, x, sh)
                    }
                    _ => {
                        let p = match op {
                            "max_u16" => pred::IUGT,
                            "min_u16" => pred::IULT,
                            "max_i16" => pred::SGT,
                            _ => pred::SLT,
                        };
                        let t = self.s.m.cmp(c, p, xs[0], xs[1]);
                        self.s.m.select(c, t, xs[0], xs[1])
                    }
                }
            };
            let m = self.s.m.i32(0xFFFF);
            halves.push(self.s.m.bin(c, Bin::And, r, m));
        }
        let k16 = self.s.m.i32(16);
        let hi = self.s.m.bin(c, Bin::Shl, halves[1], k16);
        let r = self.s.m.bin(c, Bin::Or, halves[0], hi);
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a packed result outside a VGPR");
        };
        self.write(inst, Reg::V(reg), r, guard);
        Ok(())
    }

    fn integer(&mut self, inst: &Inst, name: &'static str) -> Result<V, Why> {
        let c = self.cur;
        let s = |l: &mut Self, k| l.src(inst, k);
        let k = |l: &mut Self, x: u32| l.s.m.i32(x);
        let m31 = |l: &mut Self, x: V| {
            let k31 = l.s.m.i32(31);
            l.s.m.bin(c, Bin::And, x, k31)
        };
        Ok(match name {
            "v_and_b32" | "v_or_b32" | "v_xor_b32" | "v_add_nc_u32" | "v_sub_nc_u32"
            | "v_subrev_nc_u32" | "v_mul_lo_u32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let (op, a, b) = match name {
                    "v_and_b32" => (Bin::And, a, b),
                    "v_or_b32" => (Bin::Or, a, b),
                    "v_xor_b32" => (Bin::Xor, a, b),
                    "v_add_nc_u32" => (Bin::Add, a, b),
                    "v_sub_nc_u32" => (Bin::Sub, a, b),
                    "v_subrev_nc_u32" => (Bin::Sub, b, a),
                    _ => (Bin::Mul, a, b),
                };
                self.s.m.bin(c, op, a, b)
            }
            "v_xnor_b32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let x = self.s.m.bin(c, Bin::Xor, a, b);
                let ones = k(self, u32::MAX);
                self.s.m.bin(c, Bin::Xor, x, ones)
            }
            "v_not_b32" => {
                let a = s(self, 0)?;
                let ones = k(self, u32::MAX);
                self.s.m.bin(c, Bin::Xor, a, ones)
            }
            "v_lshlrev_b32" | "v_lshrrev_b32" | "v_ashrrev_i32" => {
                // A left shift by 16 or more reads the low half alone.
                let Fields::Valu(v) = &inst.fields else {
                    unreachable!()
                };
                let by = v.src.first().and_then(|&o| crate::resource::lane_of(o));
                let low =
                    name == "v_lshlrev_b32" && v.sdwa.is_none() && by.is_some_and(|n| n & 31 >= 16);
                let a = match self.low16_src(inst, 1).filter(|_| low) {
                    Some(x) => x,
                    None => s(self, 1)?,
                };
                let sh = s(self, 0)?;
                let sh = m31(self, sh);
                let op = match name {
                    "v_lshlrev_b32" => Bin::Shl,
                    "v_lshrrev_b32" => Bin::LShr,
                    _ => Bin::AShr,
                };
                self.s.m.bin(c, op, a, sh)
            }
            "v_lshl_add_u32" | "v_lshl_or_b32" | "v_add_lshl_u32" | "v_and_or_b32"
            | "v_or3_b32" | "v_xad_u32" | "v_add3_u32" | "v_xor3_b32" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                match name {
                    "v_lshl_add_u32" | "v_lshl_or_b32" => {
                        let b = m31(self, b);
                        let t = self.s.m.bin(c, Bin::Shl, a, b);
                        let op = if name == "v_lshl_add_u32" {
                            Bin::Add
                        } else {
                            Bin::Or
                        };
                        self.s.m.bin(c, op, t, d)
                    }
                    "v_add_lshl_u32" => {
                        let t = self.s.m.bin(c, Bin::Add, a, b);
                        let d = m31(self, d);
                        self.s.m.bin(c, Bin::Shl, t, d)
                    }
                    _ => {
                        let (o1, o2) = match name {
                            "v_and_or_b32" => (Bin::And, Bin::Or),
                            "v_or3_b32" => (Bin::Or, Bin::Or),
                            "v_xad_u32" => (Bin::Xor, Bin::Add),
                            "v_add3_u32" => (Bin::Add, Bin::Add),
                            _ => (Bin::Xor, Bin::Xor),
                        };
                        let t = self.s.m.bin(c, o1, a, b);
                        self.s.m.bin(c, o2, t, d)
                    }
                }
            }
            "v_mul_u32_u24" | "v_mul_i32_i24" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let (sh, op) = (
                    k(self, 8),
                    if name == "v_mul_i32_i24" {
                        Bin::AShr
                    } else {
                        Bin::LShr
                    },
                );
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                self.s.m.bin(c, Bin::Mul, a, b)
            }
            "v_mul_hi_u32_u24" | "v_mul_hi_i32_i24" => {
                // The high dword of the same 48-bit product: the low 24 bits
                // of each operand, zero- or sign-extended, multiplied.
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let signed = name == "v_mul_hi_i32_i24";
                let (sh, op) = (k(self, 8), if signed { Bin::AShr } else { Bin::LShr });
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                self.mul_hi(a, b, signed)
            }
            "v_bfe_u32" | "v_bfe_i32" => {
                let (a, off, w) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let w = m31(self, w);
                self.bfe(a, off, w, name == "v_bfe_i32")
            }
            "v_bfi_b32" => {
                let (m, a, b) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let ones = k(self, u32::MAX);
                let nm = self.s.m.bin(c, Bin::Xor, m, ones);
                let x = self.s.m.bin(c, Bin::And, m, a);
                let y = self.s.m.bin(c, Bin::And, nm, b);
                self.s.m.bin(c, Bin::Or, x, y)
            }
            "v_max_i32" | "v_min_i32" | "v_max_u32" | "v_min_u32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let p = match name {
                    "v_max_i32" => pred::SGT,
                    "v_min_i32" => pred::SLT,
                    "v_max_u32" => pred::IUGT,
                    _ => pred::IULT,
                };
                let t = self.s.m.cmp(c, p, a, b);
                self.s.m.select(c, t, a, b)
            }
            "v_mul_hi_u32" | "v_mul_hi_i32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                self.mul_hi(a, b, name == "v_mul_hi_i32")
            }
            "v_alignbit_b32" | "v_alignbyte_b32" => {
                // The low dword of {a, b} >> shift.
                let (a, b, sh) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let sh = if name == "v_alignbyte_b32" {
                    let three = k(self, 3);
                    let t = self.s.m.bin(c, Bin::And, sh, three);
                    let k3 = k(self, 3);
                    self.s.m.bin(c, Bin::Shl, t, k3)
                } else {
                    m31(self, sh)
                };
                let k32 = k(self, 32);
                let lo = self.s.m.bin(c, Bin::LShr, b, sh);
                let left = self.s.m.bin(c, Bin::Sub, k32, sh);
                let left = m31(self, left);
                let hi = self.s.m.bin(c, Bin::Shl, a, left);
                let zero = k(self, 0);
                let none = self.s.m.cmp(c, pred::EQ, sh, zero);
                let hi = self.s.m.select(c, none, zero, hi);
                self.s.m.bin(c, Bin::Or, lo, hi)
            }
            "v_bfrev_b32" => {
                let a = s(self, 0)?;
                self.s
                    .op(c, "unary.i32", Attr::ReadNone, self.s.t.i32, 30, &[a])
            }
            "v_bcnt_u32_b32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let n = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 31, &[a]);
                self.s.m.bin(c, Bin::Add, n, b)
            }
            "v_mad_u32_u24" | "v_mad_i32_i24" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let (sh, op) = (
                    k(self, 8),
                    if name == "v_mad_i32_i24" {
                        Bin::AShr
                    } else {
                        Bin::LShr
                    },
                );
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                let m = self.s.m.bin(c, Bin::Mul, a, b);
                self.s.m.bin(c, Bin::Add, m, d)
            }
            "v_sad_u8" | "v_sad_hi_u8" | "v_sad_u16" | "v_sad_u32" | "v_msad_u8" => {
                // src2 plus the sum of the unsigned differences |a - b| of
                // the operands' bytes, halves or whole dwords, wrapping at 32
                // bits. v_sad_hi_u8 adds the sum shifted to the high half;
                // v_msad_u8 leaves out a byte whose mask byte (src1) is 0.
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let (width, parts) = match name {
                    "v_sad_u16" => (16, 2),
                    "v_sad_u32" => (32, 1),
                    _ => (8, 4),
                };
                let zero = k(self, 0);
                let mut sum = zero;
                for i in 0..parts {
                    let (mut x, mut y) = (a, b);
                    if width < 32 {
                        let (sh, mask) = (k(self, width * i), k(self, (1 << width) - 1));
                        x = self.s.m.bin(c, Bin::LShr, x, sh);
                        x = self.s.m.bin(c, Bin::And, x, mask);
                        y = self.s.m.bin(c, Bin::LShr, y, sh);
                        y = self.s.m.bin(c, Bin::And, y, mask);
                    }
                    let above = self.s.m.cmp(c, pred::IUGT, x, y);
                    let hi = self.s.m.select(c, above, x, y);
                    let lo = self.s.m.select(c, above, y, x);
                    let mut diff = self.s.m.bin(c, Bin::Sub, hi, lo);
                    if name == "v_msad_u8" {
                        let masked = self.s.m.cmp(c, pred::NE, y, zero);
                        diff = self.s.m.select(c, masked, diff, zero);
                    }
                    sum = self.s.m.bin(c, Bin::Add, sum, diff);
                }
                if name == "v_sad_hi_u8" {
                    let k16 = k(self, 16);
                    sum = self.s.m.bin(c, Bin::Shl, sum, k16);
                }
                self.s.m.bin(c, Bin::Add, sum, d)
            }
            "v_min3_i32" | "v_max3_i32" | "v_min3_u32" | "v_max3_u32" | "v_med3_i32"
            | "v_med3_u32" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let signed = name.ends_with("i32");
                let (lt, gt) = if signed {
                    (pred::SLT, pred::SGT)
                } else {
                    (pred::IULT, pred::IUGT)
                };
                let pick = |l: &mut Self, p, x, y| {
                    let t = l.s.m.cmp(c, p, x, y);
                    l.s.m.select(c, t, x, y)
                };
                match &name[2..6] {
                    "min3" => {
                        let t = pick(self, lt, a, b);
                        pick(self, lt, t, d)
                    }
                    "max3" => {
                        let t = pick(self, gt, a, b);
                        pick(self, gt, t, d)
                    }
                    _ => {
                        let lo = pick(self, lt, a, b);
                        let hi = pick(self, gt, a, b);
                        let t = pick(self, lt, hi, d);
                        pick(self, gt, lo, t)
                    }
                }
            }
            "v_perm_b32" => {
                // Byte i of the result is byte S of {src0, src1} (src1 the
                // low dword) for selector byte S below 8; 8-11 repeat the
                // sign of bytes 1 and 3 of src1, then of src0; 12 is 0x00,
                // higher 0xFF.
                let Fields::Valu(v) = &inst.fields else {
                    unreachable!()
                };
                let sel = match v.src.get(2) {
                    Some(&Operand::Literal(x)) => x,
                    Some(&Operand::Const(k)) => crate::resource::inline(k).ok_or(name)?,
                    _ => {
                        // A computed selector: each byte picked at run time.
                        let (a, b, sv) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                        let mut r = k(self, 0);
                        for i in 0..4u32 {
                            let (sh, ff) = (k(self, 8 * i), k(self, 0xFF));
                            let sel = self.s.m.bin(c, Bin::LShr, sv, sh);
                            let sel = self.s.m.bin(c, Bin::And, sel, ff);
                            // Select a dword and a byte within it.
                            // Later selects handle codes 8 and above.
                            let four = k(self, 4);
                            let zero = k(self, 0);
                            let high = self.s.m.bin(c, Bin::And, sel, four);
                            let high = self.s.m.cmp(c, pred::NE, high, zero);
                            let word = self.s.m.select(c, high, a, b);
                            let three = k(self, 3);
                            let index = self.s.m.bin(c, Bin::And, sel, three);
                            let bits = self.s.m.bin(c, Bin::Shl, index, three);
                            let x = self.s.m.bin(c, Bin::LShr, word, bits);
                            let mut byte = self.s.m.bin(c, Bin::And, x, ff);
                            // 8-11: the sign of bytes 1, 3 of src1, then of src0.
                            for (n, src, bit) in [(8, b, 15), (9, b, 31), (10, a, 15), (11, a, 31)]
                            {
                                let left = k(self, 31 - bit);
                                let t = self.s.m.bin(c, Bin::Shl, src, left);
                                let k31 = k(self, 31);
                                let t = self.s.m.bin(c, Bin::AShr, t, k31);
                                let t = self.s.m.bin(c, Bin::And, t, ff);
                                let kn = k(self, n);
                                let is = self.s.m.cmp(c, pred::EQ, sel, kn);
                                byte = self.s.m.select(c, is, t, byte);
                            }
                            let (k12, z) = (k(self, 12), k(self, 0));
                            let is12 = self.s.m.cmp(c, pred::EQ, sel, k12);
                            byte = self.s.m.select(c, is12, z, byte);
                            let above = self.s.m.cmp(c, pred::IUGT, sel, k12);
                            byte = self.s.m.select(c, above, ff, byte);
                            let at = k(self, 8 * i);
                            let byte = self.s.m.bin(c, Bin::Shl, byte, at);
                            r = self.s.m.bin(c, Bin::Or, r, byte);
                        }
                        return Ok(r);
                    }
                };
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let mut r = k(self, 0);
                for i in 0..4 {
                    let pick = sel >> (8 * i) & 0xFF;
                    let byte = match pick {
                        0..=7 => {
                            let (src, n) = if pick < 4 { (b, pick) } else { (a, pick - 4) };
                            let sh = k(self, 8 * n);
                            let t = self.s.m.bin(c, Bin::LShr, src, sh);
                            let ff = k(self, 0xFF);
                            self.s.m.bin(c, Bin::And, t, ff)
                        }
                        8..=11 => {
                            let src = if pick < 10 { b } else { a };
                            let bit = if pick % 2 == 0 { 15 } else { 31 };
                            let left = k(self, 31 - bit);
                            let t = self.s.m.bin(c, Bin::Shl, src, left);
                            let k31 = k(self, 31);
                            let t = self.s.m.bin(c, Bin::AShr, t, k31);
                            let ff = k(self, 0xFF);
                            self.s.m.bin(c, Bin::And, t, ff)
                        }
                        12 => continue,
                        _ => k(self, 0xFF),
                    };
                    let at = k(self, 8 * i);
                    let byte = self.s.m.bin(c, Bin::Shl, byte, at);
                    r = self.s.m.bin(c, Bin::Or, r, byte);
                }
                r
            }
            "v_bfm_b32" => {
                // ((1 << src0[4:0]) - 1) << src1[4:0]
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let one = k(self, 1);
                let (a, b) = (m31(self, a), m31(self, b));
                let m = self.s.m.bin(c, Bin::Shl, one, a);
                let m = self.s.m.bin(c, Bin::Sub, m, one);
                self.s.m.bin(c, Bin::Shl, m, b)
            }
            "v_cvt_pk_u8_f32" => {
                // src0 rounded to nearest even and saturated to a byte, into
                // byte src1[1:0] of src2.
                let f = self.fsrc(inst, 0)?;
                let (b, d) = (s(self, 1)?, s(self, 2)?);
                let f = self.unary(26, f);
                let (zero, max) = (self.s.m.f32(0), self.s.m.f32(255.0f32.to_bits()));
                let f = self.binary(35, f, zero);
                let f = self.binary(36, f, max);
                let x = self.s.m.cast(c, Cast::FpToUi, f, self.s.t.i32);
                let (three, k3) = (k(self, 3), k(self, 3));
                let sh = self.s.m.bin(c, Bin::And, b, three);
                let sh = self.s.m.bin(c, Bin::Shl, sh, k3);
                let x = self.s.m.bin(c, Bin::Shl, x, sh);
                let ff = k(self, 0xFF);
                let hole = self.s.m.bin(c, Bin::Shl, ff, sh);
                let ones = k(self, u32::MAX);
                let hole = self.s.m.bin(c, Bin::Xor, hole, ones);
                let d = self.s.m.bin(c, Bin::And, d, hole);
                self.s.m.bin(c, Bin::Or, d, x)
            }
            "v_ffbl_b32" => {
                // -1 for zero, as DXIL's first-bit-low.
                let a = s(self, 0)?;
                self.s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 32, &[a])
            }
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => {
                // Bits of the mask's low (high) half below this lane, plus src1:
                // with an all-ones mask, the lane index.
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                // Mesh lanes are the console's by construction; pixel and
                // vertex lanes are the host's, as their masks are.
                if self.mask_data == MaskData::Host {
                    // Host lanes: the mask must be one (a ballot) or all lanes.
                    let Fields::Valu(v) = &inst.fields else {
                        unreachable!()
                    };
                    let host = match v.src.first() {
                        Some(&Operand::Const(193)) => true,
                        Some(&Operand::Scalar { code, .. }) => {
                            inst.read(Reg::S(code)).is_some_and(|m| {
                                self.x.st.masks.contains_key(&m) || self.x.st.highs.contains_key(&m)
                            })
                        }
                        _ => false,
                    };
                    if !host {
                        return Err("v_mbcnt of a mask that is no ballot (console lane layout)");
                    }
                    self.s.flags |= flags::WAVE_OPS;
                } else if self.s.mesh.is_none() {
                    self.cross_lane(false)?;
                }
                let lane = self.lane_index();
                let below = if name == "v_mbcnt_lo_u32_b32" {
                    lane
                } else {
                    let (k32, z) = (k(self, 32), k(self, 0));
                    let t = self.s.m.bin(c, Bin::Sub, lane, k32);
                    let low = self.s.m.cmp(c, pred::IULT, lane, k32);
                    self.s.m.select(c, low, z, t)
                };
                // (1 << below) - 1, all ones from 32 on.
                let (one, k32, ones) = (k(self, 1), k(self, 32), k(self, u32::MAX));
                let sh = m31(self, below);
                let m = self.s.m.bin(c, Bin::Shl, one, sh);
                let m = self.s.m.bin(c, Bin::Sub, m, one);
                let full = self.s.m.cmp(c, pred::IUGE, below, k32);
                let m = self.s.m.select(c, full, ones, m);
                let x = self.s.m.bin(c, Bin::And, a, m);
                let n = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 31, &[x]);
                self.s.m.bin(c, Bin::Add, n, b)
            }
            _ => return Err(name),
        })
    }

    fn compare(&mut self, inst: &Inst, rest: &str) -> Result<(), Why> {
        let (op, ty) = rest.rsplit_once('_').ok_or("a compare")?;
        let c = self.cur;
        let r = match ty {
            "f32" | "f16" if op == "class" => {
                // Bit k of src1 tests class k: signalling NaN, quiet NaN,
                // -inf, -normal, -denormal, -0, +0, +denormal, +normal, +inf.
                let f16 = ty == "f16";
                let x = if f16 {
                    let Fields::Valu(v) = &inst.fields else {
                        unreachable!()
                    };
                    let mut h = self.half_src(inst, 0)?;
                    if v.abs & 1 != 0 {
                        let m = self.s.m.i32(0x7FFF);
                        h = self.s.m.bin(c, Bin::And, h, m);
                    }
                    if v.neg & 1 != 0 {
                        let s = self.s.m.i32(0x8000);
                        h = self.s.m.bin(c, Bin::Xor, h, s);
                    }
                    h
                } else {
                    let x = self.fsrc(inst, 0)?;
                    self.as_int(x)
                };
                let mask = self.src(inst, 1)?;
                let k = |l: &mut Self, v: u32| l.s.m.i32(v);
                let (exp_sh, sign_sh, exp_mask, mant_mask, quiet_bit) = if f16 {
                    (10, 15, 0x1F, 0x03FF, 0x0200)
                } else {
                    (23, 31, 0xFF, 0x7F_FFFF, 0x40_0000)
                };
                let (k_exp, k_sign, k_emax, mant, quiet) = (
                    k(self, exp_sh),
                    k(self, sign_sh),
                    k(self, exp_mask),
                    k(self, mant_mask),
                    k(self, quiet_bit),
                );
                let e = self.s.m.bin(c, Bin::LShr, x, k_exp);
                let e = self.s.m.bin(c, Bin::And, e, k_emax);
                let m = self.s.m.bin(c, Bin::And, x, mant);
                let neg = self.s.m.bin(c, Bin::LShr, x, k_sign);
                let zero = k(self, 0);
                let (e0, emax) = (
                    self.s.m.cmp(c, pred::EQ, e, zero),
                    self.s.m.cmp(c, pred::EQ, e, k_emax),
                );
                let m0 = self.s.m.cmp(c, pred::EQ, m, zero);
                let q = self.s.m.bin(c, Bin::And, x, quiet);
                let q = self.s.m.cmp(c, pred::NE, q, zero);
                // Positive classes 6-9; negative ones are 11 minus those.
                let (normal, inf, zero_c, denorm) =
                    (k(self, 8), k(self, 9), k(self, 6), k(self, 7));
                let t = self.s.m.select(c, m0, zero_c, denorm);
                let t = self.s.m.select(c, e0, t, normal);
                let t = self.s.m.select(c, emax, inf, t);
                let eleven = k(self, 11);
                let mirrored = self.s.m.bin(c, Bin::Sub, eleven, t);
                let is_neg = self.s.m.cmp(c, pred::NE, neg, zero);
                let t = self.s.m.select(c, is_neg, mirrored, t);
                let (one, snan) = (k(self, 1), k(self, 0));
                let nan_class = self.s.m.select(c, q, one, snan);
                let yes = self.s.m.bool(true);
                let nan = self.s.m.bin(c, Bin::Xor, m0, yes);
                let nan = self.s.m.bin(c, Bin::And, nan, emax);
                let t = self.s.m.select(c, nan, nan_class, t);
                let bit = self.s.m.bin(c, Bin::LShr, mask, t);
                let bit = self.s.m.bin(c, Bin::And, bit, one);
                self.s.m.cmp(c, pred::NE, bit, zero)
            }
            "f32" | "f16" => {
                let (a, b) = if ty == "f16" {
                    (self.half_fsrc(inst, 0)?, self.half_fsrc(inst, 1)?)
                } else {
                    (self.fsrc(inst, 0)?, self.fsrc(inst, 1)?)
                };
                if op == "lg" {
                    // Ordered and not equal, false for a NaN, as `olt | ogt`.
                    // `fcmp one` means the same, but the AMD D3D12 driver
                    // runs it as unordered (true for a NaN) where NVIDIA
                    // and WARP do not; olt and ogt agree on all three.
                    let lt = self.s.m.cmp(c, pred::OLT, a, b);
                    let gt = self.s.m.cmp(c, pred::OGT, a, b);
                    self.s.m.bin(c, Bin::Or, lt, gt)
                } else if op == "f" || op == "tru" {
                    self.s.m.bool(op == "tru")
                } else {
                    let p = match op {
                        "f" => pred::FALSE,
                        "lt" => pred::OLT,
                        "eq" => pred::OEQ,
                        "le" => pred::OLE,
                        "gt" => pred::OGT,
                        "ge" => pred::OGE,
                        "o" => pred::ORD,
                        "u" => pred::UNO,
                        "nge" => pred::ULT,
                        "nlg" => pred::UEQ,
                        "ngt" => pred::ULE,
                        "nle" => pred::UGT,
                        "neq" => pred::UNE,
                        "nlt" => pred::UGE,
                        "tru" => pred::TRUE,
                        _ => return Err(inst.name()),
                    };
                    self.s.m.cmp(c, p, a, b)
                }
            }
            "u64" | "i64" => {
                let (a, b) = (self.src64(inst, 0)?, self.src64(inst, 1)?);
                let signed = ty == "i64";
                let p = match op {
                    "eq" => pred::EQ,
                    "ne" | "lg" => pred::NE,
                    "lt" if signed => pred::SLT,
                    "le" if signed => pred::SLE,
                    "gt" if signed => pred::SGT,
                    "ge" if signed => pred::SGE,
                    "lt" => pred::IULT,
                    "le" => pred::IULE,
                    "gt" => pred::IUGT,
                    "ge" => pred::IUGE,
                    _ => return Err("a 64-bit compare"),
                };
                self.s.m.cmp(c, p, a, b)
            }
            "u32" | "i32" | "u16" | "i16" => {
                let (a, b) = if ty.ends_with("16") {
                    let (mut a, mut b) = (self.half_src(inst, 0)?, self.half_src(inst, 1)?);
                    if ty == "i16" {
                        let k16 = self.s.m.i32(16);
                        for x in [&mut a, &mut b] {
                            let t = self.s.m.bin(c, Bin::Shl, *x, k16);
                            *x = self.s.m.bin(c, Bin::AShr, t, k16);
                        }
                    }
                    (a, b)
                } else {
                    (self.src(inst, 0)?, self.src(inst, 1)?)
                };
                let signed = ty == "i32" || ty == "i16";
                let p = match op {
                    "eq" => pred::EQ,
                    "ne" | "lg" => pred::NE,
                    "lt" if signed => pred::SLT,
                    "le" if signed => pred::SLE,
                    "gt" if signed => pred::SGT,
                    "ge" if signed => pred::SGE,
                    "lt" => pred::IULT,
                    "le" => pred::IULE,
                    "gt" => pred::IUGT,
                    "ge" => pred::IUGE,
                    "f" | "t" => {
                        self.lane(inst, Some(op == "t"));
                        return Ok(());
                    }
                    _ => return Err("an integer compare"),
                };
                self.s.m.cmp(c, p, a, b)
            }
            _ => return Err(inst.name()),
        };
        self.lane_bits_for(inst, r);
        Ok(())
    }

    fn lane(&mut self, inst: &Inst, value: Option<bool>) {
        let b = self.s.m.bool(value == Some(true));
        self.lane_bits_for(inst, b);
    }

    fn lane_bits_for(&mut self, inst: &Inst, r: V) {
        let Fields::Valu(v) = &inst.fields else {
            return;
        };
        let code = if inst.name().starts_with("v_cmpx") {
            Some(EXEC)
        } else {
            match v.dst {
                Some(Operand::Scalar { code, .. }) => Some(code),
                _ => None,
            }
        };
        if let Some(lo) = code.and_then(|c| inst.write(Reg::S(c))) {
            self.lane_bits.insert(lo, r);
        }
        for &(_, w) in &inst.writes {
            self.vals
                .entry(w)
                .or_insert(Val::Missing("a lane mask read as data"));
        }
    }

    fn interp(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        let Some(&interp) = self.interps.get(&site) else {
            // Keep an unknown partial sum unknown: low-half writes can
            // preserve that fact, and reads of the retained bits must retry.
            return Err(FIRST_HALF);
        };
        if let Interp::Interpolated { unless, .. } = interp
            && !self.avoids(site, unless)
        {
            return Err("an interpolation with computed barycentrics");
        }
        let name = inst.name();
        let (dst, src) = match &inst.fields {
            Fields::Vintrp { dst, src, .. } => (*dst, Some(*src)),
            Fields::Valu(v) => {
                if v.abs != 0 || v.neg != 0 {
                    return Err("interpolation operand modifiers");
                }
                (v.dst.ok_or("an interpolation")?, v.src.get(1).copied())
            }
            _ => unreachable!(),
        };
        let Operand::Vector { reg, .. } = dst else {
            return Err("an interpolation");
        };
        // A float operand of the instruction.
        let float = |l: &mut Self, op: Option<Operand>| -> Result<V, Why> {
            let x = l.operand(inst, &op.ok_or("an interpolation operand")?)?;
            Ok(l.as_float(x))
        };
        // What the result adds to the evaluated channel (a second half's
        // first half, when the halves evaluate on their own).
        let mut add = None;
        let (attribute, eval) = match *interp {
            Interp::Interpolated {
                attribute,
                barycentric,
                ..
            } => {
                let mode = interp_mode(barycentric).ok_or("pull-model interpolation")?;
                let eval = if self.per_vertex(attribute.row) {
                    // The element is read per vertex elsewhere, so it is
                    // constant: weigh its vertices with SV_Barycentrics.
                    let mut ij = [None; 2];
                    for (index, w) in ij.iter_mut().enumerate() {
                        let x = match self.barycentric(barycentric, index as u8) {
                            Val::I32(x) => x,
                            Val::Missing(w) => return Err(w),
                            Val::Bit(_) => unreachable!(),
                        };
                        *w = Some(self.as_float(x));
                    }
                    Eval::Weights {
                        p0: true,
                        i: ij[0],
                        j: ij[1],
                    }
                } else {
                    Eval::Mode(mode)
                };
                (attribute, eval)
            }
            // P0, the provoking vertex: a flat (nointerpolation) input.
            Interp::Vertex {
                attribute,
                vertex: 2,
            } => (attribute, Eval::Mode(1)),
            // P10 and P20: what the first and second halves weigh, P1 - P0
            // and P2 - P0, which is what the hardware holds for an
            // attribute the program also interpolates.
            Interp::Vertex { attribute, vertex } if self.interpolated(attribute.row) => {
                let one = Some(self.s.m.f32(1f32.to_bits()));
                let (i, j) = if vertex == 0 {
                    (one, None)
                } else {
                    (None, one)
                };
                (attribute, Eval::Weights { p0: false, i, j })
            }
            Interp::Vertex { .. } => return Err("a non-provoking vertex value"),
            // A half of an interpolation with computed i or j: P0 + i P10,
            // or the old dst (f16: the third operand) + j P20.
            Interp::Weighted { attribute } if name.starts_with("v_interp_p1") => {
                let i = Some(float(self, src)?);
                (
                    attribute,
                    Eval::Weights {
                        p0: true,
                        i,
                        j: None,
                    },
                )
            }
            Interp::Weighted { attribute } => {
                let old = match &inst.fields {
                    Fields::Valu(v) => v.src.get(2).copied(),
                    _ => Some(dst),
                };
                add = Some(float(self, old)?);
                let j = Some(float(self, src)?);
                (
                    attribute,
                    Eval::Weights {
                        p0: false,
                        i: None,
                        j,
                    },
                )
            }
        };
        let channel = |l: &mut Self, row: u8, ch: u8| -> Result<V, Why> {
            let x = l.eval_channel(row, ch, eval)?;
            if let Some(a) = add {
                let x = l.as_float(x);
                let x = l.s.m.bin(l.cur, Bin::Add, a, x);
                Ok(l.as_int(x))
            } else {
                Ok(x)
            }
        };
        let packing = match &self.x.native.io {
            StageIo::Ps { inputs, .. } => inputs
                .iter()
                .find(|i| i.row == attribute.row && i.fp16)
                .map(|i| i.pack2),
            _ => None,
        };
        let f16 = inst.name().ends_with("_f16");
        let x = match packing {
            None if f16 || attribute.high => return Err("a 16-bit read of a 32-bit input"),
            None => channel(self, attribute.row, attribute.channel)?,
            Some(pack2) => {
                // 16-bit values: one in the low half of each dword, or two
                // per dword (`packed_half`); the host's are 32-bit floats.
                let half = |l: &mut Self, h: u8| -> Result<Option<V>, Why> {
                    let src = if pack2 {
                        packed_half(&l.s.inputs, attribute.row, h)
                    } else {
                        h.is_multiple_of(2).then_some((attribute.row, h / 2))
                    };
                    let Some((row, ch)) = src else {
                        return Ok(None);
                    };
                    let x = channel(l, row, ch)?;
                    let f = l.as_float(x);
                    Ok(Some(f))
                };
                let h = 2 * attribute.channel;
                let c = self.cur;
                if name == "v_interp_p1ll_f16" {
                    let f = half(self, h + u8::from(attribute.high))?
                        .ok_or("a packed input half no element holds")?;
                    self.as_int(f)
                } else if f16 {
                    let f = half(self, h + u8::from(attribute.high))?
                        .ok_or("a packed input half no element holds")?;
                    let lo = self.narrow16(f);
                    // omod and clamp are part of the f16 instruction; op_sel[3]
                    // picks the destination half.
                    let lo = self.half_modifiers(inst, lo);
                    return self.write_half(inst, lo, None);
                } else if matches!(eval, Eval::Mode(1)) {
                    // The whole dword of a flat input: both halves.
                    let z = self.s.m.i32(0);
                    let lo = match half(self, h)? {
                        Some(f) => self.narrow16(f),
                        None => z,
                    };
                    let hi = match half(self, h + 1)? {
                        Some(f) => self.narrow16(f),
                        None => z,
                    };
                    let k16 = self.s.m.i32(16);
                    let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                    self.s.m.bin(c, Bin::Or, lo, hi)
                } else {
                    return Err("32-bit interpolation of a 16-bit input");
                }
            }
        };
        // A pixel interpolates in every lane; guards only hide inactive ones.
        self.write(inst, Reg::V(reg), x, None);
        Ok(())
    }

    /// Whether no lane that runs the instruction at `site` has been where
    /// `unless` says: a lane that came along such an edge or ran (skipped)
    /// such an instruction meets its condition, which the instruction's must
    /// contradict. Those lane bits must not change in between, so none of
    /// those blocks is on a cycle. A lane mask joined where such an edge
    /// enters holds, for the lanes that came along it, the edge's mask.
    fn per_vertex(&self, row: u8) -> bool {
        self.x.io.interps.iter().any(|(_, i)| match *i {
            Interp::Weighted { attribute } => attribute.row == row,
            Interp::Vertex { attribute, vertex } => {
                attribute.row == row && vertex != 2 && self.interpolated(row)
            }
            Interp::Interpolated { .. } => false,
        })
    }

    fn interpolated(&self, row: u8) -> bool {
        self.x.io.interps.iter().any(|(_, i)| match *i {
            Interp::Interpolated { attribute, .. } | Interp::Weighted { attribute } => {
                attribute.row == row
            }
            Interp::Vertex { .. } => false,
        })
    }

    fn eval_channel(&mut self, row: u8, ch: u8, eval: Eval) -> Result<V, Why> {
        let (id, col) = self
            .find_input(row, ch)
            .ok_or("an input the signature lacks")?;
        let mode = match eval {
            Eval::Mode(mode) => mode,
            Eval::Weights { .. } => 1,
        };
        let e = &mut self.s.inputs[id];
        if e.interp != mode && e.usage != 0 {
            return Err("one input interpolated two ways");
        }
        e.interp = mode;
        let Eval::Weights { p0, i, j } = eval else {
            return match self.load_any_input(id, col) {
                Val::I32(x) => Ok(x),
                _ => Err("an input"),
            };
        };
        let v0 = self.vertex_input(id, col, 0);
        let c = self.cur;
        let mut acc = p0.then_some(v0);
        for (k, w) in [(1, i), (2, j)] {
            let Some(w) = w else { continue };
            let vk = self.vertex_input(id, col, k);
            let d = self.s.m.bin(c, Bin::Sub, vk, v0);
            acc = Some(match acc {
                Some(a) => self.fmad(w, d, a),
                None => self.s.m.bin(c, Bin::Mul, w, d),
            });
        }
        let acc = acc.ok_or("an interpolation")?;
        Ok(self.as_int(acc))
    }

    fn vertex_input(&mut self, id: usize, col: u8, k: u8) -> V {
        let e = &mut self.s.inputs[id];
        e.usage |= 1 << col;
        let int = e.comp != 9;
        let (ty, suffix) = if int {
            (self.s.t.i32, ".i32")
        } else {
            (self.s.t.f32, ".f32")
        };
        self.s.flags |= flags::BARYCENTRICS;
        let entry = self.s.entry;
        let args = [
            self.s.m.i32(id as u32),
            self.s.m.i32(0),
            self.s.m.i8(col),
            self.s.m.i8(k),
        ];
        let v = self.s.op(
            entry,
            &format!("attributeAtVertex{suffix}"),
            Attr::ReadNone,
            ty,
            137,
            &args,
        );
        if int {
            self.s.m.cast(entry, Cast::Bitcast, v, self.s.t.f32)
        } else {
            v
        }
    }

    fn avoids(&self, (b, i): Site, unless: &[Elsewhere]) -> bool {
        let st = self.x.st;
        let runs = [
            (st.reach[b], true),
            (st.guards[b][i].unwrap_or(Bool::TRUE), true),
        ];
        let mut atoms = st.bools.support(runs[0].0);
        atoms.extend(st.bools.support(runs[1].0));
        unless.iter().all(|&u| {
            let (mut all, at, mut links) = match u {
                Elsewhere::Edge(q, to) => {
                    let all = self.lane_edge(q, to);
                    let block = &self.x.p.blocks[to];
                    let n = block.preds.iter().position(|&p| p == q).expect("an edge");
                    let links: Vec<(Bool, Bool)> = block
                        .phis
                        .iter()
                        .filter(|ph| atoms.contains(&Atom::Phi(ph.value)))
                        .filter_map(|ph| {
                            Some((*st.masks.get(&ph.value)?, *st.masks.get(&ph.args[n])?))
                        })
                        .collect();
                    (all, to, links)
                }
                Elsewhere::Write((w, k)) | Elsewhere::Skip((w, k)) => {
                    let guard = st.guards[w][k].unwrap_or(Bool::TRUE);
                    let ran = matches!(u, Elsewhere::Write(_));
                    (vec![(st.reach[w], true), (guard, ran)], w, vec![])
                }
            };
            all.extend(runs);
            // Each joined mask equals the incoming one: try both values.
            links.truncate(6);
            let some_lane = (0..1u32 << links.len()).any(|bits| {
                let mut all = all.clone();
                for (k, &(joined, incoming)) in links.iter().enumerate() {
                    let v = bits >> k & 1 != 0;
                    all.extend([(joined, v), (incoming, v)]);
                }
                st.bools.meet(&all)
            });
            !self.on_cycle(at) && !some_lane
        })
    }

    /// What holds in a lane that goes from IR block `q` to `b`.
    fn lane_edge(&self, q: usize, b: usize) -> Vec<(Bool, bool)> {
        let st = self.x.st;
        let mut all = vec![(st.reach[q], true)];
        if let LaneExit::Branch {
            when,
            taken,
            not_taken,
        } = st.exits[q]
            && taken != not_taken
        {
            all.push((when, taken == b));
        }
        all
    }

    /// Whether every lane enters IR block `b` along the edge the console's
    /// wave takes, so that an SGPR joined there holds what that edge
    /// brought: every branch into it is uniform (SCC values). The wave runs
    /// one path; a per-lane branch into the join (an EXEC skip, a loop's
    /// per-lane exit) is where lanes would take edges the wave did not, and
    /// the console compiler's if/else always skips into its join that way.
    /// Lanes that left EXEC inside a uniform loop (the BVH walks) take no
    /// edge into its header; the others take the wave's.
    fn uniform_join(&self, b: usize) -> bool {
        let st = self.x.st;
        let preds = &self.x.p.blocks[b].preds;
        preds
            .iter()
            .filter(|&&q| self.block[q].is_some())
            .all(|&q| match st.exits[q] {
                LaneExit::Branch {
                    when,
                    taken,
                    not_taken,
                } if taken != not_taken => st
                    .bools
                    .support(when)
                    .iter()
                    .all(|a| matches!(a, Atom::Uniform(_))),
                _ => true,
            })
    }

    /// Whether no lane goes from IR block `q` to `b`.
    fn never_taken(&self, q: usize, b: usize) -> bool {
        !self.x.st.bools.meet(&self.lane_edge(q, b))
    }

    /// Whether, at a per-lane join `b` of a lane mask whose register holds
    /// data along edge `n`, some edge brings a mask and no lane taking edge
    /// `n` is in any of them (a compare inside a region those lanes skip).
    /// The console wave runs one path: on a mask edge, the lanes of edge `n`
    /// read their bit of that mask, 0. The compiler merges no value for them,
    /// so it holds none for them either where the whole wave skips (the data
    /// there is a reused register): 0 is theirs.
    fn no_mask_lane_on_edge(&self, b: usize, args: &[Value], n: usize) -> bool {
        let st = self.x.st;
        let preds = &self.x.p.blocks[b].preds;
        let on_n = self.lane_edge(preds[n], b);
        let mut some = false;
        for (k, arg) in args.iter().enumerate() {
            let Some(&m) = st.masks.get(arg).filter(|_| k != n) else {
                continue;
            };
            some = true;
            let mut lanes = on_n.clone();
            lanes.push((m, true));
            if st.bools.meet(&lanes) {
                return false;
            }
        }
        some
    }

    /// Whether IR block `b` can reach itself.
    fn on_cycle(&self, b: usize) -> bool {
        let blocks = &self.x.p.blocks;
        let mut seen = vec![false; blocks.len()];
        let mut stack = blocks[b].exit.successors();
        while let Some(x) = stack.pop() {
            if x == b {
                return true;
            }
            if !std::mem::replace(&mut seen[x], true) {
                stack.extend(blocks[x].exit.successors());
            }
        }
        false
    }

    /// `x` (an f16 in the low bits) as the low half of VGPR `reg`: 16-bit
    /// results keep the register's high half (GFX10).
    fn low_half(&mut self, inst: &Inst, reg: u8, x: V) -> V {
        let c = self.cur;
        let old = match inst.read(Reg::V(reg)).map(|o| self.i32_of(o)) {
            Some(Ok(o)) => o,
            _ => self.s.m.i32(0),
        };
        let (high, low) = (self.s.m.i32(0xFFFF_0000), self.s.m.i32(0xFFFF));
        let old = self.s.m.bin(c, Bin::And, old, high);
        let x = self.s.m.bin(c, Bin::And, x, low);
        self.s.m.bin(c, Bin::Or, old, x)
    }

    /// VOP3 f16 output modifiers: omod, then clamp, applied in f32 so the
    /// half conversion rounds once (the order `write_f` uses).
    fn half_modifiers(&mut self, inst: &Inst, h: V) -> V {
        let Fields::Valu(v) = &inst.fields else {
            return h;
        };
        if v.omod == 0 && !v.clamp {
            return h;
        }
        let c = self.cur;
        let f32t = self.s.t.f32;
        let mut f = self
            .s
            .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
        if v.omod != 0 {
            let factor = [1.0f32, 2.0, 4.0, 0.5][usize::from(v.omod)];
            let k = self.s.m.f32(factor.to_bits());
            f = self.s.m.bin(c, Bin::Mul, f, k);
        }
        if v.clamp {
            f = self.unary(7, f);
        }
        self.s
            .op(c, "legacyF32ToF16", Attr::ReadNone, self.s.t.i32, 130, &[f])
    }

    /// An image instruction, its reads of texture views marked (module
    /// notes, "Read marks") after it.
    fn image(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        self.s.texture_reads.clear();
        let lowered = self.image_op(inst, b, i);
        for index in std::mem::take(&mut self.s.texture_reads) {
            if lowered.is_ok() {
                self.mark_view_read(index);
            }
        }
        lowered
    }

    /// Sets the read mark of resource-heap index `index` (module notes,
    /// "Read marks") where it is still clear. Nothing without
    /// [`Parts::view_marks`].
    fn mark_view_read(&mut self, index: V) {
        if !self.x.view_marks {
            return;
        }
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let bitmap = self.s.view_writes_handle();
        let c = self.cur;
        let (base, k5, k2, k31, one, zero) = (
            k(self, super::VIEW_READS),
            k(self, 5),
            k(self, 2),
            k(self, 31),
            k(self, 1),
            k(self, 0),
        );
        let n = self.s.m.bin(c, Bin::Add, index, base);
        let word = self.s.m.bin(c, Bin::LShr, n, k5);
        let word = self.s.m.bin(c, Bin::Shl, word, k2);
        let bit = self.s.m.bin(c, Bin::And, n, k31);
        let bit = self.s.m.bin(c, Bin::Shl, one, bit);
        // Most reads find their slot marked: a load, not an atomic.
        let old = self.raw_dword(bitmap, word);
        let set = self.s.m.bin(c, Bin::And, old, bit);
        let clear = self.s.m.cmp(c, pred::EQ, set, zero);
        let at = self.begin_guarded(Some(clear));
        let iu = self.s.m.undef(self.s.t.i32);
        // Operation 2 is OR.
        self.s.op(
            self.cur,
            "atomicBinOp.i32",
            Attr::NoUnwind,
            self.s.t.i32,
            78,
            &[bitmap, k2, word, iu, iu, bit],
        );
        self.end_guarded(at, &[]);
    }

    fn image_op(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Image {
            data,
            addr,
            dmask,
            dim,
            unorm,
            a16,
            d16,
            tfe,
            lwe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        if *tfe || *lwe {
            return Err("an image access with tfe or lwe");
        }
        let name = inst.name();
        if name.starts_with("image_bvh") {
            return self.bvh(inst, b, i);
        }
        // A load or sample with no channel enabled returns the first.
        let dmask = if *dmask == 0 { &1 } else { dmask };
        let site = (b, i);
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let (kind, coords) = match dim {
            0 => (1, 1),
            1 => (2, 2),
            2 => (4, 3),
            // Sampled (face coordinates and face, see `cube_direction`) or
            // sized only.
            3 if name.starts_with("image_sample")
                || name.starts_with("image_gather")
                || name == "image_get_resinfo" =>
            {
                (5, 3)
            }
            4 => (6, 2),
            5 => (7, 3),
            _ => return Err("an image dimension (cube loads or MSAA)"),
        };
        // Address VGPRs in order.
        let mut regs = Vec::new();
        for a in addr {
            if let Operand::Vector { reg, dwords } = *a {
                regs.extend((0..dwords).map(|k| reg + k));
            }
        }
        // With a16, two 16-bit components per VGPR, low half first, after
        // the offset, bias and compare value, which keep a VGPR each (an f16
        // bias in its low half).
        let extra = name
            .strip_prefix("image_sample")
            .or_else(|| name.strip_prefix("image_gather4"))
            .map_or(0, |rest| {
                rest.split('_')
                    .filter(|t| matches!(*t, "o" | "b" | "c"))
                    .count()
            });
        let addr_val = |l: &mut Self, k: usize| -> Result<V, Why> {
            let packed = *a16 && k >= extra;
            let r = *regs
                .get(if packed { extra + (k - extra) / 2 } else { k })
                .ok_or("an image address")?;
            let v = inst.read(Reg::V(r)).ok_or("an image address")?;
            let x = l.source(v)?;
            if !packed {
                return Ok(x);
            }
            let c = l.cur;
            let half = (k - extra) as u32 % 2;
            let (sh, mask) = (l.s.m.i32(16 * half), l.s.m.i32(0xFFFF));
            let x = l.s.m.bin(c, Bin::LShr, x, sh);
            Ok(l.s.m.bin(c, Bin::And, x, mask))
        };
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked image")?;
        let store = name.starts_with("image_store");
        let uav = store
            || self
                .used_kind(src)
                .is_some_and(|u| u.kind == ResourceType::Uav);
        let props = (kind | if uav { 0x1000 } else { 0 }, 0x409);
        if let Some(op) = name.strip_prefix("image_atomic_") {
            // DXIL's atomic operations on a 32-bit integer texture UAV; the
            // old value returns with glc. Sub is the add of the negation.
            let code = match op {
                "add" | "sub" => 0,
                "and" => 1,
                "or" => 2,
                "xor" => 3,
                "smin" => 4,
                "smax" => 5,
                "umin" => 6,
                "umax" => 7,
                "swap" => 8,
                "cmpswap" => u32::MAX,
                _ => return Err(name),
            };
            let Fields::Image { cache, .. } = &inst.fields else {
                unreachable!()
            };
            // 16-bit coordinates (a16) are read as `addr_val` reads them.
            if *d16 {
                return Err("a 16-bit image atomic");
            }
            // dmask: the data dwords (cmpswap: twice the value's); two per
            // value is a 64-bit atomic (an R32G32_UINT view, low dword in R).
            let values = if code == u32::MAX { 2 } else { 1 };
            let wide = match dmask.count_ones() / values {
                1 => false,
                2 => true,
                _ => return Err("an image atomic of more than two dwords"),
            };
            let n = if wide { 2 } else { 1 };
            let iu = self.s.m.undef(self.s.t.i32);
            let mut cs = vec![iu; 3];
            for (k, slot) in cs.iter_mut().enumerate().take(coords) {
                *slot = addr_val(self, k)?;
            }
            let Operand::Vector { reg, .. } = *data else {
                return Err("image atomic data");
            };
            let vals = self.vgprs(inst, data, values as u8 * n)?;
            let i64t = self.s.m.int(64);
            // Value k of the data: one dword, or two as an i64.
            let value_at = |l: &mut Self, k: usize| -> V {
                if !wide {
                    return vals[k];
                }
                let lo = l.s.m.cast(c, Cast::ZExt, vals[2 * k], i64t);
                let hi = l.s.m.cast(c, Cast::ZExt, vals[2 * k + 1], i64t);
                let k32 = l.s.m.int_const(i64t, 32);
                let hi = l.s.m.bin(c, Bin::Shl, hi, k32);
                l.s.m.bin(c, Bin::Or, lo, hi)
            };
            let mut value = value_at(self, 0);
            if op == "sub" {
                let z = if wide {
                    self.s.m.int_const(i64t, 0)
                } else {
                    self.s.m.i32(0)
                };
                value = self.s.m.bin(c, Bin::Sub, z, value);
            }
            let mut old = Vec::new();
            for k in 0..n {
                old.push(match inst.read(Reg::V(reg + k)) {
                    Some(v) if cache.glc => self.i32_of(v)?,
                    _ => self.s.m.i32(0),
                });
            }
            if wide {
                // SM 6.6's 64-bit atomics on typed resources in the heap: the
                // host device must support both (D3D12 options 9 and 11).
                self.s.flags |= flags::INT64 | flags::ATOMIC64_TYPED | flags::ATOMIC64_HEAP;
            }
            let h = self.handle(site, src, false, (kind | 0x1000, 0x105))?;
            let at = self.begin_guarded(guard);
            let (ty, suffix) = if wide {
                (i64t, "i64")
            } else {
                (self.s.t.i32, "i32")
            };
            let mut args = vec![h];
            let r = if code == u32::MAX {
                // cmpswap: data is (new, compare).
                args.extend(cs);
                let (new, compare) = (value, value_at(self, 1));
                args.extend([compare, new]);
                self.s.op(
                    self.cur,
                    &format!("atomicCompareExchange.{suffix}"),
                    Attr::NoUnwind,
                    ty,
                    79,
                    &args,
                )
            } else {
                args.push(self.s.m.i32(code));
                args.extend(cs);
                args.push(value);
                self.s.op(
                    self.cur,
                    &format!("atomicBinOp.{suffix}"),
                    Attr::NoUnwind,
                    ty,
                    78,
                    &args,
                )
            };
            let r = if wide {
                let (lo, hi) = self.split64(r);
                vec![lo, hi]
            } else {
                vec![r]
            };
            let inside: Vec<_> = r.into_iter().zip(old).collect();
            let r = self.end_guarded(at, &inside);
            if cache.glc {
                for (k, x) in (0..).zip(r) {
                    self.write(inst, Reg::V(reg + k), x, None);
                }
            }
            return Ok(());
        }
        if store {
            if name != "image_store" {
                return Err("an image store to a mip level");
            }
            // shortcut: stores are taken to write all four channels of a
            // float format (the console converts by the view's format).
            if *dmask != 0xF {
                return Err("an image store of some channels");
            }
            let iu = self.s.m.undef(self.s.t.i32);
            let mut cs = vec![iu; 3];
            for (k, slot) in cs.iter_mut().enumerate().take(coords) {
                *slot = addr_val(self, k)?;
            }
            let Operand::Vector { reg, .. } = *data else {
                return Err("image store data");
            };
            let mut vals = Vec::new();
            for k in 0..4u8 {
                // d16: two f16 channels per VGPR, low half first.
                let r = if *d16 { k / 2 } else { k };
                let v = inst.read(Reg::V(reg + r)).ok_or("image store data")?;
                vals.push(if *d16 {
                    let mut x = self.i32_of(v)?;
                    if k % 2 == 1 {
                        let k16 = self.s.m.i32(16);
                        x = self.s.m.bin(self.cur, Bin::LShr, x, k16);
                    }
                    let f32t = self.s.t.f32;
                    self.s
                        .op(self.cur, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x])
                } else {
                    self.float_of(v)?
                });
            }
            let h = self.handle(site, src, false, props)?;
            let at = self.begin_guarded(guard);
            let mut args = vec![h];
            args.extend(cs);
            args.extend(vals);
            args.push(self.s.m.i8(0xF));
            let void = self.s.t.void;
            self.s.op(
                self.cur,
                "textureStore.f32",
                Attr::NoUnwind,
                void,
                67,
                &args,
            );
            self.end_guarded(at, &[]);
            return Ok(());
        }
        let (op, sample) = match name {
            "image_sample" | "image_sample_o" => (60, true),
            "image_sample_lz" | "image_sample_l" => (62, true),
            "image_sample_b" => (61, true),
            "image_sample_c_lz" => (65, true),
            "image_sample_d" | "image_sample_d_cl" => (63, true),
            "image_get_lod" if self.s.pixel => (81, true),
            // Implicit derivatives: pixel shaders only.
            "image_sample_c" if self.s.pixel => (64, true),
            "image_gather4" | "image_gather4_lz" | "image_gather4_lz_o" | "image_gather4_l"
            | "image_gather4_l_o" => (73, true),
            "image_load" | "image_load_mip" => (66, false),
            "image_get_resinfo" => (72, false),
            _ => return Err(name),
        };
        if op == 72 {
            // Width, height, depth or layers, levels (per dmask) of the level
            // in the first address: DXIL's dimensions; a channel DXIL leaves
            // out reads 1 (UAVs: one level).
            let h = self.handle(site, src, false, props)?;
            let mip = if uav {
                self.s.m.undef(self.s.t.i32)
            } else {
                addr_val(self, 0)?
            };
            let i32t = self.s.t.i32;
            let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
            let d = self
                .s
                .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, mip]);
            let one = self.s.m.i32(1);
            let present = match kind {
                1 => [true, false, false, true],
                2 | 5 | 6 => [true, true, false, true],
                _ => [true, true, true, true],
            };
            let Operand::Vector { reg, .. } = *data else {
                return Err("an image result");
            };
            for (k, ch) in (0..4u32).filter(|ch| dmask >> ch & 1 != 0).enumerate() {
                let x = if ch == 3 && uav || !present[ch as usize] {
                    one
                } else {
                    self.s.m.extract(c, d, ch)
                };
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        if uav && op != 66 {
            return Err("sampling a UAV");
        }
        let h = self.handle(site, src, false, props)?;
        let fu = self.s.m.undef(self.s.t.f32);
        let iu = self.s.m.undef(self.s.t.i32);
        let r = if sample {
            let ss = self
                .descriptors
                .get(&(site, true))
                .copied()
                .ok_or("an untracked sampler")?;
            if matches!(name, "image_gather4_l" | "image_gather4_l_o") {
                if *a16 || *d16 || *unorm || kind != 2 {
                    return Err(
                        "a gather at a level of 16-bit values, texel coordinates or not 2D",
                    );
                }
                let n = 3 + usize::from(name.ends_with("_o"));
                let a = (0..n)
                    .map(|k| addr_val(self, k))
                    .collect::<Result<Vec<_>, _>>()?;
                let lod = inst.read(Reg::V(regs[n - 1])).ok_or("an image address")?;
                return self.gather_at_level(inst, h, ss, &a, lod, guard);
            }
            // Comparison: the reference value is the first address after
            // the bias, and the sampler is a comparison sampler.
            let cmp = matches!(op, 64 | 65);
            if cmp && (*a16 || dmask.count_ones() != 1) {
                return Err("a comparison sample of 16-bit coordinates or several channels");
            }
            let sh = self.handle(site, ss, true, (14 | if cmp { 0x8000 } else { 0 }, 0))?;
            // Texel offsets (`_o`) come first: x in bits 0-5, y in 8-13,
            // signed (programmable offsets: gathers only).
            let offset_word = name.ends_with("_o");
            let first =
                usize::from(offset_word) + usize::from(name == "image_sample_b") + usize::from(cmp);
            // Coordinates the size divides (not the array slice); explicit
            // gradients (x then y) come before the coordinates.
            let spatial = coords - usize::from(kind == 6 || kind == 7);
            // (A cube's are two per direction, on the face.)
            let grads = match op {
                63 if kind == 5 => 4,
                63 => 2 * spatial,
                _ => 0,
            };
            let mut cs = vec![fu; 4];
            // With a16, sampling coordinates and LOD are f16.
            let to_float = |l: &mut Self, x: V| {
                if *a16 {
                    let f = l.s.t.f32;
                    l.s.op(l.cur, "legacyF16ToF32", Attr::ReadNone, f, 131, &[x])
                } else {
                    l.as_float(x)
                }
            };
            // A level query takes no array slice (the code may pass anything
            // there, e.g. an unwritten register).
            let read = if op == 81 { spatial } else { coords };
            for (k, slot) in cs.iter_mut().enumerate().take(read) {
                let x = addr_val(self, first + grads + k)?;
                *slot = to_float(self, x);
            }
            if kind == 5 {
                if *unorm || op == 81 {
                    return Err("a cube sample with texel coordinates or a level query");
                }
                let dir = self.cube_direction(cs[0], cs[1], cs[2]);
                cs[..3].copy_from_slice(&dir);
            }
            if *unorm {
                // Texel coordinates: divided by the size of level 0.
                let i32t = self.s.t.i32;
                let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
                let z = self.s.m.i32(0);
                let d = self
                    .s
                    .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, z]);
                for (k, slot) in cs.iter_mut().enumerate().take(spatial) {
                    let n = self.s.m.extract(c, d, k as u32);
                    let n = self.s.m.cast(c, Cast::UiToFp, n, self.s.t.f32);
                    *slot = self.s.m.bin(c, Bin::SDiv, *slot, n);
                }
            }
            if op == 81 {
                // The clamped then the unclamped level of detail.
                let Operand::Vector { reg, .. } = *data else {
                    return Err("an image result");
                };
                let f32t = self.s.t.f32;
                for (k, ch) in (0..2u32).filter(|ch| dmask >> ch & 1 != 0).enumerate() {
                    let clamped = self.s.m.bool(ch == 0);
                    let lod = self.s.op(
                        c,
                        "calculateLOD.f32",
                        Attr::ReadOnly,
                        f32t,
                        81,
                        &[h, sh, cs[0], cs[1], cs[2], clamped],
                    );
                    let x = self.as_int(lod);
                    self.write(inst, Reg::V(reg + k as u8), x, guard);
                }
                return Ok(());
            }
            let offsets = if kind == 5 {
                0
            } else {
                (coords - usize::from(kind == 6 || kind == 7)).min(3)
            };
            let zero = self.s.m.i32(0);
            let mut os = vec![iu; 3];
            for o in os.iter_mut().take(offsets) {
                *o = zero;
            }
            let mut args = vec![h, sh];
            args.extend(&cs);
            match op {
                60 => {
                    if offset_word {
                        // DXIL takes immediate offsets in -8..=7; the
                        // console's are 6-bit and may be computed.
                        let w = addr_val(self, 0)?;
                        let w = self
                            .s
                            .m
                            .int_value(w)
                            .ok_or("a sample offset that is not constant")?
                            as u32;
                        for (k, o) in os.iter_mut().enumerate().take(offsets) {
                            let x = (w >> (8 * k) & 0x3F) as i32;
                            let x = if x >= 32 { x - 64 } else { x };
                            if !(-8..=7).contains(&x) {
                                return Err("a sample offset past -8..7");
                            }
                            *o = self.s.m.i32(x as u32);
                        }
                    }
                    args.extend(&os);
                    args.push(fu);
                    self.s
                        .op(c, "sample.f32", Attr::ReadOnly, self.s.t.res_ret, 60, &args)
                }
                61 => {
                    let bias = addr_val(self, 0)?;
                    let bias = to_float(self, bias);
                    args.extend(&os);
                    args.extend([bias, fu]);
                    self.s.op(
                        c,
                        "sampleBias.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        61,
                        &args,
                    )
                }
                63 if kind == 5 => {
                    // A cube's gradients are the face's: ds, dt per step in x,
                    // then in y, in face coordinates (a face spans 1). The level
                    // the hardware picks from them, isotropically: log2 of the
                    // longer gradient in texels of a face, then the clamp.
                    // shortcut: sampled at that level, so a sampler's
                    // anisotropy is lost and its LOD bias comes after the
                    // clamp (the console adds it before); D3D12 cube sampling
                    // takes 3D gradients only. Upgrade if cube lookups blur or
                    // a biased sampler meets a binding clamp.
                    let mut g = [fu; 4];
                    for (k, x) in g.iter_mut().enumerate() {
                        let v = addr_val(self, first + k)?;
                        *x = to_float(self, v);
                    }
                    let i32t = self.s.t.i32;
                    let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
                    let z = self.s.m.i32(0);
                    let d = self
                        .s
                        .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, z]);
                    let w = self.s.m.extract(c, d, 0);
                    let w = self.s.m.cast(c, Cast::UiToFp, w, self.s.t.f32);
                    let mut len2 = [fu; 2];
                    for (k, l) in len2.iter_mut().enumerate() {
                        let a = self.s.m.bin(c, Bin::Mul, g[2 * k], g[2 * k]);
                        let b = self.s.m.bin(c, Bin::Mul, g[2 * k + 1], g[2 * k + 1]);
                        *l = self.s.m.bin(c, Bin::Add, a, b);
                    }
                    // FMax, Log (base 2): 0.5 log2(max(|dx|², |dy|²) w²).
                    let m = self.binary(35, len2[0], len2[1]);
                    let w2 = self.s.m.bin(c, Bin::Mul, w, w);
                    let m = self.s.m.bin(c, Bin::Mul, m, w2);
                    let half = self.s.m.f32(0.5f32.to_bits());
                    let l = self.unary(23, m);
                    let mut lod = self.s.m.bin(c, Bin::Mul, l, half);
                    if name.ends_with("_cl") {
                        let x = addr_val(self, first + grads + coords)?;
                        let clamp = to_float(self, x);
                        lod = self.binary(35, lod, clamp);
                    }
                    args.extend(&os);
                    args.push(lod);
                    self.s.op(
                        c,
                        "sampleLevel.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        62,
                        &args,
                    )
                }
                63 => {
                    // With a16 each direction's gradients start a VGPR.
                    if *a16 && spatial % 2 == 1 {
                        return Err("16-bit gradients of a 1D or 3D image");
                    }
                    let mut g = vec![fu; 6];
                    for k in 0..spatial {
                        let x = addr_val(self, first + k)?;
                        g[k] = to_float(self, x);
                        let y = addr_val(self, first + spatial + k)?;
                        g[3 + k] = to_float(self, y);
                    }
                    args.extend(&os);
                    args.extend(g);
                    // `_cl`: the least level of detail, after the coordinates.
                    let clamp = if name.ends_with("_cl") {
                        let x = addr_val(self, first + grads + coords)?;
                        to_float(self, x)
                    } else {
                        fu
                    };
                    args.push(clamp);
                    self.s.op(
                        c,
                        "sampleGrad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        63,
                        &args,
                    )
                }
                64 | 65 => {
                    let reference = addr_val(self, first - 1)?;
                    let reference = self.as_float(reference);
                    args.extend(&os);
                    args.push(reference);
                    let name = if op == 64 {
                        args.push(fu);
                        "sampleCmp.f32"
                    } else {
                        "sampleCmpLevelZero.f32"
                    };
                    self.s
                        .op(c, name, Attr::ReadOnly, self.s.t.res_ret, op, &args)
                }
                62 => {
                    let lod = if name == "image_sample_l" {
                        let x = addr_val(self, coords)?;
                        to_float(self, x)
                    } else {
                        self.s.m.f32(0)
                    };
                    args.extend(&os);
                    args.push(lod);
                    self.s.op(
                        c,
                        "sampleLevel.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        62,
                        &args,
                    )
                }
                _ => {
                    if name == "image_gather4" {
                        return Err("a gather with implicit level");
                    }
                    if dmask.count_ones() != 1 {
                        return Err("a gather of several channels");
                    }
                    if offset_word {
                        let w = addr_val(self, 0)?;
                        for (k, at) in [(0, 26), (1, 18)] {
                            // Bits at..at+5 sign-extended: shift up, then down.
                            let left = self.s.m.i32(at);
                            let t = self.s.m.bin(c, Bin::Shl, w, left);
                            let k26 = self.s.m.i32(26);
                            os[k] = self.s.m.bin(c, Bin::AShr, t, k26);
                        }
                    }
                    args.extend(&os[..2]);
                    args.push(self.s.m.i32(dmask.trailing_zeros()));
                    self.s.op(
                        c,
                        "textureGather.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        73,
                        &args,
                    )
                }
            }
        } else {
            let mut cs = vec![iu; 3];
            for (k, slot) in cs.iter_mut().enumerate().take(coords) {
                *slot = addr_val(self, k)?;
            }
            let mip = if name == "image_load_mip" {
                addr_val(self, coords)?
            } else if uav {
                // Our declarations have four channels.
                self.s.flags |= flags::TYPED_UAV_LOADS;
                iu
            } else {
                self.s.m.i32(0)
            };
            let mut args = vec![h, mip];
            args.extend(&cs);
            args.extend([iu, iu, iu]);
            self.s.op(
                c,
                "textureLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                66,
                &args,
            )
        };
        let Operand::Vector { reg, .. } = *data else {
            return Err("an image result");
        };
        let channels: Vec<u32> = if op == 73 {
            vec![0, 1, 2, 3]
        } else {
            (0..4).filter(|ch| dmask >> ch & 1 != 0).collect()
        };
        let mut xs = Vec::new();
        for ch in channels {
            let x = self.s.m.extract(c, r, ch);
            xs.push(if *d16 {
                // shortcut: d16 results are taken as floats (rounded to
                // nearest even); integer formats would keep 16 bits.
                let i32t = self.s.t.i32;
                self.s
                    .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[x])
            } else {
                self.as_int(x)
            });
        }
        if *d16 {
            // Two channels per VGPR, low half first (an odd last high half 0).
            let k16 = self.s.m.i32(16);
            xs = xs
                .chunks(2)
                .map(|p| match *p {
                    [lo, hi] => {
                        let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                        self.s.m.bin(c, Bin::Or, lo, hi)
                    }
                    _ => p[0],
                })
                .collect();
        }
        for (k, x) in xs.into_iter().enumerate() {
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }

    /// The direction a cube sample's face coordinates stand for: the
    /// console takes s and t in [1, 2] (`v_cubesc`/`v_cubetc` over
    /// `|v_cubema|`, plus 1.5) and the face id (`v_cubeid`), which the
    /// host's cube sampling turns back into the same texel.
    // shortcut: the view is taken as a cube, not a cube array (whose face
    // operand adds 8 × the slice).
    fn cube_direction(&mut self, s: V, t: V, face: V) -> [V; 3] {
        let c = self.cur;
        let k = |l: &mut Self, v: f32| l.s.m.f32(v.to_bits());
        let (two, three) = (k(self, 2.0), k(self, 3.0));
        let u = self.s.m.bin(c, Bin::Mul, s, two);
        let u = self.s.m.bin(c, Bin::Sub, u, three);
        let v = self.s.m.bin(c, Bin::Mul, t, two);
        let v = self.s.m.bin(c, Bin::Sub, v, three);
        let z = self.s.m.f32(0x8000_0000);
        let (nu, nv) = (
            self.s.m.bin(c, Bin::Sub, z, u),
            self.s.m.bin(c, Bin::Sub, z, v),
        );
        let (one, mone) = (k(self, 1.0), k(self, -1.0));
        let f = self.s.m.cast(c, Cast::FpToUi, face, self.s.t.i32);
        // Per face +x, -x, +y, -y, +z, -z.
        let table = [
            [one, mone, u, u, u, nu],
            [nv, nv, one, mone, nv, nv],
            [nu, u, v, nv, one, mone],
        ];
        table.map(|row| {
            let mut r = row[0];
            for (n, &x) in row.iter().enumerate().skip(1) {
                let kn = self.s.m.i32(n as u32);
                let is = self.s.m.cmp(c, pred::EQ, f, kn);
                r = self.s.m.select(c, is, x, r);
            }
            r
        })
    }

    /// `image_gather4_l(_o)`: DXIL gathers only from a view's first level,
    /// so the four texels are loaded from the level. Only what the title's
    /// culling shaders use: an immediate sampler without bias whose border
    /// is transparent black (a texel outside reads as a load outside: 0),
    /// and a level of detail proven whole (which level a fraction picks is
    /// not known; a whole one is that level, clamped to the sampler's and
    /// the view's).
    fn gather_at_level(
        &mut self,
        inst: &Inst,
        h: V,
        sampler: &Source,
        a: &[V],
        lod: Value,
        guard: Option<V>,
    ) -> Result<(), Why> {
        let Fields::Image { data, dmask, .. } = &inst.fields else {
            unreachable!()
        };
        let Source::Constant(w) = sampler else {
            return Err("a gather at a level through a sampler from memory");
        };
        let &[w0, w1, w2, w3, ..] = &w[..] else {
            return Err("a sampler constant");
        };
        let bits = |word: u32, at: u32, n: u32| word >> at & ((1 << n) - 1);
        // Clamp to a border (x, y) of transparent black; no anisotropy,
        // unnormalized or truncated coordinates, reduction or bias; whole
        // level limits.
        if w0 & 0x3F != 0x36
            || w0 & (7 << 9 | 1 << 15 | 1 << 19 | 1 << 27 | 3 << 29) != 0
            || bits(w3, 30, 2) != 0
            || bits(w2, 0, 20) != 0
            || w1 & 0x0F_F0FF != 0
        {
            return Err("a gather at a level through a sampler other than a black border's");
        }
        // No channel enabled: the first.
        let dmask = if *dmask == 0 { 1 } else { *dmask };
        if dmask.count_ones() != 1 {
            return Err("a gather of several channels");
        }
        let mip_filter = bits(w2, 26, 2) != 0;
        if mip_filter && self.whole(lod, &mut HashSet::new()) != Some(true) {
            return Err("a gather at a level that may not be whole");
        }
        let c = self.cur;
        let i32t = self.s.t.i32;
        let dims_t = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
        let zero = self.s.m.i32(0);
        let level = if mip_filter {
            // The sampler's limits, then the view's last level.
            let f = self.as_float(a[a.len() - 1]);
            let lo = self.s.m.f32((bits(w1, 0, 12) as f32 / 256.0).to_bits());
            let hi = self.s.m.f32((bits(w1, 12, 12) as f32 / 256.0).to_bits());
            let f = self.binary(35, f, lo);
            let f = self.binary(36, f, hi);
            let l = self.s.m.cast(c, Cast::FpToSi, f, i32t);
            let d = self
                .s
                .op(c, "getDimensions", Attr::ReadOnly, dims_t, 72, &[h, zero]);
            let levels = self.s.m.extract(c, d, 3);
            let one = self.s.m.i32(1);
            let last = self.s.m.bin(c, Bin::Sub, levels, one);
            let past = self.s.m.cmp(c, pred::SGT, l, last);
            self.s.m.select(c, past, last, l)
        } else {
            zero
        };
        let d = self
            .s
            .op(c, "getDimensions", Attr::ReadOnly, dims_t, 72, &[h, level]);
        let offset = (a.len() == 4).then(|| a[0]);
        let mut first = [zero; 2];
        for (k, at) in [(0, 26), (1, 18)] {
            let size = self.s.m.extract(c, d, k as u32);
            first[k] = self.gather_first(a[a.len() - 3 + k], size);
            if let Some(o) = offset {
                // Bits at..at+5 of the offset word sign-extended.
                let left = self.s.m.i32(at);
                let t = self.s.m.bin(c, Bin::Shl, o, left);
                let k26 = self.s.m.i32(26);
                let o = self.s.m.bin(c, Bin::AShr, t, k26);
                first[k] = self.s.m.bin(c, Bin::Add, first[k], o);
            }
        }
        // Texels (x, y + 1), (x + 1, y + 1), (x + 1, y), (x, y).
        let one = self.s.m.i32(1);
        let next = first.map(|f| self.s.m.bin(c, Bin::Add, f, one));
        let iu = self.s.m.undef(i32t);
        let Operand::Vector { reg, .. } = *data else {
            return Err("an image result");
        };
        for (k, (x, y)) in [
            (first[0], next[1]),
            (next[0], next[1]),
            (next[0], first[1]),
            (first[0], first[1]),
        ]
        .into_iter()
        .enumerate()
        {
            let args = [h, level, x, y, iu, iu, iu, iu];
            let r = self.s.op(
                c,
                "textureLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                66,
                &args,
            );
            let v = self.s.m.extract(c, r, dmask.trailing_zeros());
            let v = self.as_int(v);
            self.write(inst, Reg::V(reg + k as u8), v, guard);
        }
        Ok(())
    }

    /// The first texel of a gather's footprint along an axis of `size`
    /// texels at coordinate `u` (bits of a float): floor(u × size − 0.5)
    /// with the product exact and rounded to 1/256 of a texel, half up, as
    /// the hardware does (`gather_level_tests`), i.e.
    /// (floor(u × size × 512) − 255) >> 9, in 64-bit integers. NaN is 0;
    /// beyond ±2 every texel of the footprint is outside, as at ±2.
    fn gather_first(&mut self, u: V, size: V) -> V {
        let c = self.cur;
        let f = self.as_float(u);
        let num = self.s.m.cmp(c, pred::OEQ, f, f);
        let fz = self.s.m.f32(0);
        let f = self.s.m.select(c, num, f, fz);
        let (lo, hi) = (
            self.s.m.f32((-2.0f32).to_bits()),
            self.s.m.f32(2.0f32.to_bits()),
        );
        let f = self.binary(35, f, lo);
        let f = self.binary(36, f, hi);
        let x = self.as_int(f);
        // u = m × 2^(e − 150), e at least 1 (denormals).
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let (k23, kff) = (k(self, 23), k(self, 0xFF));
        let e = self.s.m.bin(c, Bin::LShr, x, k23);
        let e = self.s.m.bin(c, Bin::And, e, kff);
        let (kmant, zero, one) = (k(self, 0x7F_FFFF), k(self, 0), k(self, 1));
        let m = self.s.m.bin(c, Bin::And, x, kmant);
        let normal = self.s.m.cmp(c, pred::NE, e, zero);
        let implicit = k(self, 0x80_0000);
        let bit = self.s.m.select(c, normal, implicit, zero);
        let m = self.s.m.bin(c, Bin::Or, m, bit);
        let e = self.s.m.select(c, normal, e, one);
        // u × size × 512 = m × size × 2^(e − 141); |u| ≤ 2 keeps e ≤ 128.
        let k141 = k(self, 141);
        let shift = self.s.m.bin(c, Bin::Sub, k141, e);
        let k63 = k(self, 63);
        let far = self.s.m.cmp(c, pred::SGT, shift, k63);
        let shift = self.s.m.select(c, far, k63, shift);
        let i64t = self.s.m.int(64);
        self.s.flags |= flags::INT64;
        let m = self.s.m.cast(c, Cast::ZExt, m, i64t);
        let n = self.s.m.cast(c, Cast::ZExt, size, i64t);
        let p = self.s.m.bin(c, Bin::Mul, m, n);
        let z64 = self.s.m.int_const(i64t, 0);
        let neg = self.s.m.bin(c, Bin::Sub, z64, p);
        let negative = self.s.m.cmp(c, pred::SLT, x, zero);
        let p = self.s.m.select(c, negative, neg, p);
        let shift = self.s.m.cast(c, Cast::ZExt, shift, i64t);
        let g = self.s.m.bin(c, Bin::AShr, p, shift);
        let k255 = self.s.m.int_const(i64t, 255);
        let g = self.s.m.bin(c, Bin::Sub, g, k255);
        let k9 = self.s.m.int_const(i64t, 9);
        let g = self.s.m.bin(c, Bin::AShr, g, k9);
        self.s.m.cast(c, Cast::Trunc, g, self.s.t.i32)
    }

    /// Whether `v` holds, as a float, a whole number or an infinity in
    /// every lane: `Some(true)` and never NaN, `Some(false)` or NaN, `None`
    /// perhaps a fraction. Follows moves, selects, minimum and maximum,
    /// sums with constants, rounding and conversions from integers (a level
    /// of detail made with `ceil`), through phis and guarded writes' kept
    /// values.
    fn whole(&self, v: Value, seen: &mut HashSet<Value>) -> Option<bool> {
        if !seen.insert(v) {
            // Around a loop: what enters it decides.
            return Some(true);
        }
        let p = self.x.p;
        if let Some(&(b, i)) = self.phi_at.get(&v) {
            let mut never_nan = true;
            for &arg in &p.blocks[b].phis[i].args {
                never_nan &= self.whole(arg, seen)?;
            }
            return Some(never_nan);
        }
        let Some(&b) = self.def_block.get(&v) else {
            // A VGPR no one set reads zero.
            let vgpr = p
                .inputs
                .iter()
                .any(|&(r, x)| x == v && matches!(r, Reg::V(_)));
            let io = self.x.io.inputs.iter().any(|i| i.0 == v);
            return (vgpr && !io).then_some(true);
        };
        let (i, inst) = p.blocks[b]
            .insts
            .iter()
            .enumerate()
            .find(|(_, i)| i.writes.iter().any(|w| w.1 == v))?;
        let Fields::Valu(f) = &inst.fields else {
            return None;
        };
        if f.omod != 0 || f.sdwa.is_some() || f.dpp.is_some() {
            return None;
        }
        let mut never_nan = true;
        if self.x.st.guards[b][i].is_some() {
            let Some(Operand::Vector { reg, .. }) = f.dst else {
                return None;
            };
            never_nan = self.whole(inst.read(Reg::V(reg))?, seen)?;
        }
        // Per source: (never NaN, a finite constant).
        let mut src = |k: usize| -> Option<(bool, bool)> {
            let x = match *f.src.get(k)? {
                Operand::Vector { reg, .. } => {
                    return Some((self.whole(inst.read(Reg::V(reg))?, seen)?, false));
                }
                Operand::Const(c) => crate::resource::inline(c)?,
                Operand::Literal(x) => x,
                _ => return None,
            };
            let x = f32::from_bits(x);
            (x.is_infinite() || x.fract() == 0.0).then_some((true, x.is_finite()))
        };
        let r = match inst.name() {
            "v_cvt_f32_i32" | "v_cvt_f32_u32" => true,
            "v_floor_f32" | "v_ceil_f32" | "v_trunc_f32" | "v_rndne_f32" => false,
            "v_mov_b32" => src(0)?.0,
            "v_cndmask_b32" => src(0)?.0 & src(1)?.0,
            "v_max_f32" | "v_min_f32" => src(0)?.0 | src(1)?.0,
            "v_max3_f32" | "v_min3_f32" => src(0)?.0 | src(1)?.0 | src(2)?.0,
            "v_add_f32" | "v_sub_f32" | "v_subrev_f32" => {
                // Infinities of both signs make NaN.
                let (x, y) = (src(0)?, src(1)?);
                x.0 && y.0 && (x.1 || y.1)
            }
            _ => return None,
        };
        Some(r && never_nan)
    }

    fn buffer(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Buffer {
            data,
            addr,
            soffset,
            offset,
            offen,
            idxen,
            format,
            lds,
            tfe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let name = inst.name();
        if name == "buffer_gl0_inv" || name == "buffer_gl1_inv" {
            // Console cache coherence: gl0 writes back and invalidates the
            // per-CU shader L0, gl1 invalidates GL1, so one wavefront's
            // writes become visible to others. D3D12 already guarantees UAV
            // coherence across wavefronts and dispatches, DXIL has no
            // equivalent instruction, and this project's host-visible
            // writeback comes from command-stream GpuWrite ranges rather
            // than shader cache hints. Ordering is kept by the surrounding
            // s_waitcnt/s_barrier, which the translator does lower.
            return Ok(());
        }
        if *lds || *tfe {
            return Err("a buffer access to LDS or with tfe");
        }
        if format.is_some() && !name.starts_with("tbuffer_load_format_") {
            return Err(name);
        }
        let site = (b, i);
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let store = name.starts_with("buffer_store_");
        // DXIL atomic operations: add, and, or, xor, signed and unsigned
        // min and max, exchange; compare-exchange apart.
        let atomic = match name.strip_prefix("buffer_atomic_") {
            None => None,
            Some("add") => Some(0),
            Some("and") => Some(1),
            Some("or") => Some(2),
            Some("xor") => Some(3),
            Some("smin") => Some(4),
            Some("smax") => Some(5),
            Some("umin") => Some(6),
            Some("umax") => Some(7),
            Some("swap") => Some(8),
            Some("cmpswap") => Some(u32::MAX),
            Some(_) => return Err(name),
        };
        // d16: two 16-bit channels per VGPR, low half first (buffer and
        // tbuffer formats alike).
        let d16 = name.contains("_format_d16_");
        let plain = name.replace("_format_d16_", "_format_");
        let (typed, count) = match plain
            .strip_prefix("buffer_load_")
            .or(plain.strip_prefix("buffer_store_"))
            .or(plain.strip_prefix("tbuffer_load_"))
        {
            _ if atomic.is_some() => (false, 1),
            Some("dword") => (false, 1),
            Some("dwordx2") => (false, 2),
            Some("dwordx3") => (false, 3),
            Some("dwordx4") => (false, 4),
            Some("format_x") => (true, 1),
            Some("format_xy") => (true, 2),
            Some("format_xyz") => (true, 3),
            Some("format_xyzw") => (true, 4),
            Some("ubyte" | "sbyte" | "ushort" | "sshort") if !store => (false, 1),
            Some("byte" | "short") if store => (false, 1),
            _ => return Err(name),
        };
        // Loads or stores of a byte or a half: its width and whether signed.
        let sub = match plain
            .strip_prefix("buffer_load_")
            .or(plain.strip_prefix("buffer_store_"))
        {
            Some("ubyte" | "byte") => Some((8, false)),
            Some("sbyte") => Some((8, true)),
            Some("ushort" | "short") => Some((16, false)),
            Some("sshort") => Some((16, true)),
            _ => None,
        };
        let Operand::Vector { reg: areg, .. } = *addr else {
            return Err("a buffer address");
        };
        let vaddr = |l: &mut Self, k: u8| -> Result<V, Why> {
            let v = inst.read(Reg::V(areg + k)).ok_or("a buffer address")?;
            l.source(v)
        };
        let index = if *idxen { Some(vaddr(self, 0)?) } else { None };
        let mut off = self.s.m.i32(u32::from(*offset));
        if *offen {
            let o = vaddr(self, u8::from(*idxen))?;
            off = self.s.m.bin(c, Bin::Add, off, o);
        }
        if !matches!(
            soffset,
            Operand::Scalar { code: NULL, .. } | Operand::Const(128)
        ) {
            let so = self.operand(inst, soffset)?;
            off = self.s.m.bin(c, Bin::Add, off, so);
        }
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked buffer")?;
        let used = self.used_kind(src);
        // Sixteen bits a channel through a view of a table: the bits go as
        // they are, as unsigned integers, and the host is told, which shows
        // the shader a 16-bit float view there as its unsigned twin
        // ([`super::HostShader::half_views`]).
        // A format load of whole dwords whose every use reads one half of a
        // result as a half float moves 16-bit integers too (the head
        // program's reads of the face network's output), where every
        // access through its view does ([`Self::places_read_as_halves`]).
        // shortcut: mixed or dynamically aliased slots keep legacy float conversion;
        // exact UINT/SINT and NaN payloads need format-aware lowering or separate indices.
        let place = typed.then(|| self.table_place(src)).flatten().filter(|p| {
            self.halves_only.contains(p) && (d16 || self.format_load_read_as_halves(inst))
        });
        let halves = place.is_some();
        if let Some(place) = place {
            self.half_views.insert(place);
        }
        let (base, extra) = self.built_offset(src)?;
        if base != 0 {
            let b = self.s.m.i32(base);
            off = self.s.m.bin(c, Bin::Add, off, b);
        }
        if let Some(x) = extra {
            off = self.s.m.bin(c, Bin::Add, off, x);
        }
        // A descriptor built around a root view has the stride the code puts
        // in dword 1 (bits 16-29) and no record limit (dword 2 is -1): an
        // index is a byte offset into the raw root view.
        let index = match (src, index) {
            (
                Source::Built {
                    address,
                    bits,
                    words,
                },
                Some(ix),
            ) if address.param < self.x.native.layout.len() && !typed => {
                if bits >> 30 != 0 || words[0] != u32::MAX {
                    return Err("a root view indexed with swizzling or a record limit");
                }
                let stride = self.s.m.i32(bits >> 16 & 0x3FFF);
                let byte = self.s.m.bin(c, Bin::Mul, ix, stride);
                off = self.s.m.bin(c, Bin::Add, off, byte);
                None
            }
            (_, ix) => ix,
        };
        if used.is_some_and(|u| u.kind == ResourceType::Cbv) && !typed {
            // A constant buffer read per lane (a dynamically indexed array).
            // shortcut: the console's CBV descriptor is taken to have a 16-byte
            // stride, as the vector loads of whole rows suggest.
            let table = used.is_some_and(|u| u.root == RootType::Table);
            let byte = match index {
                Some(i) => {
                    let sixteen = self.s.m.i32(16);
                    let rows = self.s.m.bin(c, Bin::Mul, i, sixteen);
                    self.s.m.bin(c, Bin::Add, rows, off)
                }
                None => off,
            };
            if table {
                // A table's constant buffer is a raw view (scalar loads above).
                let Some(Operand::Vector { reg, .. }) = *data else {
                    return Err("a buffer result");
                };
                let (h, byte) = self.raw_view(site, src, byte, 4 * count, (11, 0), false, guard)?;
                for k in 0..count {
                    let four = self.s.m.i32(4 * k);
                    let a = self.s.m.bin(c, Bin::Add, byte, four);
                    let (mask, al) = (self.s.m.i8(1), self.s.m.i32(4));
                    let u = self.s.m.undef(self.s.t.i32);
                    let r = self.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, a, u, mask, al],
                    );
                    let x = self.s.m.extract(c, r, 0);
                    let x = self.as_int(x);
                    self.write(inst, Reg::V(reg + k as u8), x, guard);
                }
                return Ok(());
            }
            let h = self.handle(site, src, false, (13, 0x1_0000))?;
            let Some(Operand::Vector { reg, .. }) = *data else {
                return Err("a buffer result");
            };
            for k in 0..count {
                let four = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, byte, four);
                let x = self.cbuffer_dword(h, a);
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        // A heap-indexed buffer read by element has no used-resource entry,
        // so no stride: its bytes are addressed as the hardware does, the
        // element times the view's stride (the host's table) plus the offset.
        let index = match index {
            Some(i) if used.is_none() && !typed => {
                let stride = self.view_stride(src)?;
                let rows = self.s.m.bin(c, Bin::Mul, i, stride);
                off = self.s.m.bin(c, Bin::Add, rows, off);
                None
            }
            index => index,
        };
        let uav = store || atomic.is_some() || used.is_some_and(|u| u.kind == ResourceType::Uav);
        let u = if uav { 0x1000 } else { 0 };
        let iu = self.s.m.undef(self.s.t.i32);
        if let Some(op) = atomic {
            let d = data.as_ref().ok_or("buffer atomic data")?;
            let vals = self.vgprs(inst, d, if op == u32::MAX { 2 } else { 1 })?;
            let (h, coords, byte) = match index {
                Some(index) => {
                    let stride = used.map_or(0, |u| u.stride);
                    if stride < 4 || !stride.is_multiple_of(4) {
                        return Err("a structured buffer of unknown stride");
                    }
                    let h = self.handle(site, src, false, (12 | u, stride))?;
                    let s = self.s.m.i32(stride);
                    let row = self.s.m.bin(self.cur, Bin::Mul, index, s);
                    let byte = self.s.m.bin(self.cur, Bin::Add, row, off);
                    (h, [index, off, iu], byte)
                }
                None => {
                    let (h, off) = self.raw_view(site, src, off, 4, (11 | u, 0), true, guard)?;
                    (h, [off, iu, iu], off)
                }
            };
            let Operand::Vector { reg, .. } = *d else {
                return Err("buffer atomic data");
            };
            let returns = matches!(inst.fields, Fields::Buffer { cache, .. } if cache.glc);
            let old = match inst.read(Reg::V(reg)) {
                Some(v) if returns => self.i32_of(v)?,
                _ => self.s.m.i32(0),
            };
            let at = self.begin_guarded(guard);
            let i32t = self.s.t.i32;
            let r = if op == u32::MAX {
                // cmpswap: data is (new, compare).
                let mut args = vec![h];
                args.extend(coords);
                args.extend([vals[1], vals[0]]);
                self.s.op(
                    self.cur,
                    "atomicCompareExchange.i32",
                    Attr::NoUnwind,
                    i32t,
                    79,
                    &args,
                )
            } else {
                let mut args = vec![h, self.s.m.i32(op)];
                args.extend(coords);
                args.push(vals[0]);
                self.s
                    .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args)
            };
            self.mark_view_write(h, byte, 4, None);
            let r = self.end_guarded(at, &[(r, old)])[0];
            if returns {
                self.write(inst, Reg::V(reg), r, None);
            }
            return Ok(());
        }
        if let (true, Some((width, _))) = (store, sub) {
            // The field of the dword holding it, by two atomics on that dword
            // (clear, then set): other lanes' fields in it are left alone.
            // shortcut: the field is taken not to cross a dword.
            if index.is_some() {
                return Err("a byte or half buffer store by index");
            }
            let d = data.as_ref().ok_or("buffer store data")?;
            let x = self.vgprs(inst, d, 1)?[0];
            let (not3, three) = (self.s.m.i32(!3), self.s.m.i32(3));
            let at = self.s.m.bin(c, Bin::And, off, not3);
            let byte = self.s.m.bin(c, Bin::And, off, three);
            let sh = self.s.m.bin(c, Bin::Shl, byte, three);
            let field = self.s.m.i32(if width == 8 { 0xFF } else { 0xFFFF });
            let x = self.s.m.bin(c, Bin::And, x, field);
            let x = self.s.m.bin(c, Bin::Shl, x, sh);
            let mask = self.s.m.bin(c, Bin::Shl, field, sh);
            let all = self.s.m.i32(u32::MAX);
            let keep = self.s.m.bin(c, Bin::Xor, mask, all);
            let (h, at) = self.raw_view(site, src, at, 4, (11 | 0x1000, 0), true, guard)?;
            let begin = self.begin_guarded(guard);
            let i32t = self.s.t.i32;
            for (op, v) in [(1, keep), (2, x)] {
                let args = [h, self.s.m.i32(op), at, iu, iu, v];
                self.s
                    .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args);
            }
            self.mark_view_write(h, at, 4, None);
            self.end_guarded(begin, &[]);
            return Ok(());
        }
        if store {
            let regs = if d16 { count.div_ceil(2) } else { count };
            let mut vals =
                self.vgprs(inst, data.as_ref().ok_or("buffer store data")?, regs as u8)?;
            let typed_h = if typed {
                Some(self.handle(
                    site,
                    src,
                    false,
                    (10 | u, if halves { 0x405 } else { 0x409 }),
                )?)
            } else {
                None
            };
            if d16 {
                // d16 data is f16 for float and normalized formats, widened
                // for the 32-bit float store; for integer formats (the view's
                // kind, from the view-stride table) the 16-bit integer, zero-
                // or sign-extended (stored through the float-typed handle as
                // loads read: see there).
                let integer = if halves {
                    None
                } else {
                    typed_h.and_then(|h| self.integer_view(h))
                };
                let c = self.cur;
                let mut wide = Vec::new();
                for k in 0..count {
                    let mut x = vals[(k / 2) as usize];
                    if k % 2 == 1 {
                        let k16 = self.s.m.i32(16);
                        x = self.s.m.bin(c, Bin::LShr, x, k16);
                    }
                    if halves {
                        let low = self.s.m.i32(0xFFFF);
                        wide.push(self.s.m.bin(c, Bin::And, x, low));
                        continue;
                    }
                    let f32t = self.s.t.f32;
                    let f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x]);
                    let mut w = self.as_int(f);
                    if let Some((int, signed)) = integer {
                        let (k16, low) = (self.s.m.i32(16), self.s.m.i32(0xFFFF));
                        let zero_ext = self.s.m.bin(c, Bin::And, x, low);
                        let up = self.s.m.bin(c, Bin::Shl, x, k16);
                        let sign_ext = self.s.m.bin(c, Bin::AShr, up, k16);
                        let n = self.s.m.select(c, signed, sign_ext, zero_ext);
                        w = self.s.m.select(c, int, n, w);
                    }
                    wide.push(w);
                }
                vals = wide;
            }
            let mut args = Vec::new();
            // Where the write starts in the view, and how many bytes it may
            // cover (for its marks).
            let (byte, bytes);
            let (op, name) = if typed {
                // shortcut: typed stores are taken to write all four channels
                // of a float format.
                if count != 4 || *offen || *offset != 0 {
                    return Err("a typed buffer store of some channels or at an offset");
                }
                let h = typed_h.ok_or("a typed buffer store")?;
                let element = index.unwrap_or(self.s.m.i32(0));
                args.push(h);
                args.extend([element, iu]);
                for x in vals {
                    args.push(if halves { x } else { self.as_float(x) });
                }
                // The element's size is the view's stride (16 bytes at most).
                (byte, bytes) = match self.s.view_index.get(&h).copied() {
                    Some(at) if self.x.view_marks => {
                        let stride = self.stride_at(at);
                        (self.s.m.bin(self.cur, Bin::Mul, element, stride), 16)
                    }
                    _ => (element, 16),
                };
                (
                    69,
                    if halves {
                        "bufferStore.i32"
                    } else {
                        "bufferStore.f32"
                    },
                )
            } else {
                match index {
                    Some(index) => {
                        let stride = used.map_or(0, |u| u.stride);
                        if stride < 4 || !stride.is_multiple_of(4) {
                            return Err("a structured buffer of unknown stride");
                        }
                        args.push(self.handle(site, src, false, (12 | u, stride))?);
                        args.extend([index, off]);
                        let s = self.s.m.i32(stride);
                        let row = self.s.m.bin(self.cur, Bin::Mul, index, s);
                        (byte, bytes) = (self.s.m.bin(self.cur, Bin::Add, row, off), 4 * count);
                    }
                    None => {
                        let n = 4 * count;
                        let (h, off) =
                            self.raw_view(site, src, off, n, (11 | u, 0), true, guard)?;
                        args.extend([h, off, iu]);
                        (byte, bytes) = (off, n);
                    }
                }
                args.extend(&vals);
                args.resize(3 + 4, iu);
                (140, "rawBufferStore.i32")
            };
            args.push(self.s.m.i8(if typed { 0xF } else { (1u8 << count) - 1 }));
            if !typed {
                args.push(self.s.m.i32(4));
            }
            let at = self.begin_guarded(guard);
            let void = self.s.t.void;
            self.s.op(self.cur, name, Attr::NoUnwind, void, op, &args);
            self.mark_view_write(args[0], byte, bytes, None);
            self.end_guarded(at, &[]);
            return Ok(());
        }
        if let Some(f) = *format {
            if index.is_some() {
                return Err("a typed-format load by index");
            }
            let (h, off) = self.raw_view(site, src, off, 4 * count, (11 | u, 0), false, guard)?;
            let Some(Operand::Vector { reg, .. }) = *data else {
                return Err("a buffer result");
            };
            let mut xs = self.format_load(h, off, f, count)?;
            if d16 {
                xs = self.d16_channels(f, xs)?;
            }
            for (k, x) in xs.into_iter().enumerate() {
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        if let Some((width, signed)) = sub {
            // The dword holding the field, as `global_load_ubyte` and kin.
            // shortcut: the field is taken not to cross a dword.
            if index.is_some() {
                return Err("a byte or half buffer load by index");
            }
            let (not3, three) = (self.s.m.i32(!3), self.s.m.i32(3));
            let at = self.s.m.bin(c, Bin::And, off, not3);
            let (h, at) = self.raw_view(site, src, at, 4, (11 | u, 0), false, guard)?;
            let c = self.cur;
            let (mask, align) = (self.s.m.i8(1), self.s.m.i32(4));
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[h, at, iu, mask, align],
            );
            let w = self.s.m.extract(c, r, 0);
            let w = self.as_int(w);
            let byte = self.s.m.bin(c, Bin::And, off, three);
            let sh = self.s.m.bin(c, Bin::Shl, byte, three);
            let x = self.s.m.bin(c, Bin::LShr, w, sh);
            let left = self.s.m.i32(32 - width);
            let x = self.s.m.bin(c, Bin::Shl, x, left);
            let x = self
                .s
                .m
                .bin(c, if signed { Bin::AShr } else { Bin::LShr }, x, left);
            let Some(Operand::Vector { reg, .. }) = *data else {
                return Err("a buffer result");
            };
            self.write(inst, Reg::V(reg), x, guard);
            return Ok(());
        }
        let mut typed_h = None;
        let r = if typed {
            if *offen || matches!(off, V::Inst(_)) || *offset != 0 {
                return Err("a typed buffer load at an offset");
            }
            if uav {
                self.s.flags |= flags::TYPED_UAV_LOADS;
            }
            let h = self.handle(
                site,
                src,
                false,
                (10 | u, if halves { 0x405 } else { 0x409 }),
            )?;
            typed_h = Some(h);
            let index = index.unwrap_or(self.s.m.i32(0));
            let ret = if halves {
                self.s
                    .m
                    .named_struct("dx.types.ResRet.i32", vec![self.s.t.i32; 5])
            } else {
                self.s.t.res_ret
            };
            self.s.op(
                c,
                if halves {
                    "bufferLoad.i32"
                } else {
                    "bufferLoad.f32"
                },
                Attr::ReadOnly,
                ret,
                68,
                &[h, index, iu],
            )
        } else {
            let mask = self.s.m.i8((1u8 << count) - 1);
            let align = self.s.m.i32(4);
            match index {
                Some(index) => {
                    let stride = used.map_or(0, |u| u.stride);
                    if stride < 4 || !stride.is_multiple_of(4) {
                        return Err("a structured buffer of unknown stride");
                    }
                    let h = self.handle(site, src, false, (12 | u, stride))?;
                    self.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, index, off, mask, align],
                    )
                }
                None => {
                    let bytes = 4 * count;
                    let (h, off) =
                        self.raw_view(site, src, off, bytes, (11 | u, 0), false, guard)?;
                    self.s.op(
                        self.cur,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, off, iu, mask, align],
                    )
                }
            }
        };
        let c = self.cur;
        let Some(Operand::Vector { reg, .. }) = *data else {
            return Err("a buffer result");
        };
        if d16 {
            // Two 16-bit channels per VGPR (an odd last high half 0): f16 for
            // float and normalized formats; for integer formats (the view's
            // kind, from the view-stride table) the integer's low 16 bits.
            // shortcut: an integer view is read and written through the
            // float-typed handle, so its channels' bits travel as the driver
            // passes a mismatched component type (unchanged on the RX 6700
            // XT and WARP, `stride_tests`); an integer-typed second handle
            // for the slot would not rely on it.
            let integer = if halves {
                None
            } else {
                typed_h.and_then(|h| self.integer_view(h))
            };
            let c = self.cur;
            let k16 = self.s.m.i32(16);
            let i32t = self.s.t.i32;
            for k in 0..count.div_ceil(2) {
                let mut x = None;
                for ch in (2 * k..(2 * k + 2).min(count)).rev() {
                    let f = self.s.m.extract(c, r, ch);
                    let mut h = if halves {
                        let low = self.s.m.i32(0xFFFF);
                        self.s.m.bin(c, Bin::And, f, low)
                    } else {
                        self.s
                            .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
                    };
                    if let Some((int, _)) = integer {
                        let bits = self.as_int(f);
                        let low = self.s.m.i32(0xFFFF);
                        let low = self.s.m.bin(c, Bin::And, bits, low);
                        h = self.s.m.select(c, int, low, h);
                    }
                    x = Some(match x {
                        Some(hi) => {
                            let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                            self.s.m.bin(c, Bin::Or, h, hi)
                        }
                        None => h,
                    });
                }
                self.write(inst, Reg::V(reg + k as u8), x.expect("a channel"), guard);
            }
            return Ok(());
        }
        for k in 0..count {
            let x = self.s.m.extract(c, r, k);
            let x = if halves { x } else { self.as_int(x) };
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }

    fn export(&mut self, b: usize, i: usize) -> Result<(), Why> {
        let e = *self
            .exports
            .get(&(b, i))
            .ok_or("an export I/O raising did not see")?;
        if e.compressed {
            return Err("a compressed export");
        }
        let guard = self.guard(b, i)?;
        let mesh = self.s.mesh.is_some();
        let c = self.cur;
        // A pixel export whose lanes outside its guard the `done` export
        // after it discards: what those lanes write is lost, so it stores in
        // every lane.
        if let Some(g) = guard
            && !mesh
            && !(self.s.pixel && !e.done && self.discarded_at_done(b, i))
        {
            if !(e.done && self.s.pixel) {
                return Err("a conditional export");
            }
            let t = self.s.m.bool(true);
            let kill = self.s.m.bin(c, Bin::Xor, g, t);
            self.s
                .op(c, "discard", Attr::NoUnwind, self.s.t.void, 82, &[kill]);
        }
        if mesh && e.target == Target::Primitive {
            return self.emit_indices(e, guard);
        }
        let param_row = |usage: Usage, row: u8| match usage {
            // The 2020 compiler leaves a mesh program's first entry zeroed
            // (usage "position"): its row is the one no other entry names.
            Usage::Other(0) if mesh => self.unnamed_row(),
            _ => Some(row),
        };
        // (output element, channel) per exported channel; per-primitive
        // outputs are elements of `prims`.
        let mut stores = Vec::new();
        let primitive = matches!(e.target, Target::PrimitiveParam { .. });
        let outputs = if primitive {
            &self.s.prims
        } else {
            &self.s.outputs
        };
        let by_kind = |kind: u8| outputs.iter().position(|o| o.kind == kind);
        // 16-bit vertex outputs: the halves each dword carries, as (output
        // element, channel, high half) per channel.
        let halves = match e.target {
            Target::Param { usage, row } => {
                param_row(usage, row).and_then(|r| self.half_outputs.get(&r).map(|&h| (r, h)))
            }
            _ => None,
        };
        if let Some((row, pack2)) = halves {
            for ch in 0..4u8 {
                let Some(value) = e.values[usize::from(ch)] else {
                    continue;
                };
                for high in [false, true] {
                    let h = 2 * ch + u8::from(high);
                    let src = if pack2 {
                        packed_half(outputs, row, h)
                    } else {
                        (!high).then_some((row, ch))
                    };
                    let found = src.and_then(|(r, c)| {
                        outputs.iter().position(|o| {
                            o.row == Some(u32::from(r)) && c >= o.col && c < o.col + o.cols
                        })
                    });
                    if let Some(id) = found {
                        let col = src.expect("found").1 - outputs[id].col;
                        stores.push((id, col, value, Some(high)));
                    }
                }
            }
        }
        for ch in 0..4u8 {
            let Some(value) = e.values[usize::from(ch)] else {
                continue;
            };
            if halves.is_some() {
                break;
            }
            let found = match e.target {
                Target::Color(n) => outputs
                    .iter()
                    .position(|o| o.kind == 16 && o.row == Some(u32::from(n)))
                    .map(|id| (id, ch)),
                Target::Position(0) => by_kind(3).map(|id| (id, ch)),
                Target::Depth => match ch {
                    0 => by_kind(17)
                        .or(by_kind(18))
                        .or(by_kind(19))
                        .map(|id| (id, 0)),
                    1 => by_kind(20).map(|id| (id, 0)),
                    2 => by_kind(14).map(|id| (id, 0)),
                    _ => None,
                },
                Target::Param { usage, row } | Target::PrimitiveParam { usage, row } => {
                    let row = param_row(usage, row);
                    outputs
                        .iter()
                        .position(|o| {
                            row.is_some_and(|r| o.row == Some(u32::from(r)))
                                && ch >= o.col
                                && ch < o.col + o.cols
                        })
                        .map(|id| (id, ch - outputs[id].col))
                }
                Target::Null | Target::Unread => None,
                Target::Position(n @ 1..=2) => {
                    // shortcut: without the misc vector (point size, render
                    // target or viewport index) pos1 and pos2 carry the clip
                    // then cull distances, in semantic order.
                    if outputs.iter().any(|o| matches!(o.kind, 4 | 5)) {
                        return Err("render-target or viewport index exports");
                    }
                    let mut dists: Vec<usize> = (0..outputs.len())
                        .filter(|&i| matches!(outputs[i].kind, 6 | 7))
                        .collect();
                    dists.sort_by_key(|&i| (outputs[i].kind, outputs[i].index));
                    let flat: Vec<(usize, u8)> = dists
                        .iter()
                        .flat_map(|&i| (0..outputs[i].cols).map(move |c| (i, c)))
                        .collect();
                    // Channels past the distances the signature declares are
                    // ones the clipper does not enable (`PA_CL_VS_OUT_CNTL`).
                    let k = usize::from(n - 1) * 4 + usize::from(ch);
                    flat.get(k).copied()
                }
                Target::Position(_) => return Err("clip, cull or point-size exports"),
                Target::Primitive => return Err("a primitive export"),
            };
            if let Some((id, col)) = found {
                stores.push((id, col, value, None));
            }
        }
        fn elements<'l>(l: &'l mut Lower, primitive: bool) -> &'l mut Vec<SigElement> {
            if primitive {
                &mut l.s.prims
            } else {
                &mut l.s.outputs
            }
        }
        if stores
            .iter()
            .any(|s| s.3.is_none() && elements(self, primitive)[s.0].half)
        {
            return Err("a 16-bit output the stage data does not mark");
        }
        // A mesh lane writes its own vertex or primitive, where it exports.
        let at = if mesh {
            self.begin_guarded(guard)
        } else {
            None
        };
        let (name, opcode) = match (mesh, primitive) {
            (false, _) => ("storeOutput", 5),
            (true, false) => ("storeVertexOutput", 171),
            (true, true) => ("storePrimitiveOutput", 172),
        };
        for (id, col, value, half) in stores {
            let mut x = self.i32_of(value)?;
            if let Some(high) = half {
                // An f16 half, declared a 32-bit float on the host.
                let c = self.cur;
                if high {
                    let k16 = self.s.m.i32(16);
                    x = self.s.m.bin(c, Bin::LShr, x, k16);
                }
                let f32t = self.s.t.f32;
                let f = self
                    .s
                    .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x]);
                x = self.as_int(f);
            }
            let el = &mut elements(self, primitive)[id];
            el.usage |= 1 << col;
            let (suffix, x) = match el.comp {
                9 if half.is_some() => (".f32", self.as_float(x)),
                9 => {
                    let f = match self.floats.get(&value) {
                        Some(&f) => f,
                        None => self.as_float(x),
                    };
                    (".f32", f)
                }
                _ => (".i32", x),
            };
            let mut args = vec![
                self.s.m.i32(id as u32),
                self.s.m.i32(0),
                self.s.m.i8(col),
                x,
            ];
            if mesh {
                args.push(self.mesh_thread());
            }
            let void = self.s.t.void;
            self.s.op(
                self.cur,
                &format!("{name}{suffix}"),
                Attr::NoUnwind,
                void,
                opcode,
                &args,
            );
        }
        self.end_guarded(at, &[]);
        Ok(())
    }

    /// Mesh: `exp prim` as the lane's primitive. Each corner takes ten
    /// bits: a 9-bit vertex index and, above it, its edge flag (bits 9, 19
    /// and 29), which only the console's edge-flag rasterisation reads and
    /// which is no part of the index. Bit 31 culls it (all indices 0: no
    /// area).
    fn emit_indices(&mut self, e: &Export, guard: Option<V>) -> Result<(), Why> {
        let v = e.values[0].ok_or("a primitive export without its indices")?;
        let x = self.i32_of(v)?;
        let at = self.begin_guarded(guard);
        let c = self.cur;
        let z = self.s.m.i32(0);
        let culled = self.s.m.cmp(c, pred::SLT, x, z);
        let mut args = vec![self.mesh_thread()];
        for k in 0..3 {
            let (shift, mask) = (self.s.m.i32(10 * k), self.s.m.i32(0x1FF));
            let i = self.s.m.bin(c, Bin::LShr, x, shift);
            let i = self.s.m.bin(c, Bin::And, i, mask);
            args.push(self.s.m.select(c, culled, z, i));
        }
        let void = self.s.t.void;
        self.s
            .op(c, "emitIndices", Attr::NoUnwind, void, 169, &args);
        self.end_guarded(at, &[]);
        Ok(())
    }

    /// The per-vertex arbitrary row no mesh output entry names but one
    /// (the 2020 compiler's zeroed entry).
    fn unnamed_row(&self) -> Option<u8> {
        let StageIo::Ms(ms) = &self.x.native.io else {
            return None;
        };
        let named = |r: u32| {
            ms.outputs
                .iter()
                .any(|o| o.param & 0x80 == 0 && o.usage != Usage::Other(0) && u32::from(o.row) == r)
        };
        let free: std::collections::BTreeSet<u32> = self
            .s
            .outputs
            .iter()
            .filter(|o| o.kind == 0)
            .filter_map(|o| o.row)
            .filter(|&r| !named(r))
            .collect();
        match free.into_iter().collect::<Vec<_>>()[..] {
            [r] => u8::try_from(r).ok(),
            _ => None,
        }
    }

    fn find_cap_follows(&mut self, order: &[usize]) {
        let p = self.x.p;
        let idom = crate::ir::idoms(&p.blocks, order, &self.rpo);
        let dominates = |h: usize, mut b: usize| {
            while b != h {
                let d = idom[b];
                if d == b || d == usize::MAX {
                    return false;
                }
                b = d;
            }
            true
        };
        for &h in self.counters.keys() {
            let LaneExit::Branch {
                taken, not_taken, ..
            } = self.x.st.exits[h]
            else {
                continue;
            };
            let latches: Vec<_> = p.blocks[h]
                .preds
                .iter()
                .copied()
                .filter(|&b| dominates(h, b))
                .collect();
            if !latches
                .iter()
                .any(|&b| matches!(self.x.st.exits[b], LaneExit::Jump(t) if t == h))
            {
                continue; // Conditional latches already have their own exit.
            }
            let mut inside = HashSet::from([h]);
            let mut work = latches;
            while let Some(b) = work.pop() {
                if inside.insert(b) {
                    work.extend(p.blocks[b].preds.iter().copied());
                }
            }
            let follow = match (inside.contains(&taken), inside.contains(&not_taken)) {
                (true, false) => not_taken,
                (false, true) => taken,
                _ => continue,
            };
            if inside.iter().all(|&b| {
                p.blocks[b]
                    .exit
                    .successors()
                    .iter()
                    .all(|t| inside.contains(t) || *t == follow)
            }) {
                self.cap_follows.insert(h, follow);
            }
        }
    }

    fn exit(&mut self, b: usize) -> Result<(), Stop> {
        let offset = self.x.p.blocks[b]
            .insts
            .last()
            .map_or(self.x.p.blocks[b].offset, |i| i.machine.offset);
        let term = match self.x.st.exits[b] {
            LaneExit::Jump(t) if self.counters.contains_key(&t) && self.rpo[t] <= self.rpo[b] => {
                let ok = self.count_iteration(t, pred::IULT);
                if self.cap_follows.contains_key(&t) {
                    // The next header visit tests the cap and takes its real
                    // exit, preserving loop-carried values and later barriers.
                    Term::Br(self.block[t].expect("reachable"))
                } else {
                    let abort = self.abort_block();
                    Term::CondBr(ok, self.block[t].expect("reachable"), abort)
                }
            }
            LaneExit::Jump(t) => {
                self.enter_loop(b, t, self.cur);
                Term::Br(self.block[t].expect("reachable"))
            }
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } => {
                // A negated condition branches the other way round.
                let (cond, taken, not_taken) = match self.x.st.bools.node(when) {
                    _ if self.x.st.wide.contains(&b) => {
                        let c = self
                            .wave_wide(b, when)
                            .map_err(|w| unsupported(offset, w))?;
                        (c, taken, not_taken)
                    }
                    _ if self.twin.is_some()
                        && !self.x.st.bools.support(when).iter().all(
                            |a| matches!(a, Atom::Uniform(v) if !self.lane_scalars.contains(v)),
                        ) =>
                    {
                        return Err(unsupported(
                            offset,
                            "a branch the two lanes of a thread may not take together",
                        ));
                    }
                    Some((atom, Bool::TRUE, Bool::FALSE)) => {
                        let a = self
                            .atom(atom, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        (a, not_taken, taken)
                    }
                    _ => {
                        let c = self
                            .boolean(when, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        (c, taken, not_taken)
                    }
                };
                // A back edge is taken only below the iteration limit.
                let back =
                    |l: &Self, to: usize| l.counters.contains_key(&to) && l.rpo[to] <= l.rpo[b];
                // (No `xor` of a compare: a loop exit testing one hung the
                // AMD driver.)
                let mut cond = if back(self, taken) {
                    let ok = self.count_iteration(taken, pred::IULT);
                    if self.cap_follows.contains_key(&taken) {
                        cond
                    } else {
                        self.s.m.bin(self.cur, Bin::And, cond, ok)
                    }
                } else if back(self, not_taken) {
                    let stop = self.count_iteration(not_taken, pred::IUGE);
                    if self.cap_follows.contains_key(&not_taken) {
                        cond
                    } else {
                        self.s.m.bin(self.cur, Bin::Or, cond, stop)
                    }
                } else {
                    cond
                };
                if let Some(&follow) = self.cap_follows.get(&b) {
                    let count = self.counters[&b].0;
                    let limit = self.iteration_limit();
                    cond = if taken == follow {
                        let stop = self.s.m.cmp(self.cur, pred::IUGE, count, limit);
                        self.s.m.tag(stop, Tag::Counter);
                        self.s.m.bin(self.cur, Bin::Or, cond, stop)
                    } else {
                        let ok = self.s.m.cmp(self.cur, pred::IULT, count, limit);
                        self.s.m.tag(ok, Tag::Counter);
                        self.s.m.bin(self.cur, Bin::And, cond, ok)
                    };
                }
                // Critical edges get a block of their own, so loops have
                // dedicated exits and preheaders as DXC emits them.
                let (t, f) = (self.edge(b, taken), self.edge(b, not_taken));
                for (to, via) in [(taken, t), (not_taken, f)] {
                    if !back(self, to) {
                        // From an edge block, or from here.
                        let from = if Some(via) == self.block[to] {
                            self.cur
                        } else {
                            via
                        };
                        self.enter_loop(b, to, from);
                    }
                }
                Term::CondBr(cond, t, f)
            }
            LaneExit::End => {
                if self.s.pixel && !self.exported[b] && !self.has_done_export(b) {
                    // A lane leaving without its final export was killed.
                    let t = self.s.m.bool(true);
                    self.s
                        .op(self.cur, "discard", Attr::NoUnwind, self.s.t.void, 82, &[t]);
                }
                Term::Ret
            }
            LaneExit::Indirect => return Err(unsupported(offset, "an indirect jump")),
        };
        let _ = Exit::End;
        self.s.m.terminate(self.cur, term);
        Ok(())
    }

    /// A forward edge into loop header `to` (from DXIL block `via`) starts
    /// its count at 0.
    fn enter_loop(&mut self, _from: usize, to: usize, via: usize) {
        if let Some((_, incoming)) = self.counters.get_mut(&to) {
            let z = self.s.m.i32(0);
            incoming.push((z, via));
        }
    }

    /// A back edge to header `to` (a back edge keeps its block, no
    /// critical-edge block): the count goes up; the result compares it with
    /// [`LOOP_LIMIT`] by `p` (below: go on; at or above: stop).
    fn count_iteration(&mut self, to: usize, p: u8) -> V {
        let c = self.cur;
        let (phi, _) = self.counters[&to];
        let one = self.s.m.i32(1);
        let next = self.s.m.bin(c, Bin::Add, phi, one);
        self.s.m.tag(phi, Tag::Counter);
        self.s.m.tag(next, Tag::Counter);
        let limit = self.iteration_limit();
        self.counters
            .get_mut(&to)
            .expect("header")
            .1
            .push((next, c));
        let over = self.s.m.cmp(c, p, next, limit);
        self.s.m.tag(over, Tag::Counter);
        over
    }

    fn iteration_limit(&mut self) -> V {
        // Per stage unless the switch names one: a loop that never meets its
        // exit (a count read from a buffer the host has not got yet, say)
        // runs to the limit in every lane. A million iterations a pixel
        // hung the GPU for seconds a draw until Windows reset it
        // (PS_Textured_Eight, 2026-11-24, run 12).
        // shortcut: a pixel loop past 128 iterations, a vertex or mesh loop
        // past 256 or a compute loop past 4,096 is cut short. (They were
        // 1,024, 4,096 and 65,536: the opening cutscene then lost the card
        // 14-25 frames in, in a minimap or shadow mesh draw; with every
        // loop cut at 64 it ran 90 frames. Raise them again once shaders
        // stop reading data that is not there yet.)
        let compute = !self.s.pixel && self.s.mesh.is_none() && self.s.threads.is_some();
        let named = if compute {
            super::compute_loop_limit()
        } else {
            super::loop_limit()
        };
        let limit = match named {
            LOOP_LIMIT if self.s.pixel => 1 << 7,
            LOOP_LIMIT if self.s.mesh.is_some() || self.s.threads.is_none() => 1 << 8,
            LOOP_LIMIT => 1 << 12,
            named => named,
        };
        self.s.m.i32(limit)
    }

    /// The block a lane stopped by [`LOOP_LIMIT`] leaves through.
    fn abort_block(&mut self) -> usize {
        if let Some(a) = self.abort {
            return a;
        }
        let a = self.s.m.new_block();
        if self.s.pixel {
            let t = self.s.m.bool(true);
            self.s
                .op(a, "discard", Attr::NoUnwind, self.s.t.void, 82, &[t]);
        }
        self.s.m.terminate(a, Term::Ret);
        self.abort = Some(a);
        a
    }

    /// The DXIL block a branch from IR block `from` to `to` goes to.
    fn edge(&mut self, from: usize, to: usize) -> usize {
        let target = self.block[to].expect("reachable");
        let preds = self.x.p.blocks[to]
            .preds
            .iter()
            .filter(|&&q| self.block[q].is_some())
            .count();
        // Back edges stay as they are: the exiting block is the latch.
        if preds < 2 || self.rpo[to] <= self.rpo[from] {
            return target;
        }
        let e = self.s.m.new_block();
        self.s.m.terminate(e, Term::Br(target));
        self.edges.insert((from, to), e);
        e
    }

    /// Whether no lane entering IR block `b` from its `n`th predecessor
    /// reads what `reg` holds there before something overwrites it. The
    /// lane is followed along its per-lane branches, with the phis it
    /// passes taking that path's values; a branch whose compares have
    /// constant operands on the path is followed one way. Scalar arithmetic
    /// on the value passes it on; any other read of it, or a branch or guard
    /// that depends on it, makes it live. Paths meeting at a block with the
    /// same registers holding it go on with what they agree on. This is the
    /// console compiler's flag after a join (`if (c) { x = load; f = .. }
    /// if (f) use(x)`, with `f` 0 on the other path), where the register
    /// that holds `x` holds a stale value, often a lane mask, on that path.
    fn dead_on_edge(&self, b: usize, n: usize, reg: Reg) -> bool {
        let p = self.x.p;
        let st = self.x.st;
        // Values computed from the stale one on some path followed.
        let mut tainted = HashSet::new();
        let mut work = vec![(b, n, HashMap::new(), BTreeSet::from([reg]))];
        // Per block reached and registers holding tainted values on entry:
        // the phi values known there, over the paths so far.
        let mut at: HashMap<(usize, BTreeSet<Reg>), HashMap<Value, u32>> = HashMap::new();
        let mut visits = 0;
        while let Some((b, n, mut env, mut regs)) = work.pop() {
            let block = &p.blocks[b];
            let known: Vec<_> = block
                .phis
                .iter()
                .map(|phi| (phi.value, self.konst(&env, phi.args[n])))
                .collect();
            for (v, k) in known {
                match k {
                    Some(k) => env.insert(v, k),
                    None => env.remove(&v),
                };
            }
            for phi in &block.phis {
                if regs.contains(&phi.reg) {
                    tainted.insert(phi.value);
                }
            }
            let key = (b, regs.clone());
            if let Some(old) = at.get(&key) {
                let agreed: HashMap<Value, u32> = old
                    .iter()
                    .filter(|&(v, k)| env.get(v) == Some(k))
                    .map(|(&v, &k)| (v, k))
                    .collect();
                if agreed.len() == old.len() {
                    continue;
                }
                env = agreed;
            }
            at.insert(key, env.clone());
            visits += 1;
            if visits > 4 * p.blocks.len() + 64 {
                return false;
            }
            for (i, inst) in block.insts.iter().enumerate() {
                if regs.is_empty() {
                    break;
                }
                let guard = st.guards[b][i];
                if guard.is_some_and(|g| self.touches(g, &env, &tainted)) {
                    return false;
                }
                // The closing branch is the lane's exit, tested below.
                if inst.reads.iter().any(|r| regs.contains(&r.0))
                    && !inst.name().starts_with("s_cbranch")
                {
                    // Scalar arithmetic only passes the value on.
                    let pure = matches!(inst.fields, Fields::Sop { .. })
                        && inst.write(Reg::S(EXEC)).is_none()
                        && !inst.name().starts_with("s_setreg");
                    if !pure {
                        return false;
                    }
                    for &(r, w) in &inst.writes {
                        regs.insert(r);
                        tainted.insert(w);
                    }
                } else if guard.is_none() {
                    for w in &inst.writes {
                        regs.remove(&w.0);
                    }
                }
            }
            if regs.is_empty() {
                continue;
            }
            let into = |t: usize| p.blocks[t].preds.iter().position(|&q| q == b);
            let mut go = |t: usize, env, regs| match into(t) {
                Some(n) => {
                    work.push((t, n, env, regs));
                    true
                }
                None => false,
            };
            let ok = match st.exits[b] {
                LaneExit::End => true,
                LaneExit::Indirect => false,
                LaneExit::Jump(t) => go(t, env, regs),
                LaneExit::Branch {
                    when,
                    taken,
                    not_taken,
                } => {
                    let (no, yes) = self.can_be(when, &env, &mut HashMap::new());
                    !(no && yes && self.touches(when, &env, &tainted))
                        && (!yes || go(taken, env.clone(), regs.clone()))
                        && (!no || go(not_taken, env, regs))
                }
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// Whether deciding `b`, given the values in `env`, may test an atom
    /// of a `tainted` value.
    fn touches(&self, b: Bool, env: &HashMap<Value, u32>, tainted: &HashSet<Value>) -> bool {
        let mut stack = vec![b];
        let mut seen = HashSet::new();
        while let Some(b) = stack.pop() {
            let Some((atom, lo, hi)) = self.x.st.bools.node(b) else {
                continue;
            };
            if !seen.insert(b) {
                continue;
            }
            let v = match atom {
                Atom::Lane(v)
                | Atom::Uniform(v)
                | Atom::Phi(v)
                | Atom::Quad(v)
                | Atom::Opaque(v) => Some(v),
                Atom::Constant(_) => None,
            };
            if v.is_some_and(|v| tainted.contains(&v)) {
                return true;
            }
            match v.and_then(|v| self.compared(env, v)) {
                Some(true) => stack.push(hi),
                Some(false) => stack.push(lo),
                None => stack.extend([lo, hi]),
            }
        }
        false
    }

    /// Whether `b` can be false, and true, given the values in `env`.
    fn can_be(
        &self,
        b: Bool,
        env: &HashMap<Value, u32>,
        memo: &mut HashMap<Bool, (bool, bool)>,
    ) -> (bool, bool) {
        if b == Bool::TRUE || b == Bool::FALSE {
            return (b == Bool::FALSE, b == Bool::TRUE);
        }
        if let Some(&r) = memo.get(&b) {
            return r;
        }
        let (atom, lo, hi) = self.x.st.bools.node(b).expect("not a constant");
        let v = match atom {
            Atom::Lane(v) | Atom::Uniform(v) => Some(v),
            _ => None,
        };
        let r = match v.and_then(|v| self.compared(env, v)) {
            Some(true) => self.can_be(hi, env, memo),
            Some(false) => self.can_be(lo, env, memo),
            None => {
                let (a, c) = self.can_be(lo, env, memo);
                let (d, e) = self.can_be(hi, env, memo);
                (a || d, c || e)
            }
        };
        memo.insert(b, r);
        r
    }

    /// The instruction writing `v`.
    fn def_inst(&self, v: Value) -> Option<&'a Inst> {
        let b = *self.def_block.get(&v)?;
        let p = self.x.p;
        p.blocks[b]
            .insts
            .iter()
            .find(|i| i.writes.iter().any(|w| w.1 == v))
    }

    /// The constant `v` holds, `env` giving phi values: moves of constants.
    fn konst(&self, env: &HashMap<Value, u32>, v: Value) -> Option<u32> {
        if let Some(&k) = env.get(&v) {
            return Some(k);
        }
        let inst = self.def_inst(v)?;
        let op = match &inst.fields {
            Fields::Valu(w)
                if inst.name() == "v_mov_b32" && w.sdwa.is_none() && w.dpp.is_none() =>
            {
                w.src.first()?
            }
            Fields::Sop { src, .. } if inst.name() == "s_mov_b32" => src.first()?,
            Fields::Sop { imm, .. } if inst.name() == "s_movk_i32" => {
                return Some(i32::from((*imm)? as i16) as u32);
            }
            _ => return None,
        };
        self.konst_operand(env, inst, op)
    }

    fn konst_operand(&self, env: &HashMap<Value, u32>, inst: &Inst, op: &Operand) -> Option<u32> {
        match *op {
            Operand::Const(c) => crate::resource::inline(c),
            Operand::Literal(x) => Some(x),
            Operand::Scalar { code, .. } if code != NULL => {
                self.konst(env, inst.read(Reg::S(code))?)
            }
            Operand::Vector { reg, .. } => self.konst(env, inst.read(Reg::V(reg))?),
            _ => None,
        }
    }

    /// The lane's bit of the 32-bit integer compare writing `v` (a VALU
    /// result before EXEC masking, or SCC), when its operands are constants.
    fn compared(&self, env: &HashMap<Value, u32>, v: Value) -> Option<bool> {
        let inst = self.def_inst(v)?;
        let name = inst.name();
        let rest = ["v_cmpx_", "v_cmp_", "s_cmp_"]
            .iter()
            .find_map(|p| name.strip_prefix(p))?;
        let (op, ty) = rest.rsplit_once('_')?;
        let (a, b) = match &inst.fields {
            Fields::Valu(w) if w.sdwa.is_none() && w.dpp.is_none() => {
                (w.src.first()?, w.src.get(1)?)
            }
            Fields::Sop { src, .. } => (src.first()?, src.get(1)?),
            _ => return None,
        };
        let (a, b) = (
            self.konst_operand(env, inst, a)?,
            self.konst_operand(env, inst, b)?,
        );
        let (sa, sb) = (a as i32, b as i32);
        Some(match (op, ty) {
            ("f", _) => false,
            ("t", _) => true,
            ("eq", "u32" | "i32") => a == b,
            ("ne" | "lg", "u32" | "i32") => a != b,
            ("lt", "u32") => a < b,
            ("le", "u32") => a <= b,
            ("gt", "u32") => a > b,
            ("ge", "u32") => a >= b,
            ("lt", "i32") => sa < sb,
            ("le", "i32") => sa <= sb,
            ("gt", "i32") => sa > sb,
            ("ge", "i32") => sa >= sb,
            _ => return None,
        })
    }

    /// The incoming edges (phi, predecessor index) over which a value can
    /// reach a read. A phi that only another read phi consumes is live too
    /// (else a value with no DXIL form reaching a read through two joins
    /// would be replaced by zero), but only over edges a lane can take on
    /// its way there: each step back adds the branch a lane took on that
    /// edge, and a contradiction (the lane that skipped an arm is not one
    /// that ran it) leaves the edge dead. A back edge forgets the branches
    /// (the loop redefines their conditions). Fewer conditions are weaker,
    /// so keeping only the last few stays sound.
    fn live_edges(&self) -> HashSet<(Value, usize)> {
        const MAX_KNOWN: usize = 4;
        let (p, st) = (self.x.p, &self.x.st);
        let mut work: Vec<(Value, Vec<(Bool, bool)>)> =
            self.read_phis.iter().map(|&v| (v, Vec::new())).collect();
        let mut live = HashSet::new();
        let mut seen = HashMap::<Value, Vec<Vec<(Bool, bool)>>>::new();
        while let Some((v, known)) = work.pop() {
            let Some(&(b, i)) = self.phi_at.get(&v) else {
                continue;
            };
            let done = seen.entry(v).or_default();
            // Known of fewer lanes than before: nothing new is live.
            if done.iter().any(|d| d.iter().all(|c| known.contains(c))) {
                continue;
            }
            done.push(known.clone());
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                if self.tail[q].is_none() {
                    continue;
                }
                let mut k = Vec::new();
                // Forward edges out of a block that is no loop's latch (a
                // loop's exit is also taken at its iteration limit).
                if let LaneExit::Branch {
                    when,
                    taken,
                    not_taken,
                } = st.exits[q]
                    && taken != not_taken
                    && self.rpo[taken] > self.rpo[q]
                    && self.rpo[not_taken] > self.rpo[q]
                {
                    k.clone_from(&known);
                    k.push((when, taken == b));
                    k.drain(..k.len().saturating_sub(MAX_KNOWN));
                    if !st.bools.meet(&k) {
                        continue;
                    }
                } else if self.rpo[q] < self.rpo[b] {
                    k.clone_from(&known);
                }
                live.insert((v, n));
                let arg = p.blocks[b].phis[i].args[n];
                if self.phi_at.contains_key(&arg) {
                    work.push((arg, k));
                }
            }
        }
        live
    }

    /// An unguarded indexed write replaces the incoming value before a
    /// consumer can observe it, either on this edge or in a counted loop.
    fn overwritten_movreld_edge(&self, b: usize, phi: &Phi, edge: usize) -> Option<()> {
        let Reg::V(reg) = phi.reg else { return None };
        let p = self.x.p;
        let block = &p.blocks[b];
        if self.sections.contains_key(&b)
            || self.sections.contains_key(&block.preds[edge])
            || p.blocks
                .iter()
                .flat_map(|bb| &bb.phis)
                .any(|ph| ph.args.contains(&phi.value))
        {
            return None;
        }
        let mut uses = p.blocks.iter().enumerate().flat_map(|(bb, block)| {
            block.insts.iter().enumerate().filter_map(move |(i, inst)| {
                inst.reads
                    .iter()
                    .any(|r| r.1 == phi.value)
                    .then_some((bb, i, inst))
            })
        });
        let (bb, i, mov) = uses.next()?;
        if uses.next().is_some()
            || bb != b
            || self.x.st.guards[b][i].is_some()
            || mov.name() != "v_movreld_b32"
            || mov.read(phi.reg) != Some(phi.value)
        {
            return None;
        }
        let Fields::Valu(v) = &mov.fields else {
            return None;
        };
        let (
            Some(Operand::Vector {
                reg: base,
                dwords: 1,
            }),
            [
                Operand::Vector {
                    reg: src,
                    dwords: 1,
                },
            ],
        ) = (v.dst, v.src.as_slice())
        else {
            return None;
        };
        if v.dpp.is_some()
            || v.sdwa.is_some()
            || mov.read(Reg::V(*src))? == phi.value
            || mov.write(phi.reg).is_none()
        {
            return None;
        }
        let index = u32::from(reg.checked_sub(base)?);
        // Follow M0 to a copy of a scalar phi in this block, then the
        // constant written on the corresponding predecessor.
        let (copy, [src]) = scalar_def(block, mov.read(Reg::S(crate::ir::M0))?, "s_mov_b32")?;
        let scalar = sgpr_operand(copy, src)?;
        let index_phi = block.phis.iter().find(|ph| ph.value == scalar)?;
        let (_, [initial]) = scalar_def(
            &p.blocks[block.preds[edge]],
            index_phi.args[edge],
            "s_mov_b32",
        )?;
        let known = scalar_constant(initial)?;
        if index == known {
            return Some(());
        }
        if index < known {
            return None; // Reaching this slot would require the index to wrap.
        }
        self.counted_movreld_index(b, edge, phi, index_phi, mov, index)
    }

    /// An unguarded move fills one more register on every iteration.
    /// Values from before the loop can only survive in slots the index skips.
    fn counted_movreld_index(
        &self,
        b: usize,
        edge: usize,
        phi: &Phi,
        indices: &Phi,
        mov: &Inst,
        slot: u32,
    ) -> Option<()> {
        let p = self.x.p;
        let st = self.x.st;
        let block = &p.blocks[b];
        let LaneExit::Branch {
            when,
            taken: latch,
            not_taken: exit,
        } = st.exits[b]
        else {
            return None;
        };
        let back = block.preds.iter().position(|&q| q == latch)?;
        if block.preds.len() != 2
            || back == edge
            || b == latch
            || b == exit
            || exit == latch
            || self.sections.contains_key(&latch)
            || st.exits[latch] != LaneExit::Jump(b)
            || p.blocks[latch].preds != [b]
            || p.blocks[exit].preds != [b]
            || st.exits[exit] != LaneExit::End
            || self.rpo[b] >= self.rpo[latch]
            || self.rpo[block.preds[edge]] >= self.rpo[b]
        {
            return None;
        }
        // The indexed result may only go around the loop as its own old
        // value or be consumed in the normal exit block.
        let result = mov.write(phi.reg)?;
        if phi.args[back] != result {
            return None;
        }
        for (bb, block) in p.blocks.iter().enumerate() {
            if bb != exit
                && block
                    .insts
                    .iter()
                    .any(|inst| inst.reads.iter().any(|r| r.1 == result))
            {
                return None;
            }
            if block.phis.iter().any(|other| {
                other
                    .args
                    .iter()
                    .enumerate()
                    .any(|(n, &arg)| arg == result && (other.value != phi.value || n != back))
            }) {
                return None;
            }
        }
        // Follow the latch copy to the unit increment, then require that
        // this very increment is compared for equality to the exit bound.
        let (copy, [src]) = scalar_def(&p.blocks[latch], indices.args[back], "s_mov_b32")?;
        let next = sgpr_operand(copy, src)?;
        let (increment, [from, step]) = scalar_def(block, next, "s_add_i32")?;
        if sgpr_operand(increment, from)? != indices.value || scalar_constant(step)? != 1 {
            return None;
        }
        let (Atom::Uniform(scc), Bool::TRUE, Bool::FALSE) = st.bools.node(when)? else {
            return None;
        };
        let branch = block.insts.last()?;
        if branch.name() != "s_cbranch_scc0" || branch.read(Reg::Scc) != Some(scc) {
            return None;
        }
        let (cmp, [tested, bound]) = scalar_def(block, scc, "s_cmp_eq_u32")?;
        if sgpr_operand(cmp, tested)? != next {
            return None;
        }
        let limit = scalar_constant(bound)?;
        // The initial index is below this slot. The loop must pass it before exit.
        (slot < limit).then_some(())
    }

    fn fill_phis(&mut self) -> Result<(), Stop> {
        let p = self.x.p;
        let live = self.live_edges();
        for (v, b, i) in std::mem::take(&mut self.data_phis) {
            let phi = &p.blocks[b].phis[i];
            let bit = phi.reg == Reg::Scc;
            let mut incoming = Vec::new();
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                let Some(qb) = self.tail[q] else { continue };
                let qb = self.edges.get(&(q, b)).copied().unwrap_or(qb);
                let arg = phi.args[n];
                let read = live.contains(&(phi.value, n));
                let overwritten = read && self.overwritten_movreld_edge(b, phi, n).is_some();
                if read && !overwritten && !self.vals.contains_key(&arg) {
                    let _ = self.lazy_barycentric(arg);
                    if !self.vals.contains_key(&arg)
                        && let Some(&(param, dword)) = self.params.get(&arg)
                    {
                        let val = self.user(param, dword);
                        self.vals.insert(arg, val);
                    }
                }
                let x = match (self.vals.get(&arg).copied(), bit) {
                    (Some(Val::I32(x)), false) | (Some(Val::Bit(x)), true) => x,
                    // No lane from this edge reads it, or no lane comes
                    // along it at all: any value will do.
                    _ if read && (self.never_taken(q, b) || self.dead_on_edge(b, n, phi.reg)) => {
                        if bit {
                            self.s.m.bool(false)
                        } else {
                            self.s.m.i32(0)
                        }
                    }
                    // An input an unguarded counted `v_movreld` loop overwrites
                    // before anything reads it (PR #457).
                    (None | Some(Val::Missing("barycentrics read directly")), false)
                        if overwritten =>
                    {
                        self.s.m.undef(self.s.t.i32)
                    }
                    (Some(Val::Bit(_)), false) if read => {
                        return Err(Stop::Retry(phi.value, "an SCC joined with SGPR values"));
                    }
                    // A lane mask joined with data at a join every lane
                    // enters along one edge: the register holds the mask's
                    // number there (console lanes, so exact).
                    _ if read
                        && !bit
                        && self.mask_data == MaskData::Console
                        && (self.x.st.masks.contains_key(&arg)
                            || self.x.st.highs.contains_key(&arg))
                        && self.uniform_join(b) =>
                    {
                        self.mask_dword(arg)
                            .map_err(|w| Stop::Retry(phi.value, w))?
                    }
                    // SCC from a mask operation is a wave-wide "some lane",
                    // not the bit of whichever lane reaches this phi.
                    (Some(Val::Missing(_)) | None, true)
                        if read
                            && self.mask_data == MaskData::Console
                            && self.x.st.scc_some.contains_key(&arg) =>
                    {
                        let st = self.x.st;
                        let per_lane = !st.wide.contains(&q)
                            && matches!(st.exits[q], LaneExit::Branch { when, .. }
                                if !st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_))));
                        if per_lane {
                            for &other in p.blocks[b].preds.iter().filter(|&&o| o != q) {
                                need(other);
                            }
                            return Err(Stop::Retry(phi.value, LANE_MASK));
                        }
                        self.scc_some(arg)
                            .map_err(|why| Stop::Retry(phi.value, why))?
                    }
                    (Some(Val::Missing(why)), _) if read => {
                        return Err(Stop::Retry(phi.value, why));
                    }
                    _ if read => {
                        return Err(Stop::Retry(phi.value, "a join of mismatched values"));
                    }
                    _ if bit => self.s.m.bool(false),
                    _ => self.s.m.i32(0),
                };
                incoming.push((x, qb));
            }
            self.s.m.set_phi(v, incoming);
        }
        for (phi, incoming) in std::mem::take(&mut self.counters).into_values() {
            self.s.m.set_phi(phi, incoming);
        }
        while let Some((v, b, i)) = self.pending_masks.pop() {
            let phi = &p.blocks[b].phis[i];
            let offset = p.blocks[b].offset;
            let mut incoming = Vec::new();
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                let Some(qb) = self.tail[q] else { continue };
                let qb = self.edges.get(&(q, b)).copied().unwrap_or(qb);
                let arg = phi.args[n];
                const UNTRACKED: Why = "a lane-mask join of an untracked mask";
                // Data joined with masks where the wave takes one edge (the BVH
                // walks pop masks from memory): with console lanes the lane's
                // own bit of the number, as `Atom::Opaque`.
                let number = self.console_lanes && !self.x.native.wave64 && self.uniform_join(b);
                let x = match (self.x.st.masks.get(&arg), self.vals.get(&arg)) {
                    (Some(&m), _) => self.boolean(m, qb).map_err(|w| unsupported(offset, w))?,
                    (None, Some(&Val::I32(word))) if number => self.lane_bit(qb, word, None),
                    (None, _)
                        if !self.uniform_join(b) && self.no_mask_lane_on_edge(b, &phi.args, n) =>
                    {
                        self.s.m.bool(false)
                    }
                    (None, _) => return Err(unsupported(offset, UNTRACKED)),
                };
                incoming.push((x, qb));
            }
            self.s.m.set_phi(v, incoming);
        }
        Ok(())
    }
}

fn all_lane_blocks(st: &Structured) -> Option<Vec<bool>> {
    let n = st.exits.len();
    let mut all = vec![true; n];
    for (_, region) in lane_regions(st)? {
        for b in (0..n).filter(|&b| region[b]) {
            all[b] = false;
        }
    }
    Some(all)
}

fn lane_regions(st: &Structured) -> Option<Vec<(usize, Vec<bool>)>> {
    let n = st.exits.len();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, e) in st.exits.iter().enumerate() {
        match *e {
            LaneExit::Jump(t) => succ[b].push(t),
            LaneExit::Branch {
                taken, not_taken, ..
            } => {
                succ[b].push(taken);
                succ[b].push(not_taken);
            }
            LaneExit::End => {}
            LaneExit::Indirect => return None,
        }
    }
    // post-dominators (bitsets; a virtual exit after every End)
    let words = n.div_ceil(64);
    let full = vec![u64::MAX; words];
    let mut pdom: Vec<Vec<u64>> = (0..n)
        .map(|b| {
            if succ[b].is_empty() {
                let mut v = vec![0; words];
                v[b / 64] |= 1 << (b % 64);
                v
            } else {
                full.clone()
            }
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            if succ[b].is_empty() {
                continue;
            }
            let mut v = full.clone();
            for &t in &succ[b] {
                for w in 0..words {
                    v[w] &= pdom[t][w];
                }
            }
            v[b / 64] |= 1 << (b % 64);
            if v != pdom[b] {
                pdom[b] = v;
                changed = true;
            }
        }
    }
    let mut regions = Vec::new();
    for (d, e) in st.exits.iter().enumerate() {
        let LaneExit::Branch { when, .. } = *e else {
            continue;
        };
        if st.wide.contains(&d)
            || st
                .bools
                .support(when)
                .iter()
                .all(|a| matches!(a, Atom::Uniform(_)))
        {
            continue; // a wave-wide or uniform branch: all lanes go the same way
        }
        // blocks reachable from the lane branch that do not post-dominate it
        let mut seen = vec![false; n];
        let mut stack: Vec<usize> = succ[d].clone();
        while let Some(b) = stack.pop() {
            if seen[b] || pdom[d][b / 64] >> (b % 64) & 1 == 1 {
                continue;
            }
            seen[b] = true;
            stack.extend(succ[b].iter().copied());
        }
        regions.push((d, seen));
    }
    Some(regions)
}

fn lane_constant_masks(st: &Structured) -> bool {
    let whens = st.exits.iter().filter_map(|e| match *e {
        LaneExit::Branch { when, .. } => Some(when),
        _ => None,
    });
    let mut work: Vec<Bool> = st
        .guards
        .iter()
        .flatten()
        .flatten()
        .copied()
        .chain(whens)
        .collect();
    // Through lane-mask joins too: an edge that brings a number or a constant lane set needs
    // the lane's own bit of it.
    // shortcut: masks only selects read (`v_cndmask_b32`, carries in) are not followed; such
    // a program still stops with "a constant lane mask" or "an opaque lane mask".
    let (mut seen, mut joins) = (HashSet::new(), HashSet::new());
    while let Some(g) = work.pop() {
        if !seen.insert(g) {
            continue;
        }
        for a in st.bools.support(g) {
            match a {
                Atom::Opaque(_) | Atom::Constant(_) => return true,
                Atom::Phi(id) if joins.insert(id) => {
                    if let Some(edges) = st.phi_masks.get(&id) {
                        work.extend(edges.iter().copied());
                    }
                }
                _ => {}
            }
        }
    }
    false
}

fn meeting_mode() -> u32 {
    if super::group_sync() == 0 { 8 } else { 1 | 8 }
}

fn interp_mode(kind: Barycentric) -> Option<u8> {
    Some(match kind {
        Barycentric::PerspectiveCenter => 2,
        Barycentric::PerspectiveCentroid => 3,
        Barycentric::PerspectiveSample => 6,
        Barycentric::LinearCenter => 4,
        Barycentric::LinearCentroid => 5,
        Barycentric::LinearSample => 7,
        Barycentric::PerspectivePullModel => return None,
    })
}

#[cfg(test)]
#[path = "lower_tests.rs"]
mod tests;
