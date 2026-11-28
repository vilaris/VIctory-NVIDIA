//! What goes up to copies in the card's memory goes up as it was when its
//! submission was prepared (`VICTORY_SEND_OWN`, on by default; `0`: as
//! before).
//!
//! The sends were copies out of the copies' CPU sides, which the card makes
//! when it comes to the submission. By then later submissions are prepared
//! (two frames of them with `VICTORY_RELAY`), and their refreshes have
//! written the same CPU sides: the earlier submission's shaders then read a
//! later one's bytes. For what the title's CPU writes that is as on the
//! console (it does not write what the card still reads). But the title's
//! own buffer copies are commands in its lists, and those are done by us
//! when a submission is prepared: "a value to a buffer, a draw that reads
//! it" in one submission and the same with another value in the next is
//! ordinary, and both draws read the second value.
//!
//! So each submission's bytes are taken out of the CPU sides into memory of
//! the submission's own when they are taken from the dirty lists, and the
//! card copies them from there. Runs of a few words, which a refresh that
//! sends only what changed makes many of, go as pairs of offset and value,
//! stored by one dispatch per copy; the others as a copy each.

use super::*;
use crate::shadow::Shadow;

pub(super) fn own() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_OWN").is_none_or(|v| v != "0"))
}

/// Whether fragmented 4-byte-aligned sends go through one bulk upload copy into a
/// device-local staging buffer and the `BULK_SCATTER` compute shader, with
/// `SCATTER` reading from device-local memory instead of over PCIe
/// (`VICTORY_BULK_SCATTER`, on by default; `=0`: `CopyBufferRegion` + PCIe
/// `SCATTER`).
fn bulk_scatter_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| own() && std::env::var_os("VICTORY_BULK_SCATTER").is_none_or(|v| v != "0"))
}

/// Runs up to this many bytes go as pairs.
const SHORT: usize = 256;

/// A contiguous send of at least one write-watch block goes directly to
/// its destination. Staging it would use a copy operation anyway
/// and add a scatter over the whole block.
const DIRECT_BYTES: usize = 64 << 10;

/// Maximum dwords copied per `BULK_SCATTER` threadgroup (64 threads × 4 dwords).
const TILE_DWORDS: u32 = 256;

/// Whether a run of `len` bytes at offset `at` of a copy goes as pairs.
fn short(at: usize, len: usize) -> bool {
    len <= SHORT && at % 4 == 0 && len % 4 == 0
}

/// Whether a run of `len` bytes at offset `at` of a copy in the card's memory
/// is no copy command of its own, but pairs of one dispatch.
pub(crate) fn goes_as_words(at: usize, len: usize) -> bool {
    own() && short(at, len)
}

/// Whether the title's bytes that go in under flights' writes
/// (`Shadow::merge_under_writes`) go in by whole words
/// (`VICTORY_UNDER_WORDS=0`: by the bytes that changed, as before).
///
/// By bytes nearly every run (a float's three low bytes) was a copy command
/// of its own, and a submission with more than 4,096 of them stood until the
/// flights had landed: 4 to 14 ms of a steady frame (session play-1125-0203:
/// "more changed runs than a submission sends up"), for some 20 KB. Whole
/// words are pairs of one dispatch, and are not counted.
///
/// A word is taken to be the title's or the shaders', not shared: where the
/// title changed a byte of a word of which a flight's shader writes another
/// byte, that byte is as it was before the flight. The landing rests on the
/// same ([`crate::shadow::exact_back`]).
pub(crate) fn words_under() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| own() && std::env::var_os("VICTORY_UNDER_WORDS").is_none_or(|v| v != "0"))
}

/// Compiles `source` with `D3DCOMPILE_OPTIMIZATION_LEVEL3` (`1 << 15`).
fn compile_opt(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("victory-send"),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize())
            };
            tracing::warn!("a send shader of ours: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe { std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize()) };
    Ok(bytes.to_vec())
}

const SCATTER: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t0), UAV(u0)"
cbuffer Part : register(b0) { uint at; uint count; };
ByteAddressBuffer pairs : register(t0);
RWByteAddressBuffer card : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= count) return;
    uint2 p = pairs.Load2(at + id.x * 8u);
    card.Store(p.x, p.y);
}
"#;

const BULK_SCATTER: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), SRV(t0), UAV(u0)"
cbuffer Part : register(b0) { uint tiles_at; };
ByteAddressBuffer vram : register(t0);
RWByteAddressBuffer card : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID) {
    uint3 t = vram.Load3(tiles_at + gid.x * 12u);
    uint dst = t.x;
    uint src = t.y;
    uint dwords = t.z;
    if (((dst | src) & 15u) == 0u) {
        uint vecs = dwords >> 2;
        if (tid.x < vecs) {
            card.Store4(dst + tid.x * 16u, vram.Load4(src + tid.x * 16u));
        }
        uint tail = vecs << 2;
        if (tail + tid.x < dwords) {
            uint off = (tail + tid.x) * 4u;
            card.Store(dst + off, vram.Load(src + off));
        }
    } else {
        for (uint i = tid.x; i < dwords; i += 64u) {
            uint off = i * 4u;
            card.Store(dst + off, vram.Load(src + off));
        }
    }
}
"#;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

fn make_pass(device: &ID3D12Device, source: &str, label: &'static str) -> Option<Pass> {
    let code = compile_opt(source, windows::core::s!("main"), windows::core::s!("cs_5_1")).ok()?;
    // SAFETY: the code lives through the calls; the root signature is
    // inside it.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &code).ok()?;
        let pso: ID3D12PipelineState = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })
            .ok()?;
        name(&pso, || label.into());
        Some(Pass { root, pso })
    }
}

fn pass(host: &Host) -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| make_pass(&host.device, SCATTER, "victory: changed words sent to the card"))
        .as_ref()
}

fn bulk_pass(host: &Host) -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| make_pass(&host.device, BULK_SCATTER, "victory: bulk scatter to the card"))
        .as_ref()
}

/// Device-local staging buffer used as the single DMA target before
/// `BULK_SCATTER` and `SCATTER` read from VRAM.
struct VramBuffer {
    buffer: ID3D12Resource,
    size: u64,
}

// SAFETY: a D3D12 resource is free-threaded with one owner while in use/pooled.
unsafe impl Send for VramBuffer {}

/// Memory a submission's sends come from, mapped, plus optional device-local
/// staging for bulk scatter.
pub(super) struct Memory {
    buffer: ID3D12Resource,
    data: usize,
    size: u64,
    offset: u64,
    ring: Option<UploadLease>,
    vram: Option<VramBuffer>,
}

// SAFETY: a D3D12 object (free-threaded) and exclusive access to this slice.
unsafe impl Send for Memory {}

struct RingMemory {
    buffer: ID3D12Resource,
    data: usize,
    slots: Mutex<crate::rf_x9::UploadRing>,
}

struct UploadLease {
    slots: &'static Mutex<crate::rf_x9::UploadRing>,
    token: u64,
}

impl Drop for UploadLease {
    fn drop(&mut self) {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).release(self.token);
    }
}

const RING_BYTES: u64 = 32 << 20;

fn ring_memory(host: &Host, size: u64) -> Option<Memory> {
    static RING: OnceLock<Option<RingMemory>> = OnceLock::new();
    if size > RING_BYTES {
        return None;
    }
    let ring = RING.get_or_init(|| {
        let (buffer, data) = host.upload_buffer(RING_BYTES)?;
        name(&buffer, || "X9 submission upload ring".into());
        Some(RingMemory {
            buffer,
            data: data as usize,
            slots: Mutex::new(crate::rf_x9::UploadRing::new(RING_BYTES)),
        })
    }).as_ref()?;
    let (offset, token) = ring.slots.lock().unwrap_or_else(|e| e.into_inner()).reserve(size)?;
    crate::rf_x9::traffic(crate::rf_x9::Traffic::UploadRing, || size);
    Some(Memory {
        buffer: ring.buffer.clone(),
        data: ring.data + offset as usize,
        size,
        offset,
        ring: Some(UploadLease { slots: &ring.slots, token }),
        vram: None,
    })
}

/// What submissions that have run gave back.
static SPARE: Mutex<Vec<Memory>> = Mutex::new(Vec::new());
/// Device-local staging buffers returned after submissions have run.
static SPARE_VRAM: Mutex<Vec<VramBuffer>> = Mutex::new(Vec::new());

/// At most this much is kept (of the card's memory, with [`on_card`]).
fn kept() -> u64 {
    if on_card() { 256 << 20 } else { 768 << 20 }
}

const KEPT_VRAM: u64 = 512 << 20;

/// Whether the memory lies in the card's memory, written by the CPU through
/// the bus (`VICTORY_SEND_BAR=1`): the copies on the card's drawing queue, which
/// nothing overlaps, then cross no bus. In a captured frame they took 5.4 ms of
/// the card's 30 (342 copies), and take 0.4 this way; but filling it takes the
/// submitting thread 8-9 ms a frame instead of 3 (73-93 MB, run card-3 against
/// card-1), so it is a loss while the card waits for that thread: off by
/// default.
fn on_card() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_BAR").is_some_and(|v| v == "1"))
}

fn memory(host: &Host, size: u64) -> Option<Memory> {
    if crate::rf_x9::enabled() && !on_card() && let Some(memory) = ring_memory(host, size) {
        return Some(memory);
    }
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        let fit = spare.iter().enumerate().filter(|(_, m)| m.size >= size).min_by_key(|(_, m)| m.size).map(|(i, _)| i);
        if let Some(i) = fit {
            return Some(spare.swap_remove(i));
        }
    }
    // By doubles, so that the next of about this size finds it: what
    // submissions send differs from frame to frame.
    let size = size.max(8 << 20).next_power_of_two();
    let mapped = if on_card() { host.mapped_card_buffer(size, false) } else { None };
    let (buffer, data) = match mapped {
        Some(made) => made,
        None => host.upload_buffer(size)?,
    };
    name(&buffer, || "bytes a submission sends up".into());
    Some(Memory {
        buffer,
        data: data as usize,
        size,
        offset: 0,
        ring: None,
        vram: None,
    })
}

fn vram_buffer(host: &Host, size: u64) -> Option<VramBuffer> {
    {
        let mut spare = SPARE_VRAM.lock().unwrap_or_else(|e| e.into_inner());
        let fit = spare.iter().enumerate().filter(|(_, v)| v.size >= size).min_by_key(|(_, v)| v.size).map(|(i, _)| i);
        if let Some(i) = fit {
            return Some(spare.swap_remove(i));
        }
    }
    let size = size.max(8 << 20).next_power_of_two();
    let buffer = host.uav_buffer_for(size, Kind::UavSend)?;
    name(&buffer, || "bytes a submission scatters on the card".into());
    Some(VramBuffer { buffer, size })
}

/// The submission has run.
pub(super) fn give_back(mut memory: Memory) {
    if let Some(vram) = memory.vram.take() {
        let mut spare_vram = SPARE_VRAM.lock().unwrap_or_else(|e| e.into_inner());
        spare_vram.push(vram);
        spare_vram.sort_by_key(|v| std::cmp::Reverse(v.size));
        while spare_vram.iter().map(|v| v.size).sum::<u64>() > KEPT_VRAM {
            spare_vram.remove(0);
        }
    }
    if memory.ring.is_some() {
        return;
    }
    let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
    spare.push(memory);
    spare.sort_by_key(|m| std::cmp::Reverse(m.size));
    while spare.iter().map(|m| m.size).sum::<u64>() > kept() {
        spare.remove(0);
    }
}

/// Since the last frame-rate line: bytes copied, runs copied, words stored,
/// microseconds of taking them out of the CPU sides, direct copy bytes and
/// staged bytes (including tile and pair records).
pub(super) static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// One tile descriptor read by `BULK_SCATTER`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tile {
    dst: u32,
    src: u32,
    dwords: u32,
}

const TILE_STRIDE: usize = std::mem::size_of::<Tile>();

/// Splits a 4-byte-aligned run `[dst, dst + len)` from `src` into fixed-max-size
/// tiles (`TILE_DWORDS`), emitting a tiny head tile when `dst` is not 16-byte
/// aligned so all body tiles hit the `uint4` vectorized path.
fn push_tiles(tiles: &mut Vec<Tile>, mut dst: u32, mut src: u32, mut len: u32) {
    debug_assert_eq!(dst % 4, 0);
    debug_assert_eq!(src % 4, 0);
    debug_assert_eq!(len % 4, 0);
    if len == 0 {
        return;
    }
    let misalign = dst & 15;
    if misalign != 0 && (src & 15) == misalign {
        let head = (16 - misalign).min(len);
        tiles.push(Tile {
            dst,
            src,
            dwords: head / 4,
        });
        dst += head;
        src += head;
        len -= head;
    }
    let mut dwords = len / 4;
    while dwords > 0 {
        let n = dwords.min(TILE_DWORDS);
        tiles.push(Tile { dst, src, dwords: n });
        let bytes = n * 4;
        dst += bytes;
        src += bytes;
        dwords -= n;
    }
}

/// One copy's sends: fallback `CopyBufferRegion` ranges (offset in the copy,
/// in the upload memory, bytes), `BULK_SCATTER` tiles (offset in VRAM buffer,
/// count), `SCATTER` pairs (offset in buffer, count), and bulk run count.
struct Copy {
    runs: Vec<(usize, u64, usize)>,
    tiles: (u64, u32),
    pairs: (u64, u32),
    bulk_runs: u64,
}

/// Runs of a copy's sends joined into one copy each where one ends exactly
/// where the next begins, on the card side and in the memory alike (never
/// across a gap: bytes between two runs may be what shaders wrote on the
/// card). `VICTORY_BATCH_COPIES=0`: as made.
pub(crate) fn batch_copies_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BATCH_COPIES").is_none_or(|v| v != "0"))
}

/// `runs` (card offset, memory offset, bytes) with adjacent ones joined
/// ([`batch_copies_on`]).
fn joined_runs(runs: &[(usize, u64, usize)]) -> Vec<(usize, u64, usize)> {
    let mut out: Vec<(usize, u64, usize)> = Vec::with_capacity(runs.len());
    for &(a, from, n) in runs {
        if batch_copies_on()
            && let Some(last) = out.last_mut()
            && last.0 + last.2 == a
            && last.1 + last.2 as u64 == from
        {
            last.2 += n;
            continue;
        }
        out.push((a, from, n));
    }
    out
}

/// Where a submission's bytes are taken to.
pub(super) struct Own {
    memory: Memory,
    copies: Vec<Copy>,
    vram_bytes: u64,
}

/// Runs whose pairs are one piece of work.
const RUNS_AT_A_TIME: usize = 4096;

/// Takes what `dirty` sends up out of the CPU sides.
pub(super) fn take(host: &Host, dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)]) -> Option<Own> {
    let began = std::time::Instant::now();
    pass(host)?;
    let bulk_scatter = bulk_scatter_on() && bulk_pass(host).is_some();
    take_with(host, dirty, bulk_scatter, began)
}

fn take_with(
    host: &Host,
    dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)],
    bulk_scatter: bool,
    began: std::time::Instant,
) -> Option<Own> {
    let mut vram_at = 0u64;
    let mut unaligned_at = 0u64;
    let mut copies = Vec::with_capacity(dirty.len());
    let mut all_tiles: Vec<Tile> = Vec::new();
    let mut tile_slices: Vec<(usize, usize, u64)> = Vec::new();
    // (src_ptr, dst_rel_offset, len)
    let mut bulk_moves: Vec<(usize, u64, usize)> = Vec::new();
    let mut short_ranges: Vec<(usize, usize)> = Vec::new();
    // (from_ptr, short_slice_start..short_slice_end, dst_rel_offset)
    let mut pending_work: Vec<(usize, std::ops::Range<usize>, u64)> = Vec::new();
    let mut total_words = 0u64;

    for (shadow, ranges) in dirty {
        let from = shadow.card_data() as usize;
        let mut runs = Vec::new();
        let mut words = 0usize;
        let mut bulk_runs = 0u64;
        let tiles_start = all_tiles.len();
        let work_start = pending_work.len();
        let mut chunk_start = short_ranges.len();
        let mut chunk_words = 0usize;
        let mut pair_rel = 0u64;

        for &(a, b) in ranges {
            let len = b - a;
            if len == 0 {
                continue;
            }
            if short(a, len) {
                short_ranges.push((a, b));
                let w = len / 4;
                words += w;
                chunk_words += w;
                if short_ranges.len() - chunk_start == RUNS_AT_A_TIME {
                    pending_work.push((from, chunk_start..short_ranges.len(), pair_rel));
                    pair_rel += chunk_words as u64 * 8;
                    chunk_start = short_ranges.len();
                    chunk_words = 0;
                }
            } else if bulk_scatter && len < DIRECT_BYTES && a % 4 == 0 && len % 4 == 0 {
                // Keep src mod 16 equal to dst mod 16 so body tiles are 16-byte aligned.
                let src_off = vram_at + (a & 15) as u64;
                vram_at = (src_off + len as u64).next_multiple_of(16);
                bulk_runs += 1;
                bulk_moves.push((from + a, src_off, len));
                push_tiles(&mut all_tiles, a as u32, src_off as u32, len as u32);
            } else {
                runs.push((a, unaligned_at, len));
                unaligned_at += (len as u64).next_multiple_of(16);
            }
        }
        if chunk_words > 0 {
            pending_work.push((from, chunk_start..short_ranges.len(), pair_rel));
        }

        let tile_count = (all_tiles.len() - tiles_start) as u32;
        let tiles = if tile_count > 0 {
            let off = vram_at;
            vram_at += (u64::from(tile_count) * TILE_STRIDE as u64).next_multiple_of(16);
            tile_slices.push((tiles_start, all_tiles.len(), off));
            (off, tile_count)
        } else {
            (0, 0)
        };

        let pairs = if words > 0 {
            let off = vram_at;
            vram_at += (words as u64 * 8).next_multiple_of(16);
            for w in &mut pending_work[work_start..] {
                w.2 += off;
            }
            total_words += words as u64;
            (off, words as u32)
        } else {
            (0, 0)
        };

        copies.push(Copy {
            runs,
            tiles,
            pairs,
            bulk_runs,
        });
    }

    let vram_bytes = if bulk_scatter { vram_at } else { 0 };
    let total_upload = vram_at + unaligned_at;
    if total_upload == 0 {
        return None;
    }

    let vram = if bulk_scatter && vram_bytes > 0 {
        if vram_bytes > u32::MAX as u64 {
            return take_with(host, dirty, false, began);
        }
        match vram_buffer(host, vram_bytes) {
            Some(v) => Some(v),
            None => return take_with(host, dirty, false, began),
        }
    } else {
        None
    };

    let mut memory = memory(host, total_upload)?;
    memory.vram = vram;

    if vram_at > 0 && unaligned_at > 0 {
        for copy in &mut copies {
            for run in &mut copy.runs {
                run.1 += vram_at;
            }
        }
    }

    let fallback_runs: usize = copies.iter().map(|c| c.runs.len()).sum();
    let mut moves: Vec<(usize, usize, usize)> = Vec::with_capacity(bulk_moves.len() + fallback_runs);
    moves.extend(
        bulk_moves
            .into_iter()
            .map(|(src, off, len)| (src, memory.data + off as usize, len)),
    );
    for ((shadow, _), copy) in dirty.iter().zip(&copies) {
        if !copy.runs.is_empty() {
            let from = shadow.card_data() as usize;
            moves.extend(copy.runs.iter().map(|&(a, to, n)| (from + a, memory.data + to as usize, n)));
        }
    }

    let one = |i: usize| {
        let (from, ref range_idx, to_off) = pending_work[i];
        let mut to = (memory.data + to_off as usize) as *mut [u32; 2];
        for &(a, b) in &short_ranges[range_idx.clone()] {
            for offset in (a..b).step_by(4) {
                // SAFETY: inside the copy's mapping (its CPU side, which
                // the thread that asked, and waits, alone writes in these
                // ranges) and inside the memory, which has room for every
                // pair, a piece of work's behind the last one's.
                unsafe {
                    let word = ((from + offset) as *const u32).read_unaligned();
                    to.write([offset as u32, word]);
                    to = to.add(1);
                }
            }
        }
    };
    STATS[0].fetch_add(moves.iter().map(|m| m.2 as u64).sum(), Ordering::Relaxed);
    STATS[1].fetch_add(moves.len() as u64, Ordering::Relaxed);
    STATS[2].fetch_add(total_words, Ordering::Relaxed);
    STATS[4].fetch_add(copies.iter().flat_map(|copy| &copy.runs).map(|run| run.2 as u64).sum(), Ordering::Relaxed);
    STATS[5].fetch_add(vram_bytes, Ordering::Relaxed);
    let tile_moves = tile_slices.into_iter().map(|(start, end, off)| {
        let slice = &all_tiles[start..end];
        (slice.as_ptr() as usize, memory.data + off as usize, std::mem::size_of_val(slice))
    });
    if crate::rf_x9::enabled() {
        moves.extend(tile_moves);
        // The plan has already assigned disjoint destinations to payload,
        // tile tables and pairs. Run all three in one wave while the source
        // shadows and this submission's upload allocation are still held.
        let (moves, starts) = crate::rf_x9::upload_jobs(&moves, 256 << 10);
        let pairs = pending_work.len();
        crate::land::each_under(pairs + starts.len() - 1, &|i| {
            let _job = crate::rf_x9::timer(crate::rf_x9::Work::UploadPack);
            if i < pairs {
                one(i);
            } else {
                let job = i - pairs;
                copy_upload(&moves[starts[job]..starts[job + 1]]);
            }
        });
        if crate::rf_x9::verify() {
            for ((shadow, _), copy) in dirty.iter().zip(&copies) {
                if let Some(&(at, offset, len)) = copy.runs.first() {
                    // Check the CopyBufferRegion source that the recorded
                    // command will read, relative to the legacy shadow.
                    crate::rf_x9::verify_memory_at(
                        "card-upload",
                        shadow.span().0 as usize + at,
                        shadow.card_data() as usize + at,
                        memory.data + offset as usize,
                        len,
                    );
                }
            }
        }
    } else {
        tile_moves.for_each(|m| copy_upload(std::slice::from_ref(&m)));
        crate::land::each_under(pending_work.len(), &one);
        crate::shadow::copy_all(moves);
    }
    STATS[3].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);

    Some(Own {
        memory,
        copies,
        vram_bytes,
    })
}

fn copy_upload(moves: &[(usize, usize, usize)]) {
    for &(src, dst, bytes) in moves {
        // SAFETY: these moves are made by `take_with` from live sources to
        // disjoint slices of its private upload mapping, then joined there.
        unsafe { std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, bytes) };
    }
}

impl Own {
    /// Records the sends into `list`; the copies that `looked` says are in
    /// `COPY_DEST`, the others in the common state. Those with a "before" are
    /// left readable by compute shaders.
    ///
    /// # Safety
    /// An open list of ours, whose compute state is set anew behind this.
    pub(super) unsafe fn record(
        &self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)],
        looked: impl Fn(&Shadow) -> bool,
    ) {
        let common = D3D12_RESOURCE_STATE_COMMON;
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let write = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let dest = D3D12_RESOURCE_STATE_COPY_DEST;
        let (mut first, mut last) = (Vec::new(), Vec::new());
        let (mut made, mut recorded) = (0u64, 0u64);

        if let Some(vram) = &self.memory.vram
            && self.vram_bytes > 0
        {
            recorded += 1;
            // SAFETY: the caller's; live buffers, range inside both.
            unsafe { list.CopyBufferRegion(&vram.buffer, 0, &self.memory.buffer, self.memory.offset, self.vram_bytes) };
            first.push(transition(&vram.buffer, dest, read));
            last.push(transition(&vram.buffer, read, common));
        }

        for ((shadow, _), copy) in dirty.iter().zip(&self.copies) {
            made += copy.runs.len() as u64 + copy.bulk_runs;
            for (a, from, n) in joined_runs(&copy.runs) {
                recorded += 1;
                // SAFETY: the caller's; live buffers, ranges inside them.
                unsafe { list.CopyBufferRegion(&shadow.buffer, a as u64, &self.memory.buffer, self.memory.offset + from, n as u64) };
            }
            let mut state = (looked(shadow) || !copy.runs.is_empty()).then_some(dest);
            let uav_writes = copy.tiles.1 > 0 || copy.pairs.1 > 0;
            if uav_writes {
                if let Some(was) = state {
                    first.push(transition(&shadow.buffer, was, write));
                }
                state = Some(write);
            }
            let hashed = shadow.card_has_before();
            if let Some(was) = state
                && hashed
                && (!copy.runs.is_empty() || uav_writes)
            {
                last.push(transition(&shadow.buffer, was, read));
            }
        }

        // SAFETY: the caller's; live objects.
        unsafe {
            if !first.is_empty() {
                crate::rf_x2::barriers(list, "vx:barrier.coh.send", &first); // rf-x2
            }
            let srv_gpu = self
                .memory
                .vram
                .as_ref()
                .map_or_else(|| self.memory.buffer.GetGPUVirtualAddress() + self.memory.offset, |v| v.buffer.GetGPUVirtualAddress());
            if let Some(bulk) = bulk_pass(host)
                && self.copies.iter().any(|c| c.tiles.1 > 0)
            {
                let _rf_x2 = crate::rf_x2::range(list, "vx:coh.send.bulk"); // rf-x2
                list.SetComputeRootSignature(&bulk.root);
                list.SetPipelineState(&bulk.pso);
                list.SetComputeRootShaderResourceView(1, srv_gpu);
                for ((shadow, _), copy) in dirty.iter().zip(&self.copies).filter(|(_, c)| c.tiles.1 > 0) {
                    list.SetComputeRootUnorderedAccessView(2, shadow.buffer.GetGPUVirtualAddress());
                    let (at, count) = copy.tiles;
                    let mut done = 0u32;
                    while done < count {
                        let n = (count - done).min(D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION);
                        let constants = [at as u32 + done * TILE_STRIDE as u32];
                        list.SetComputeRoot32BitConstants(0, 1, constants.as_ptr().cast(), 0);
                        list.Dispatch(n, 1, 1);
                        done += n;
                    }
                }
            }
            if let Some(pass) = pass(host)
                && self.copies.iter().any(|c| c.pairs.1 > 0)
            {
                let _rf_x2 = crate::rf_x2::range(list, "vx:coh.send.scatter"); // rf-x2
                list.SetComputeRootSignature(&pass.root);
                list.SetPipelineState(&pass.pso);
                list.SetComputeRootShaderResourceView(1, srv_gpu);
                for ((shadow, _), copy) in dirty.iter().zip(&self.copies).filter(|(_, c)| c.pairs.1 > 0) {
                    list.SetComputeRootUnorderedAccessView(2, shadow.buffer.GetGPUVirtualAddress());
                    const MOST: u32 = 64 * D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION;
                    let (at, words) = copy.pairs;
                    let mut done = 0;
                    while done < words {
                        let n = (words - done).min(MOST);
                        let constants = [at as u32 + done * 8, n];
                        list.SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
                        list.Dispatch(n.div_ceil(64), 1, 1);
                        done += n;
                    }
                }
            }
            if !last.is_empty() {
                crate::rf_x2::barriers(list, "vx:barrier.coh.send", &last); // rf-x2
            }
        }
        super::gpu_time::count_sent_runs(made, recorded);
    }

    /// The memory, to be kept until the submission has run.
    pub(super) fn memory(self) -> Memory {
        self.memory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_split_and_align_body_chunks() {
        let mut tiles = Vec::new();
        // 16-byte aligned exact multiple of TILE_DWORDS (1024 B)
        push_tiles(&mut tiles, 0x2000, 0x1000, 2048);
        assert_eq!(
            tiles,
            vec![
                Tile { dst: 0x2000, src: 0x1000, dwords: 256 },
                Tile { dst: 0x2400, src: 0x1400, dwords: 256 },
            ]
        );

        // 4-byte aligned, non-16-byte aligned start: emits 3-dword head tile,
        // then 16-byte-aligned body tile(s).
        tiles.clear();
        push_tiles(&mut tiles, 0x104, 0x4004, 260);
        assert_eq!(
            tiles,
            vec![
                Tile { dst: 0x104, src: 0x4004, dwords: 3 },
                Tile { dst: 0x110, src: 0x4010, dwords: 62 },
            ]
        );
        let total_bytes: u32 = tiles.iter().map(|t| t.dwords * 4).sum();
        assert_eq!(total_bytes, 260);
        for t in &tiles[1..] {
            assert_eq!(t.dst & 15, 0);
            assert_eq!(t.src & 15, 0);
        }

        // Crosses multiple tiles with non-zero tail dwords.
        tiles.clear();
        push_tiles(&mut tiles, 0x8, 0x8, 2500);
        assert_eq!(tiles[0], Tile { dst: 0x8, src: 0x8, dwords: 2 });
        assert_eq!(tiles[1], Tile { dst: 0x10, src: 0x10, dwords: 256 });
        assert_eq!(tiles[2], Tile { dst: 0x410, src: 0x410, dwords: 256 });
        assert_eq!(tiles[3], Tile { dst: 0x810, src: 0x810, dwords: 111 });
        assert_eq!(tiles.iter().map(|t| t.dwords * 4).sum::<u32>(), 2500);
    }

    #[test]
    fn shaders_compile_with_optimization_level3() {
        let scatter = compile_opt(SCATTER, windows::core::s!("main"), windows::core::s!("cs_5_1"));
        assert!(scatter.is_ok(), "SCATTER failed to compile: {scatter:?}");
        assert!(!scatter.unwrap().is_empty());

        let bulk = compile_opt(BULK_SCATTER, windows::core::s!("main"), windows::core::s!("cs_5_1"));
        assert!(bulk.is_ok(), "BULK_SCATTER failed to compile: {bulk:?}");
        assert!(!bulk.unwrap().is_empty());
    }

    fn test_device() -> Option<ID3D12Device> {
        // SAFETY: plain DXGI/D3D12 device probing for unit test execution.
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).is_ok()
                && let Some(d) = device
            {
                return Some(d);
            }
            let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
            let warp: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            D3D12CreateDevice(&warp, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    fn create_buffer(
        device: &ID3D12Device,
        heap_type: D3D12_HEAP_TYPE,
        size: u64,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: heap_type,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        };
        let mut res: Option<ID3D12Resource> = None;
        // SAFETY: plain committed buffer creation in tests.
        unsafe {
            device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut res)
                .unwrap();
        }
        res.unwrap()
    }

    #[test]
    fn bulk_scatter_and_scatter_roundtrip_on_gpu() {
        let Some(device) = test_device() else {
            return;
        };
        let scatter_pass = make_pass(&device, SCATTER, "test scatter").unwrap();
        let bulk_pass = make_pass(&device, BULK_SCATTER, "test bulk scatter").unwrap();

        const DST_SIZE: usize = 2 * DIRECT_BYTES;
        let mut expected = vec![0xAAu8; DST_SIZE];
        // Unaligned-mod-16 long run (> SHORT)
        let run1 = (0x104usize, 0x104usize + 260usize);
        // 16-byte-aligned multi-tile run (> TILE_DWORDS * 4) with non-multiple-of-16 tail
        let run2 = (0x1000usize, 0x1000usize + 2500usize);
        // Short run via SCATTER pairs
        let short_run = (0x20usize, 0x20usize + 16usize);
        let direct_run = (DIRECT_BYTES, DST_SIZE);

        let mut fill_pattern = |range: (usize, usize), seed: u8| {
            for (i, byte) in expected[range.0..range.1].iter_mut().enumerate() {
                *byte = seed.wrapping_add((i * 13 + 7) as u8);
            }
        };
        fill_pattern(run1, 0x11);
        fill_pattern(run2, 0x55);
        fill_pattern(short_run, 0x99);
        fill_pattern(direct_run, 0x37);

        // Pack upload buffer: [init_sentinel(8192) | bulk_payloads | tiles | pairs]
        let mut upload_bytes = vec![0xAAu8; DST_SIZE];
        let mut at = 0usize;
        let mut tiles = Vec::new();
        for &(a, b) in &[run1, run2] {
            let len = b - a;
            let src_off = at + (a & 15);
            let need = src_off + len;
            if upload_bytes.len() < DST_SIZE + need {
                upload_bytes.resize(DST_SIZE + need, 0);
            }
            upload_bytes[DST_SIZE + src_off..DST_SIZE + need].copy_from_slice(&expected[a..b]);
            push_tiles(&mut tiles, a as u32, src_off as u32, len as u32);
            at = need.next_multiple_of(16);
        }
        let tiles_off = at;
        let tiles_bytes = tiles.len() * TILE_STRIDE;
        upload_bytes.resize(DST_SIZE + tiles_off + tiles_bytes.next_multiple_of(16), 0);
        // SAFETY: copying Pod-like Tile structs into staging byte vec.
        unsafe {
            std::ptr::copy_nonoverlapping(
                tiles.as_ptr().cast::<u8>(),
                upload_bytes.as_mut_ptr().add(DST_SIZE + tiles_off),
                tiles_bytes,
            );
        }
        at = (tiles_off + tiles_bytes).next_multiple_of(16);

        let pairs_off = at;
        let pair_words = (short_run.1 - short_run.0) / 4;
        upload_bytes.resize(DST_SIZE + pairs_off + (pair_words * 8).next_multiple_of(16), 0);
        for (idx, off) in (short_run.0..short_run.1).step_by(4).enumerate() {
            let val = u32::from_le_bytes(expected[off..off + 4].try_into().unwrap());
            let dst = DST_SIZE + pairs_off + idx * 8;
            upload_bytes[dst..dst + 4].copy_from_slice(&(off as u32).to_le_bytes());
            upload_bytes[dst + 4..dst + 8].copy_from_slice(&val.to_le_bytes());
        }
        let vram_payload_size = (pairs_off + pair_words * 8).next_multiple_of(16) as u64;
        let direct_off = upload_bytes.len();
        upload_bytes.extend_from_slice(&expected[direct_run.0..direct_run.1]);

        let upload = create_buffer(
            &device,
            D3D12_HEAP_TYPE_UPLOAD,
            upload_bytes.len() as u64,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let vram = create_buffer(
            &device,
            D3D12_HEAP_TYPE_DEFAULT,
            vram_payload_size,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        );
        let card = create_buffer(
            &device,
            D3D12_HEAP_TYPE_DEFAULT,
            DST_SIZE as u64,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        );
        let readback = create_buffer(
            &device,
            D3D12_HEAP_TYPE_READBACK,
            DST_SIZE as u64,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        // SAFETY: plain D3D12 command recording and synchronous fence wait.
        unsafe {
            let mut ptr = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut ptr)).unwrap();
            std::ptr::copy_nonoverlapping(upload_bytes.as_ptr(), ptr.cast(), upload_bytes.len());
            upload.Unmap(0, None);

            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let alloc: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)
                .unwrap();

            // Seed card with 0xAA and populate VRAM staging via a single CopyBufferRegion.
            list.CopyBufferRegion(&card, 0, &upload, 0, DST_SIZE as u64);
            list.CopyBufferRegion(&card, direct_run.0 as u64, &upload, direct_off as u64, DIRECT_BYTES as u64);
            list.CopyBufferRegion(&vram, 0, &upload, DST_SIZE as u64, vram_payload_size);
            list.ResourceBarrier(&[
                transition(&vram, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
                transition(&card, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
            ]);

            list.SetComputeRootSignature(&bulk_pass.root);
            list.SetPipelineState(&bulk_pass.pso);
            list.SetComputeRootShaderResourceView(1, vram.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(2, card.GetGPUVirtualAddress());
            let c_bulk = [tiles_off as u32];
            list.SetComputeRoot32BitConstants(0, 1, c_bulk.as_ptr().cast(), 0);
            list.Dispatch(tiles.len() as u32, 1, 1);

            list.SetComputeRootSignature(&scatter_pass.root);
            list.SetPipelineState(&scatter_pass.pso);
            list.SetComputeRootShaderResourceView(1, vram.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(2, card.GetGPUVirtualAddress());
            let c_scatter = [pairs_off as u32, pair_words as u32];
            list.SetComputeRoot32BitConstants(0, 2, c_scatter.as_ptr().cast(), 0);
            list.Dispatch((pair_words as u32).div_ceil(64), 1, 1);

            list.ResourceBarrier(&[
                transition(&vram, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_COMMON),
                transition(&card, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE),
            ]);
            list.CopyBufferRegion(&readback, 0, &card, 0, DST_SIZE as u64);
            list.Close().unwrap();

            let cmd: ID3D12CommandList = list.cast().unwrap();
            queue.ExecuteCommandLists(&[Some(cmd)]);
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            queue.Signal(&fence, 1).unwrap();
            if fence.GetCompletedValue() < 1 {
                fence.SetEventOnCompletion(1, event).unwrap();
                WaitForSingleObject(event, INFINITE);
            }
            windows::Win32::Foundation::CloseHandle(event).unwrap();

            let mut rb_ptr = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut rb_ptr)).unwrap();
            let got = std::slice::from_raw_parts(rb_ptr.cast::<u8>(), DST_SIZE);
            assert_eq!(got, expected.as_slice());
            readback.Unmap(0, None);
        }
    }
}
