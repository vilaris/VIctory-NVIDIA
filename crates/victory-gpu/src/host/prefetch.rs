//! Pieces of copies in the card's memory that a flight's compare flags come
//! down at the flight's own end, in the same submission (`VICTORY_PREFETCH`,
//! on by default with the relay; `=0`: fetched by the landing, as before).
//!
//! Before, the thread that lands flights ([`relay`]) learned which pieces
//! differ only once the flight had run, then recorded a list of copies for
//! them, sent it and waited for the card again: 9-10 ms a frame in 8-9 such
//! waits, 32 MB a frame fetched (run 133409, 13:38), each landing (and so
//! every fence the title waits for behind it) a round trip later.
//!
//! Here the compare's list goes on: after the compare passes, a pass of
//! [`CARD_SPILL`] over the same pieces copies every piece the compare
//! flagged, whole (as the compare hashed it), into memory of ours in the
//! PC's memory, at a slot it takes with an atomic count, and notes the slot
//! beside the piece's result. When the flight has run, what it wrote is
//! there already: the landing merges it without asking the card for
//! anything. A piece without a slot (more were flagged than the memory
//! holds, a range that was not compared, a piece fetched because an
//! earlier fetch was torn) is fetched as before.
//!
//! Why the bytes are right. The pass runs on the drawing queue behind the
//! flight's lists and the compare, before anything sent later: it holds
//! what the flight ended with, and title bytes sent up with sends up to the
//! flight's own (`CardPlan::sent_as`), which is what the merge is told
//! (`fetched_after`): bytes sent up later are left out, as with a fetch.
//! Where several flights of a landing flagged a piece the last one's slot
//! is taken; where a later flight declared it and did not flag it, the
//! compare found it as an earlier one left it (it stores the hash it makes),
//! so the earlier slot holds the piece as it is, but for title bytes sent up
//! since, which are left out. No later flight can be at a piece while it
//! comes down: what came down is never torn (`torn`).

use std::sync::atomic::{AtomicU32, AtomicU64};

use super::*;

/// See the file.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| relay() && std::env::var_os("VICTORY_PREFETCH").is_none_or(|v| v != "0"))
}

/// The pass: one group per piece of a compare pass, as [`CARD_COMPARE`]
/// cuts them. Its first thread reads the piece's result; a flagged piece
/// takes a slot (the dword at 0 counts them, also past `capacity`), and
/// the slot (1 on) or 0 goes into the header at the piece's place; then
/// the group copies the piece, 16 bytes a thread at a time.
pub(crate) const CARD_SPILL: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), SRV(t0), UAV(u0), UAV(u1)"
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes
    uint pieces;
    uint out_at;   // the first piece's dword in `differs`, in bytes
    uint local;    // the first piece's place in the header, in dwords
    uint capacity; // slots
    uint header;   // bytes before slot 0
};
ByteAddressBuffer now : register(t0);
RWByteAddressBuffer differs : register(u0);
RWByteAddressBuffer spill : register(u1);
groupshared uint slot;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_GroupID, uint thread : SV_GroupIndex) {
    if (thread == 0u) {
        uint s = 0u;
        if ((differs.Load(out_at + id.x * 4u) & 1u) != 0u) {
            uint n;
            spill.InterlockedAdd(0u, 1u, n);
            s = n < capacity ? n + 1u : 0u;
        }
        spill.Store(4u + (local + id.x) * 4u, s);
        slot = s;
    }
    GroupMemoryBarrierWithGroupSync();
    if (slot == 0u) return;
    uint piece = first + id.x;
    uint lo = piece * 65536u;
    lo = lo > skew ? lo - skew : 0u;
    uint hi = min((piece + 1u) * 65536u - skew, size);
    lo = lo & ~15u;
    hi = (hi + 15u) & ~15u;
    uint base = header + (slot - 1u) * 65792u;
    [loop]
    for (uint at = lo + thread * 16u; at < hi; at += 1024u) {
        spill.Store4(base + (at - lo), now.Load4(at));
    }
}
"#;

/// Bytes a slot takes: a piece widened to 16 bytes at both ends, with room.
pub(crate) const STRIDE: u64 = 65536 + 256;

/// Slots a plan's memory is made with at first, the fewest and the most.
const FIRST: u32 = 64;
const FEWEST: u32 = 16;
const MOST: u32 = 2048;

/// Since the last frame-rate line: pieces landed from a slot and their
/// bytes; flagged pieces fetched for want of one; plans whose flagged
/// pieces did not all find a slot; pieces the passes copied down.
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Slots the next plan's memory is to have.
static WANT: AtomicU32 = AtomicU32::new(FIRST);

struct Pipeline {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects, free-threaded.
unsafe impl Send for Pipeline {}
unsafe impl Sync for Pipeline {}

/// Memory of ours: buffer, mapping, bytes.
struct Memory(ID3D12Resource, usize, u64);

// SAFETY: a buffer and its persistent mapping; used by one plan at a time.
unsafe impl Send for Memory {}

/// Memory given back by plans done with, for the next ones.
static POOL: Mutex<Vec<Memory>> = Mutex::new(Vec::new());

/// At most this many kept in [`POOL`].
const KEPT: usize = 48;

/// A plan's slots: where its flagged pieces came down.
pub(super) struct Spill {
    memory: Memory,
    /// The bytes before slot 0.
    header: u64,
    capacity: u32,
    /// The plan's results in `differs`: the first dword and how many.
    base: u32,
    dwords: u32,
}

impl Spill {
    /// Where the piece whose compare result is `dword` came down (the first
    /// byte [`crate::shadow::Shadow::card_hashed`] gives), if it did.
    pub(super) fn slot(&self, dword: u32) -> Option<usize> {
        let local = dword.checked_sub(self.base).filter(|&l| l < self.dwords)?;
        // SAFETY: inside the header, which the pass (it has run) wrote.
        let s = unsafe { ((self.memory.1 + 4 + 4 * local as usize) as *const u32).read_volatile() };
        (s != 0 && s <= self.capacity).then(|| self.memory.1 + self.header as usize + (s as usize - 1) * STRIDE as usize)
    }

    /// The pieces flagged, also those past the slots.
    fn flagged(&self) -> u32 {
        // SAFETY: the count, at the mapping's start.
        unsafe { (self.memory.1 as *const u32).read_volatile() }
    }
}

fn pipeline(host: &Host) -> Option<&'static Pipeline> {
    static MADE: OnceLock<Option<Pipeline>> = OnceLock::new();
    MADE.get_or_init(|| {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the calls; the blob's
        // own bytes; the root signature is inside the shader.
        let made = unsafe {
            (|| -> windows::core::Result<Pipeline> {
                D3DCompile(
                    CARD_SPILL.as_ptr().cast(),
                    CARD_SPILL.len(),
                    windows::core::s!("card spill"),
                    None,
                    None,
                    windows::core::s!("main"),
                    windows::core::s!("cs_5_1"),
                    1 << 15,
                    0,
                    &mut code,
                    None,
                )?;
                let code = code.ok_or_else(windows::core::Error::empty)?;
                let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
                let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes)?;
                let pso: ID3D12PipelineState = host.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })?;
                Ok(Pipeline { root, pso })
            })()
        };
        match made {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(error = %e, "no pass to bring flagged pieces down with their flight: the landings fetch them (VICTORY_PREFETCH)");
                None
            }
        }
    })
    .as_ref()
}

/// Memory of at least `bytes`, from the pool or new.
fn memory(host: &Host, bytes: u64) -> Option<Memory> {
    {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        // The smallest that holds it.
        let best = pool.iter().enumerate().filter(|(_, m)| m.2 >= bytes).min_by_key(|(_, m)| m.2).map(|(i, _)| i);
        if let Some(i) = best {
            return Some(pool.swap_remove(i));
        }
    }
    let size = bytes.next_multiple_of(1 << 20);
    let (buffer, data) = host.shared_buffer_cpu(size)?;
    name(&buffer, || "flagged pieces of copies in the card's memory, come down with their flight".into());
    Some(Memory(buffer, data as usize, size))
}

/// After the compare passes of a plan, on its open list (the compare's):
/// the pass over the pieces of `compares` (of the plan's `ranges`) that are
/// landed. `differs` is the
/// compare's results buffer; the plan's results are its dwords `base` to
/// `end`. `None` if there is nothing to do or no memory.
///
/// # Safety
/// The list is open and records the compare of `ranges` just before;
/// live buffers.
pub(super) unsafe fn record(
    host: &Host,
    list: &ID3D12GraphicsCommandList,
    ranges: &[CardRange],
    compares: &[CardCompare],
    differs: &ID3D12Resource,
    base: u32,
    end: u32,
) -> Option<Spill> {
    let passes: Vec<&CardCompare> = compares.iter().filter(|c| !ranges[c.range].picked).collect();
    if passes.is_empty() || end <= base {
        return None;
    }
    let pipeline = pipeline(host)?;
    let dwords = end - base;
    let header = (4 + 4 * u64::from(dwords)).next_multiple_of(256);
    let capacity = WANT.load(Ordering::Relaxed);
    let memory = memory(host, header + u64::from(capacity) * STRIDE)?;
    // As many slots as the memory holds.
    let capacity = (((memory.2 - header) / STRIDE) as u32).min(MOST);
    // SAFETY: the memory is ours alone (the plan that had it is done).
    unsafe { (memory.1 as *mut u32).write_volatile(0) };
    // SAFETY: the caller's.
    unsafe {
        let _rf_x2 = crate::rf_x2::range(list, "vx:prefetch"); // rf-x2
        // What the compare wrote, first.
        crate::rf_x2::barriers(list, "vx:barrier.prefetch", &[D3D12_RESOURCE_BARRIER { // rf-x2
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        }]);
        list.SetComputeRootSignature(&pipeline.root);
        list.SetPipelineState(&pipeline.pso);
        list.SetComputeRootUnorderedAccessView(2, differs.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(3, memory.0.GetGPUVirtualAddress());
        for pass in passes {
            let r = &ranges[pass.range];
            if !r.shadow.card_has_before() {
                continue;
            }
            let size = r.shadow.card_hash_size();
            list.SetComputeRootShaderResourceView(1, r.shadow.buffer.GetGPUVirtualAddress());
            const MOST_GROUPS: u32 = D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION;
            let mut done = 0;
            while done < pass.span.pieces {
                let n = (pass.span.pieces - done).min(MOST_GROUPS);
                let at = pass.at + done;
                let constants =
                    [pass.span.first + done, pass.span.skew, size as u32, n, at * 4, at - base, capacity, header as u32];
                list.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
                list.Dispatch(n, 1, 1);
                done += n;
            }
        }
    }
    Some(Spill { memory, header, capacity, base, dwords })
}

/// The plan is done with (`Host::card_done`): its memory goes back, and the
/// next plans get as many slots as this one wanted, with room.
pub(super) fn give_back(spill: Spill) {
    let flagged = spill.flagged();
    STATS[4].fetch_add(u64::from(flagged.min(spill.capacity)), Ordering::Relaxed);
    if flagged > spill.capacity {
        STATS[3].fetch_add(1, Ordering::Relaxed);
    }
    let wanted = (flagged + flagged / 4).next_power_of_two().clamp(FEWEST, MOST);
    // Up at once, down slowly.
    let _ = WANT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |was| {
        Some(if wanted >= was { wanted } else { (was - (was - wanted) / 8).max(FEWEST) })
    });
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if pool.len() < KEPT {
        pool.push(spill.memory);
    }
}

/// A piece landed from a slot (`bytes` of it), or fetched for want of one.
pub(super) fn landed(from_slot: bool, bytes: u64) {
    if from_slot {
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(bytes, Ordering::Relaxed);
    } else {
        STATS[2].fetch_add(1, Ordering::Relaxed);
    }
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    if !on() || s.iter().all(|&v| v == 0.0) {
        return;
    }
    tracing::info!(
        "flagged pieces come down with their flight per frame (VICTORY_PREFETCH=1): {:.0} copied down by the card, {:.0} landed \
         from there ({:.1} MB) with no wait for the card, {:.0} fetched by the landing for want of a slot; {:.2} plans with more \
         flagged than slots ({} slots now)",
        s[4],
        s[0],
        s[1] / (1 << 20) as f64,
        s[2],
        s[3],
        WANT.load(Ordering::Relaxed),
    );
}
