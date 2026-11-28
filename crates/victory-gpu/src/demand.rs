//! Demand landing for shader-written memory in carded copies (`VICTORY_DEMAND_LAND=1`).
//!
//! Carded host copies (such as the 174 MB graphics heap) accumulate hundreds of
//! GPU-written 64 KiB pieces per frame (skinning output, compute scratch) that
//! are consumed solely by subsequent GPU passes and never read by the title
//! CPU. Instead of fetching and merging every differing piece over PCIe before
//! signalling the title, [`park`] records the latest GPU range per 64 KiB block,
//! marks pages whose block is not yet known to be CPU-read as `PAGE_NOACCESS`,
//! and skips the eager copy-back.
//!
//! Any CPU read or write to a parked block traps in `watch::on_write`, fetches
//! the latest bytes from the card via a dedicated copy queue, lands them
//! (through the runtime's `beside` view when available), restores protection,
//! and promotes the block to `CPU_READ` so subsequent frames land it eagerly
//! with zero further faults.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const BLOCK: usize = 64 << 10;
const MAX_BLOCKS: usize = 1 << 16; // 4 GiB / 64 KiB
const WORDS: usize = MAX_BLOCKS / 64;
const SHARDS: usize = 16;

/// `VICTORY_DEMAND_LAND=0` disables demand landing (eagerly lands every
/// differing piece, as before).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DEMAND_LAND").is_none_or(|v| v != "0") && crate::shadow::cards())
}

static CPU_READ: [AtomicU64; WORDS] = [const { AtomicU64::new(0) }; WORDS];
static GUARDED: [AtomicU64; WORDS] = [const { AtomicU64::new(0) }; WORDS];
static PINNED: [AtomicU32; MAX_BLOCKS] = [const { AtomicU32::new(0) }; MAX_BLOCKS];

static STATS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
const DEFERRED_PIECES: usize = 0;
const DEFERRED_BYTES: usize = 1;
const FAULTS: usize = 2;
const FAULT_BYTES: usize = 3;
const KNOWN_READ_BLOCKS: usize = 4;
const TITLE_FAULTS: usize = 5;
const SAFECOPY_FAULTS: usize = 6;
const PIN_FULFILLS: usize = 7;
const WRITE_FULFILLS: usize = 8;
const KERNEL_FULFILLS: usize = 9;

#[derive(Clone)]
struct ParkedPiece {
    shadow: Arc<crate::shadow::Shadow>,
    lo: usize,
    hi: usize,
    fetched_after: u64,
}

struct ParkedBlock {
    pieces: Vec<ParkedPiece>,
    views: Vec<(usize, u32)>,
}

type ShardMap = HashMap<usize, ParkedBlock>;
static PARKED: [Mutex<Option<ShardMap>>; SHARDS] = [const { Mutex::new(None) }; SHARDS];
static ALIASES: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);

#[inline]
fn has_bit(bits: &[AtomicU64; WORDS], block: usize) -> bool {
    block < MAX_BLOCKS && (bits[block / 64].load(Ordering::Acquire) >> (block % 64)) & 1 != 0
}

#[inline]
fn set_bit(bits: &[AtomicU64; WORDS], block: usize) -> bool {
    if block >= MAX_BLOCKS {
        return false;
    }
    let mask = 1u64 << (block % 64);
    (bits[block / 64].fetch_or(mask, Ordering::AcqRel) & mask) == 0
}

#[inline]
fn clear_bit(bits: &[AtomicU64; WORDS], block: usize) {
    if block < MAX_BLOCKS {
        bits[block / 64].fetch_and(!(1u64 << (block % 64)), Ordering::AcqRel);
    }
}

#[inline]
fn has_any(bits: &[AtomicU64; WORDS], from: usize, to: usize) -> bool {
    let to = to.min(MAX_BLOCKS);
    if from >= to {
        return false;
    }
    let mut b = from;
    while b < to {
        let word = bits[b / 64].load(Ordering::Acquire);
        if word == 0 {
            b = (b / 64 + 1) * 64;
            continue;
        }
        let bit = b % 64;
        let end = ((b / 64 + 1) * 64).min(to) - b;
        let mask = if end >= 64 { !0u64 << bit } else { ((1u64 << end) - 1) << bit };
        if (word & mask) != 0 {
            return true;
        }
        b += end;
    }
    false
}

fn shard(block: usize) -> std::sync::MutexGuard<'static, Option<ShardMap>> {
    PARKED[block % SHARDS].lock().unwrap_or_else(|e| e.into_inner())
}

fn mark_cpu_read(block: usize) {
    if set_bit(&CPU_READ, block) {
        STATS[KNOWN_READ_BLOCKS].fetch_add(1, Ordering::Relaxed);
        crate::rf_x9::eager_landing();
    }
}

/// Pins blocks overlapping `[at, at + len)` so `park` will not mark them
/// `PAGE_NOACCESS` while the caller inspects title memory under a shadow lock.
/// Also fulfills any already-parked blocks in that range first.
pub(crate) struct Pin {
    from: usize,
    to: usize,
}

impl Drop for Pin {
    fn drop(&mut self) {
        for b in self.from..self.to {
            PINNED[b].fetch_sub(1, Ordering::Release);
        }
    }
}

pub(crate) fn pin_range(at: usize, len: usize) -> Pin {
    if !on() || len == 0 {
        return Pin { from: 0, to: 0 };
    }
    let from = (at / BLOCK).min(MAX_BLOCKS);
    let to = at.saturating_add(len).div_ceil(BLOCK).min(MAX_BLOCKS);
    for b in from..to {
        PINNED[b].fetch_add(1, Ordering::AcqRel);
    }
    if has_any(&GUARDED, from, to) {
        for b in from..to {
            if has_bit(&GUARDED, b) && fulfill_block(b, false) {
                STATS[PIN_FULFILLS].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Pin { from, to }
}

/// Protects the X9 guard opener from racing a demand-landing protection
/// change. Unlike pin_range, this operation never lands bytes or waits.
pub(crate) fn try_pin_fault(block: usize) -> Option<Pin> {
    let pin = PINNED.get(block)?;
    pin.fetch_add(1, Ordering::SeqCst);
    let held = Pin { from: block, to: block + 1 };
    let guarded = GUARDED[block / 64].load(Ordering::SeqCst) & (1 << (block % 64)) != 0;
    (!guarded).then_some(held)
}

/// Attempts to defer landing of piece `[lo, hi)` of `shadow`.
/// Returns `true` if parked on the card (caller must skip eager fetch/land).
pub(crate) fn park(shadow: &Arc<crate::shadow::Shadow>, lo: usize, hi: usize, fetched_after: u64) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    if !on() || lo >= hi {
        return false;
    }
    let title_start = shadow.span().0 as usize + lo;
    let title_end = shadow.span().0 as usize + hi;
    let b = title_start / BLOCK;
    if b >= MAX_BLOCKS || b != (title_end - 1) / BLOCK {
        return false;
    }
    // Never allow C1 in-place reads to bypass a shader-written block parked on card.
    crate::port::gpu_bound(title_start, title_end - title_start);
    if has_bit(&CPU_READ, b) || PINNED[b].load(Ordering::Acquire) != 0 {
        return false;
    }

    let mut map = shard(b);
    if has_bit(&CPU_READ, b) || PINNED[b].load(Ordering::Acquire) != 0 {
        return false;
    }
    let blocks = map.get_or_insert_with(Default::default);
    if let Some(existing) = blocks.get_mut(&b) {
        push_or_merge_piece(&mut existing.pieces, shadow, lo, hi, fetched_after);
        STATS[DEFERRED_PIECES].fetch_add(1, Ordering::Relaxed);
        STATS[DEFERRED_BYTES].fetch_add((hi - lo) as u64, Ordering::Relaxed);
        return true;
    }

    // Collect the title view block(s) backing physical block `b`.
    let mut addrs = Vec::with_capacity(2);
    addrs.push(b * BLOCK);
    if let Some(_reading) = crate::watch::title_memory_reading_now()
        && let Some((_, shown)) = crate::inplace::beside_page(b * BLOCK)
    {
        for v in shown {
            if v != 0 && v < MAX_BLOCKS * BLOCK && !addrs.contains(&v) {
                if has_bit(&CPU_READ, v / BLOCK) || PINNED[v / BLOCK].load(Ordering::Acquire) != 0 {
                    return false;
                }
                addrs.push(v);
            }
        }
    }

    if crate::rf_x9::enabled() {
        for (i, &addr) in addrs.iter().enumerate() {
            let block = addr / BLOCK;
            let mask = 1u64 << (block % 64);
            if GUARDED[block / 64].fetch_or(mask, Ordering::SeqCst) & mask != 0 {
                for &made in &addrs[..i] {
                    clear_bit(&GUARDED, made / BLOCK);
                }
                return false;
            }
        }
        // If a fault didn't see the guard bit, this check sees its pin.
        if addrs.iter().any(|&addr| PINNED[addr / BLOCK].load(Ordering::SeqCst) != 0) {
            for &addr in &addrs {
                clear_bit(&GUARDED, addr / BLOCK);
            }
            return false;
        }
    } else {
        for &addr in &addrs {
            set_bit(&GUARDED, addr / BLOCK);
        }
    }

    let mut views = Vec::with_capacity(addrs.len());
    let mut failed = false;
    for &addr in &addrs {
        let mut old = 0u32;
        // SAFETY: `addr` is a 64 KiB-aligned title virtual block; only protection changes.
        let ok = unsafe { VirtualProtect(addr as *const _, BLOCK, PAGE_NOACCESS, &mut old) } != 0;
        if !ok || !matches!(old, PAGE_READWRITE | PAGE_READONLY) {
            if ok {
                // SAFETY: restore original protection for non-RW/RO pages (code/noaccess).
                unsafe { VirtualProtect(addr as *const _, BLOCK, old, &mut old) };
            }
            failed = true;
            break;
        }
        views.push((addr, old));
    }

    if failed || PINNED[b].load(Ordering::Acquire) != 0 {
        restore_views(&views);
        for &addr in &addrs {
            clear_bit(&GUARDED, addr / BLOCK);
        }
        if failed {
            mark_cpu_read(b);
        }
        return false;
    }

    if addrs.len() > 1 {
        let mut aliases = ALIASES.lock().unwrap_or_else(|e| e.into_inner());
        let a = aliases.get_or_insert_with(Default::default);
        for &addr in &addrs[1..] {
            a.insert(addr / BLOCK, b);
        }
    }

    blocks.insert(
        b,
        ParkedBlock {
            pieces: vec![ParkedPiece {
                shadow: shadow.clone(),
                lo,
                hi,
                fetched_after,
            }],
            views,
        },
    );
    STATS[DEFERRED_PIECES].fetch_add(1, Ordering::Relaxed);
    STATS[DEFERRED_BYTES].fetch_add((hi - lo) as u64, Ordering::Relaxed);
    true
}

fn push_or_merge_piece(pieces: &mut Vec<ParkedPiece>, shadow: &Arc<crate::shadow::Shadow>, lo: usize, hi: usize, fetched_after: u64) {
    for p in pieces.iter_mut() {
        if Arc::ptr_eq(&p.shadow, shadow) {
            p.lo = p.lo.min(lo);
            p.hi = p.hi.max(hi);
            p.fetched_after = p.fetched_after.max(fetched_after);
            return;
        }
    }
    pieces.push(ParkedPiece {
        shadow: shadow.clone(),
        lo,
        hi,
        fetched_after,
    });
}

fn restore_views(views: &[(usize, u32)]) {
    use windows_sys::Win32::System::Memory::VirtualProtect;
    for &(addr, old) in views {
        let mut unused = 0u32;
        // SAFETY: restoring previous protection saved from `VirtualProtect`.
        unsafe { VirtualProtect(addr as *const _, BLOCK, old, &mut unused) };
    }
}

/// Called from `watch::on_write` when an access violation occurs at `at`.
/// `permanent` is `true` for title thread faults and `false` for `safecopy`.
pub(crate) fn on_fault(at: usize, permanent: bool) -> bool {
    if !on() {
        return false;
    }
    let block = at / BLOCK;
    if !has_bit(&GUARDED, block) {
        return false;
    }
    let _fault = crate::rf_x9::timer(crate::rf_x9::Work::Fault);
    let canonical = ALIASES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|a| a.get(&block).copied())
        .unwrap_or(block);
    let ok = fulfill_block(canonical, permanent);
    if ok {
        STATS[if permanent { TITLE_FAULTS } else { SAFECOPY_FAULTS }].fetch_add(1, Ordering::Relaxed);
    }
    ok
}

/// Fulfills any demand-guarded blocks overlapping `[at, at + len)`.
/// Returns `true` if at least one guarded block was fulfilled.
pub(crate) fn fulfill_range(at: usize, len: usize, permanent: bool) -> bool {
    if !on() || len == 0 {
        return false;
    }
    let from = (at / BLOCK).min(MAX_BLOCKS);
    let to = at.saturating_add(len).div_ceil(BLOCK).min(MAX_BLOCKS);
    if !has_any(&GUARDED, from, to) {
        return false;
    }
    let mut did = false;
    for block in from..to {
        if has_bit(&GUARDED, block) {
            let canonical = ALIASES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|a| a.get(&block).copied())
                .unwrap_or(block);
            if fulfill_block(canonical, permanent) {
                STATS[if permanent { KERNEL_FULFILLS } else { WRITE_FULFILLS }].fetch_add(1, Ordering::Relaxed);
                did = true;
            }
        }
    }
    did
}

fn fulfill_block(block: usize, permanent: bool) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    if !has_bit(&GUARDED, block) {
        return false;
    }
    let mut map = shard(block);
    let Some(entry) = map.as_mut().and_then(|m| m.remove(&block)) else {
        // Another thread just finished fulfilling this block concurrently.
        if permanent {
            mark_cpu_read(block);
        }
        return !has_bit(&GUARDED, block);
    };

    if entry.views.len() > 1 {
        let mut aliases = ALIASES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = aliases.as_mut() {
            for &(addr, _) in &entry.views[1..] {
                a.remove(&(addr / BLOCK));
            }
        }
    }

    let has_beside = crate::land::beside_on()
        && crate::watch::title_memory_reading_now()
            .and_then(|_| crate::inplace::beside_page(block * BLOCK))
            .is_some();

    if !has_beside {
        // Private allocation with no alias mapping: temporarily open title view
        // so `land_card` can merge bytes in place.
        for &(addr, _) in &entry.views {
            let mut old = 0u32;
            // SAFETY: saved address from our `VirtualProtect`.
            unsafe { VirtualProtect(addr as *const _, BLOCK, PAGE_READWRITE, &mut old) };
        }
    }

    let mut gpu = Vec::new();
    let mut notes = crate::land::Notes::default();
    let mut total_bytes = 0u64;
    if let Some(host) = crate::host::existing() {
        for p in &entry.pieces {
            let len = p.hi - p.lo;
            let raw = windows::core::Interface::as_raw(&p.shadow.buffer) as usize;
            if len == 0 || raw <= 0x1000 {
                continue;
            }
            let started = crate::rf_x3::timer();
            let fetched = host.demand_fetch(&p.shadow.buffer, p.lo as u64, len, &mut gpu);
            if fetched {
                total_bytes += len as u64;
                let ptr = gpu.as_ptr() as usize;
                // SAFETY: `gpu` holds `len` bytes just fetched for `[p.lo, p.hi)`.
                unsafe {
                    p.shadow.land_card(&[(p.lo, p.hi, ptr)], p.fetched_after, &mut notes);
                }
            }
            if let Some(started) = started {
                use crate::rf_x3::{Object, Source, elapsed, work};
                work(Object::shadow(&p.shadow), Source::DemandFetch, if fetched { len as u64 } else { 0 }, elapsed(started));
            }
        }
    }
    notes.flush();

    if has_beside {
        restore_views(&entry.views);
    } else {
        for &(addr, old) in &entry.views {
            if old == PAGE_READONLY {
                let mut unused = 0u32;
                // SAFETY: restore `PAGE_READONLY` expected by `watch::Guards`.
                unsafe { VirtualProtect(addr as *const _, BLOCK, old, &mut unused) };
            }
        }
    }

    for &(addr, _) in &entry.views {
        clear_bit(&GUARDED, addr / BLOCK);
        if permanent {
            mark_cpu_read(addr / BLOCK);
        }
    }
    if permanent {
        mark_cpu_read(block);
    }
    STATS[FAULTS].fetch_add(1, Ordering::Relaxed);
    STATS[FAULT_BYTES].fetch_add(total_bytes, Ordering::Relaxed);
    true
}

/// Forgets any parked blocks overlapping `[at, at + len)` when title memory is
/// unmapped, remapped, or freed (`len == 0`: queries the region at `at`).
pub(crate) fn forget(at: usize, len: usize) {
    if !on() {
        return;
    }
    let len = if len != 0 {
        len
    } else {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid out pointer of `MEMORY_BASIC_INFORMATION`.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 { BLOCK } else { info.RegionSize.max(BLOCK) }
    };
    let from = (at / BLOCK).min(MAX_BLOCKS);
    let to = at.saturating_add(len).div_ceil(BLOCK).min(MAX_BLOCKS);
    for b in from..to {
        clear_bit(&CPU_READ, b);
        if has_bit(&GUARDED, b) {
            let canonical = ALIASES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .and_then(|a| a.remove(&b))
                .unwrap_or(b);
            let mut map = shard(canonical);
            if let Some(entry) = map.as_mut().and_then(|m| m.remove(&canonical)) {
                for &(addr, _) in &entry.views {
                    clear_bit(&GUARDED, addr / BLOCK);
                }
            }
            clear_bit(&GUARDED, b);
        }
    }
}

/// Removes parked pieces belonging to a dropped `Shadow`.
pub(crate) fn drop_shadow(id: u64) {
    if !on() {
        return;
    }
    for s in 0..SHARDS {
        let mut map = PARKED[s].lock().unwrap_or_else(|e| e.into_inner());
        let Some(blocks) = map.as_mut() else {
            continue;
        };
        let mut empty = Vec::new();
        for (&b, entry) in blocks.iter_mut() {
            entry.pieces.retain(|p| p.shadow.id() != id);
            if entry.pieces.is_empty() {
                empty.push(b);
            }
        }
        for b in empty {
            if let Some(entry) = blocks.remove(&b) {
                if entry.views.len() > 1 {
                    let mut aliases = ALIASES.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(a) = aliases.as_mut() {
                        for &(addr, _) in &entry.views[1..] {
                            a.remove(&(addr / BLOCK));
                        }
                    }
                }
                restore_views(&entry.views);
                for &(addr, _) in &entry.views {
                    clear_bit(&GUARDED, addr / BLOCK);
                }
            }
        }
    }
}

pub(crate) fn log(frames: f64) {
    let pieces = STATS[DEFERRED_PIECES].swap(0, Ordering::Relaxed) as f64 / frames;
    let bytes = STATS[DEFERRED_BYTES].swap(0, Ordering::Relaxed) as f64 / frames;
    let faults = STATS[FAULTS].swap(0, Ordering::Relaxed) as f64 / frames;
    let fault_bytes = STATS[FAULT_BYTES].swap(0, Ordering::Relaxed) as f64 / frames;
    let title_f = STATS[TITLE_FAULTS].swap(0, Ordering::Relaxed) as f64 / frames;
    let safe_f = STATS[SAFECOPY_FAULTS].swap(0, Ordering::Relaxed) as f64 / frames;
    let pin_f = STATS[PIN_FULFILLS].swap(0, Ordering::Relaxed) as f64 / frames;
    let write_f = STATS[WRITE_FULFILLS].swap(0, Ordering::Relaxed) as f64 / frames;
    let kern_f = STATS[KERNEL_FULFILLS].swap(0, Ordering::Relaxed) as f64 / frames;
    let known = STATS[KNOWN_READ_BLOCKS].load(Ordering::Relaxed);
    if pieces == 0.0 && faults == 0.0 && known == 0 {
        return;
    }
    tracing::info!(
        "demand landing (VICTORY_DEMAND_LAND={}): per frame {:.1} C2 pieces deferred ({:.1} KB), {:.2} demand fulfills ({:.1} KB: title {:.2}, safecopy {:.2}, pin {:.2}, write {:.2}, kernel {:.2}), {} blocks known CPU-read",
        u8::from(on()),
        pieces,
        bytes / 1024.0,
        faults,
        fault_bytes / 1024.0,
        title_f,
        safe_f,
        pin_f,
        write_f,
        kern_f,
        known,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc_low_block() -> *mut std::ffi::c_void {
        use windows_sys::Win32::System::Memory::{
            MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_RESERVE,
            MemExtendedParameterAddressRequirements, PAGE_READWRITE, VirtualAlloc2,
        };
        let mut req = MEM_ADDRESS_REQUIREMENTS {
            LowestStartingAddress: std::ptr::null_mut(),
            HighestEndingAddress: ((1usize << 32) - 1) as *mut _,
            Alignment: 0,
        };
        let mut param = MEM_EXTENDED_PARAMETER::default();
        param.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
        param.Anonymous2.Pointer = (&raw mut req).cast();
        // SAFETY: allocating a temporary 64 KiB private block below 4 GiB for tests.
        unsafe { VirtualAlloc2(std::ptr::null_mut(), std::ptr::null(), BLOCK, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE, &mut param, 1) }
    }

    #[test]
    fn pin_prevents_parking_and_forgets_cleanly() {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, PAGE_NOACCESS, PAGE_READWRITE, VirtualFree, VirtualProtect};
        let mem = alloc_low_block();
        if mem.is_null() {
            return;
        }
        let addr = mem as usize;
        let block = addr / BLOCK;
        assert!(block < MAX_BLOCKS);
        forget(addr, BLOCK);
        {
            let _pin = pin_range(addr, BLOCK);
            assert_ne!(PINNED[block].load(Ordering::Relaxed), 0);
        }
        assert_eq!(PINNED[block].load(Ordering::Relaxed), 0);

        let mut old = 0u32;
        // SAFETY: `mem` is a valid 64 KiB allocation from `alloc_low_block`.
        unsafe {
            VirtualProtect(mem, BLOCK, PAGE_NOACCESS, &mut old);
        }
        set_bit(&GUARDED, block);
        assert!(has_any(&GUARDED, block, block + 1));
        forget(addr, 0);
        assert!(!has_any(&GUARDED, block, block + 1));
        assert!(!has_bit(&CPU_READ, block));
        // SAFETY: restoring access and releasing `mem`.
        unsafe {
            VirtualProtect(mem, BLOCK, PAGE_READWRITE, &mut old);
            VirtualFree(mem, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn fault_handler_fulfills_and_promotes_block_below_4gib() {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        crate::watch::install();
        let mem = alloc_low_block();
        if mem.is_null() {
            return;
        }
        let addr = mem as usize;
        let block = addr / BLOCK;
        forget(addr, BLOCK);
        // SAFETY: `mem` is writable; initialize a sentinel byte before parking.
        unsafe {
            (addr as *mut u8).write(0x42);
        }
        let mut backing = vec![0x42u8; BLOCK];
        let shadow = crate::shadow::Shadow::unbacked(addr as u64, backing.as_mut_ptr(), BLOCK as u64, true);
        if on() {
            assert!(park(&shadow, 0, BLOCK, 1), "park protects low RW block");
            assert!(has_bit(&GUARDED, block));
            // Real CPU read faults on `PAGE_NOACCESS`, enters `watch::on_write` -> `demand::on_fault`,
            // unprotects, promotes `CPU_READ`, and resumes.
            // SAFETY: `watch::on_write` restores read access transparently on fault.
            let got = unsafe { (addr as *const u8).read_volatile() };
            assert_eq!(got, 0x42);
            assert!(!has_bit(&GUARDED, block));
            assert!(has_bit(&CPU_READ, block));
            assert!(!park(&shadow, 0, BLOCK, 2), "promoted block is no longer parked");
        }
        forget(addr, BLOCK);
        drop(shadow);
        // SAFETY: freeing `mem`.
        unsafe { VirtualFree(mem, 0, MEM_RELEASE) };
    }

    #[test]
    fn cpu_read_bitmap_is_isolated_per_block() {
        let a = 1234usize;
        let b = 1235usize;
        mark_cpu_read(a);
        assert!(has_bit(&CPU_READ, a));
        assert!(!has_bit(&CPU_READ, b));
        clear_bit(&CPU_READ, a);
    }
}
