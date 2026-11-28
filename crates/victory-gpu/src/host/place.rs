//! The title's buffer copies, done on the card at their place in the list
//! (`VICTORY_COPIES_IN_PLACE`, on by default; `0`: all at submission, on the
//! CPU, as before).
//!
//! A buffer copy was done in title memory when its list was submitted, before
//! any draw of the submission ran, and the host copies refreshed after it.
//! That is another order than the list's: draws ahead of the copy read what
//! it put; of two copies to the same bytes with a draw behind each, both
//! draws read the second; a source that a dispatch of the same submission
//! writes first was read as it was before it; and a destination the
//! submissions in the air still read changed under them (where its host copy
//! has one side only, `crate::shadow::card_all`).
//!
//! Now the list copies, in the destination's host copy: out of the source's
//! host copy where shaders write that one, else out of upload memory of the
//! list that takes the source's bytes when the list is submitted (a source
//! shaders never wrote is the title's CPU's, or a copy's put there earlier in
//! the submission). What is written there is a shader's write from then on:
//! the bytes are declared, and what the card made of them is put into title
//! memory before the title is told of the submission.
//!
//! Title memory takes the copy at submission as well, as it did: fills of
//! upload memory, textures and host copies that lie over the same bytes read
//! it there. The reference of the destination's host copy takes the same
//! bytes ([`Shadow::placed_copy`]), so what the landing finds is what the card
//! made of them afterwards (a counter copied to zero and counted up; a source
//! that a shader wrote in between), and nothing is sent up over it. Where
//! flights in the air may write the source or the destination this is left to
//! the landing: what they wrote is not in title memory yet, and what they
//! will put there must not stand over this copy's bytes. If a step of the
//! submission reads the destination on the CPU, it waits for them instead
//! ([`Placed::waits`]).
//!
//! Copies to memory that has no host copy yet are done as before: nothing on
//! the card has read or written it. So are those of more than 8 MiB out of
//! memory shaders do not write (streaming).

use super::*;
use crate::shadow::Shadow;

pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPIES_IN_PLACE").is_none_or(|v| v != "0"))
}

/// The most bytes copied out of memory shaders do not write.
const MOST: usize = 8 << 20;

/// Since the last frame-rate line: copies at their place, their bytes, those
/// out of a host copy, texture copies out of one, and constants read on the
/// card behind such copies ([`HostList::placed_constants`]).
pub(super) static STATS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// The destinations of copies at their place of this frame and the one
/// before (from, to, frame): constants read out of them are read on the card
/// ([`HostList::placed_constants`]).
static LATELY: Mutex<Vec<(usize, usize, u64)>> = Mutex::new(Vec::new());
/// Whether [`LATELY`] may hold one (most constants are not looked up).
static ANY_LATELY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn note_placed(dst: usize, len: usize) {
    // Bytes the card writes there and lands later: not constants read in
    // place (`crate::port`) until they have landed.
    crate::port::copy_bound(dst, len);
    let frame = crate::frame::current();
    let mut lately = LATELY.lock().unwrap_or_else(|e| e.into_inner());
    lately.retain(|e| e.2 + 1 >= frame);
    lately.push((dst, dst + len, frame));
    ANY_LATELY.store(true, Ordering::Relaxed);
}

/// Whether a copy at its place lately wrote a byte of `[from, to)`.
fn placed_lately(from: usize, to: usize) -> bool {
    if !ANY_LATELY.load(Ordering::Relaxed) {
        return false;
    }
    let frame = crate::frame::current();
    let mut lately = LATELY.lock().unwrap_or_else(|e| e.into_inner());
    lately.retain(|e| e.2 + 1 >= frame);
    if lately.is_empty() {
        ANY_LATELY.store(false, Ordering::Relaxed);
    }
    lately.iter().any(|e| e.0 < to && from < e.1)
}

/// The bytes of a buffer of ours that [`HostList::placed_constants`] copies
/// constants into, one after the other (each 256-aligned; a root view reads
/// 64 KiB, so a buffer holds sixteen of those).
const CONSTANTS_ROOM: u64 = 1 << 20;

/// A descriptor slot pointed at constants on the card when the list is
/// submitted (the draw pointed it at upload memory when it was recorded).
struct PointSlot {
    heap: usize,
    index: u32,
    buffer: ID3D12Resource,
    gpu: u64,
    len: u32,
}

// SAFETY: D3D12 objects are free-threaded; the slot is written by the
// submitting thread.
unsafe impl Send for PointSlot {}
unsafe impl Sync for PointSlot {}

impl PointSlot {
    fn point(&self) {
        // (A heap that is gone: nothing reads the slot.)
        crate::descriptor::constant_slot(self.heap, self.index, &self.buffer, self.gpu, self.len);
    }
}

/// What the CPU does at submission for a copy at its place in a list.
pub(super) struct Placed {
    pub(super) dst: usize,
    pub(super) src: usize,
    pub(super) len: usize,
    /// The destination's host copy.
    pub(super) shadow: std::sync::Arc<Shadow>,
    /// Upload memory the list copies from, to be given the source's bytes.
    from: Option<usize>,
}

impl Placed {
    /// Whether flights that write `written` are waited for.
    pub(super) fn waits(&self, written: &[(usize, usize)]) -> Waits {
        let source = touches(written, self.src, self.src + self.len);
        if source && self.from.is_some() {
            return Waits::Yes;
        }
        match source || touches(written, self.dst, self.dst + self.len) {
            true => Waits::ForReaders,
            false => Waits::No,
        }
    }

    /// At submission, in the order of the steps: the list's upload memory
    /// takes the source's bytes and, if `free` (no flight in the air may write
    /// source or destination), title memory at the destination does. Gives
    /// the bytes if it took them.
    pub(super) fn prepare(&self, free: bool) -> Option<Vec<u8>> {
        if !free && self.from.is_none() {
            return None;
        }
        let mut bytes = vec![0u8; self.len];
        // (What is not there reads as zeros, as any copy's source.)
        copy_mapped(bytes.as_mut_ptr(), self.src as *const u8, self.len, false);
        if let Some(to) = self.from {
            // SAFETY: the list's own upload memory of that many bytes.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to as *mut u8, self.len) };
        }
        if !free {
            return None;
        }
        copy_mapped(self.dst as *mut u8, bytes.as_ptr(), self.len, true);
        crate::watch::wrote(self.dst, self.len);
        // Title memory that is not all there was left alone.
        let mut there = vec![0u8; self.len];
        (read_into(self.dst as *const u8, &mut there) && there == bytes).then_some(bytes)
    }
}

/// Whether a copy waits for flights before its submission is prepared: not;
/// yes, as it takes a source on the CPU that they may write; or only if
/// another step of the submission reads its destination on the CPU.
pub(super) enum Waits {
    No,
    ForReaders,
    Yes,
}

/// Whether `step` reads, on the CPU at submission, one of `ranges` (the
/// destinations of copies at their place that stand ahead of it in the
/// submission: a step that stands before such a copy never sees its bytes,
/// with the flights waited for or not, so it is not in their way).
pub(super) fn reads(step: &Step, ranges: &[(usize, usize)]) -> bool {
    let meets = |from: usize, to: usize| from < to && ranges.iter().any(|r| r.0 < to && from < r.1);
    match step {
        &Step::Copy(_, src, len) | &Step::Fill(_, src, len, ..) => meets(src, src.saturating_add(len)),
        Step::Texture(staging) => {
            let (at, len) = staging.memory();
            meets(at, at.saturating_add(len))
        }
        &Step::Plane((at, len), _) => meets(at, at.saturating_add(len)),
        &Step::Host(_, (from, to)) => meets(from, to),
        Step::Placed(p) => p.from.is_some() && meets(p.src, p.src.saturating_add(p.len)),
        _ => false,
    }
}

enum CopyWait {
    Before,
    Between,
    After,
}

/// Orders the accesses on both sides of a placed copy.
///
/// # Safety
/// An open list.
#[track_caller]
unsafe fn wait_for_copy(host: &Host, list: &ID3D12GraphicsCommandList, point: CopyWait) {
    static ENHANCED: OnceLock<bool> = OnceLock::new();
    let enhanced = *ENHANCED.get_or_init(|| {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
        // SAFETY: a feature structure of its size.
        let known = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS12,
                (&raw mut o).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS12>() as u32,
            )
        };
        known.is_ok() && o.EnhancedBarriersSupported.as_bool()
    });
    // SAFETY: the caller's.
    unsafe {
        if enhanced && let Ok(enhanced) = list.cast::<ID3D12GraphicsCommandList7>() {
            let mut barrier = D3D12_GLOBAL_BARRIER {
                SyncBefore: D3D12_BARRIER_SYNC_ALL,
                SyncAfter: D3D12_BARRIER_SYNC_ALL,
                AccessBefore: D3D12_BARRIER_ACCESS_COMMON,
                AccessAfter: D3D12_BARRIER_ACCESS_COMMON,
            };
            if crate::rf_x7::narrow() {
                let copy = D3D12_BARRIER_ACCESS_COPY_SOURCE | D3D12_BARRIER_ACCESS_COPY_DEST;
                if matches!(point, CopyWait::Between | CopyWait::After) {
                    // The completed copy must finish both its reads and writes
                    // before subsequent users, including a writer of its source.
                    barrier.SyncBefore = D3D12_BARRIER_SYNC_COPY;
                    barrier.AccessBefore = copy;
                }
                if matches!(point, CopyWait::Before | CopyWait::Between) {
                    // Earlier accesses can come from any pipeline, but the
                    // operation this wait protects is a buffer copy.
                    barrier.SyncAfter = D3D12_BARRIER_SYNC_COPY;
                    barrier.AccessAfter = copy;
                }
                crate::rf_x7::mark_scoped_global();
            }
            let all = [barrier];
            crate::rf_x2::global_barrier(list, &enhanced, &[D3D12_BARRIER_GROUP { // rf-x2
                Type: D3D12_BARRIER_TYPE_GLOBAL,
                NumBarriers: 1,
                Anonymous: D3D12_BARRIER_GROUP_0 {
                    pGlobalBarriers: all.as_ptr(),
                },
            }]);
            return;
        }
        let alias = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: std::mem::ManuallyDrop::new(None),
                    pResourceAfter: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        let uav = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        crate::rf_x2::barriers(list, "vx:barrier.place", &[alias, uav]); // rf-x2
    }
}

/// The host copy of `[src, src + len)` if shaders write it: what it holds at
/// a place in a list is not what title memory holds at submission.
fn shaders_own(src: usize, len: usize) -> Option<std::sync::Arc<Shadow>> {
    crate::shadow::peek(src as u64, len as u64).filter(|s| s.written() && !s.in_place())
}

impl HostList {
    /// Copies `len` bytes of title memory at `src` to `dst`, at this place of
    /// the list (see the file). `false` if it is to be done at submission.
    pub(crate) fn place_copy(&self, dst: usize, src: usize, len: usize) -> bool {
        if !on() {
            return false;
        }
        let Some(host) = host() else {
            return false;
        };
        let Some(to) = crate::shadow::peek(dst as u64, len as u64).filter(|s| !s.in_place()) else {
            return false;
        };
        let own = shaders_own(src, len);
        let scratch = match &own {
            Some(_) => None,
            None if len <= MOST => match self.scratch_at(len as u64) {
                Some(scratch) => Some(scratch),
                None => return false,
            },
            None => return false,
        };
        let at = to.offset(dst as u64);
        let _rf_x2 = crate::rf_x2::range(&self.list, "vx:coh.place"); // rf-x2
        // What the title's CPU wrote around and under it so far goes first.
        to.refresh(self, dst as u64, len as u64);
        // SAFETY (both): an open list; live buffers (the list holds the
        // copies and its scratch memory), ranges inside them.
        self.rf_x7_other_work();
        unsafe { wait_for_copy(host, &self.list, CopyWait::Before) };
        match (&own, &scratch) {
            (Some(from), _) => {
                from.refresh(self, src as u64, len as u64);
                let offset = from.offset(src as u64);
                if from.buffer.as_raw() == to.buffer.as_raw() {
                    let Some(between) = self.between(host, len as u64, Kind::UavCopyAlias) else {
                        return false;
                    };
                    unsafe {
                        crate::rf_x7::work(&self.list, None);
                        self.list.CopyBufferRegion(&between, 0, &from.buffer, offset, len as u64);
                        crate::rf_x2::barriers(&self.list, "vx:barrier.place", &[transition(&between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]); // rf-x2
                        wait_for_copy(host, &self.list, CopyWait::Between);
                        crate::rf_x7::work(&self.list, None);
                        self.list.CopyBufferRegion(&to.buffer, at, &between, 0, len as u64);
                    }
                } else {
                    crate::rf_x7::work(&self.list, None);
                    unsafe { self.list.CopyBufferRegion(&to.buffer, at, &from.buffer, offset, len as u64) };
                }
                STATS[2].fetch_add(1, Ordering::Relaxed);
            }
            (None, Some((from, offset, _))) => unsafe {
                crate::rf_x7::work(&self.list, None);
                self.list.CopyBufferRegion(&to.buffer, at, from, *offset, len as u64);
            },
            (None, None) => return false,
        }
        unsafe { wait_for_copy(host, &self.list, CopyWait::After) };
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(len as u64, Ordering::Relaxed);
        to.gpu_writes(self, dst as u64, len as u64);
        note_placed(dst, len);
        let mut deferred = self.deferred();
        deferred.copies.push(Step::Placed(Placed {
            dst,
            src,
            len,
            shadow: to,
            from: scratch.map(|s| s.2 as usize),
        }));
        deferred.title_written();
        self.changed_something();
        true
    }

    /// For a copy of `[src, src + len)` into a texture: a buffer (readable by
    /// copies) that holds what shaders wrote there, at this place of the list.
    /// `None` where shaders do not write it.
    pub(crate) fn place_source(&self, src: usize, len: usize) -> Option<ID3D12Resource> {
        if !on() {
            return None;
        }
        let host = host()?;
        let from = shaders_own(src, len)?;
        let between = self.between(host, len as u64, Kind::UavTextureSupport)?;
        let _rf_x2 = crate::rf_x2::range(&self.list, "vx:coh.place"); // rf-x2
        from.refresh(self, src as u64, len as u64);
        STATS[3].fetch_add(1, Ordering::Relaxed);
        // SAFETY: an open list; live buffers, ranges inside them.
        unsafe {
            self.rf_x7_other_work();
            wait_for_copy(host, &self.list, CopyWait::Before);
            crate::rf_x7::work(&self.list, None);
            self.list.CopyBufferRegion(&between, 0, &from.buffer, from.offset(src as u64), len as u64);
            crate::rf_x2::barriers(&self.list, "vx:barrier.place", &[transition(&between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]); // rf-x2
            wait_for_copy(host, &self.list, CopyWait::After);
        }
        Some(between)
    }

    /// Constants of `len` bytes at `src` that a copy at its place lately wrote
    /// a byte of, read at this place of the list: a buffer of ours and the
    /// card address they are at (256-aligned), copied there on the card out of
    /// the host copy that holds them. `None`: read them as before, on the CPU
    /// at submission (a fill).
    ///
    /// Why: with flights in the air that may write the copy's source or
    /// destination, the copy is not put into title memory at submission
    /// ([`Placed::prepare`]: it lands later), so a fill behind it that read it
    /// on the CPU waited for those flights to land: 0.8 waits and 18.6 ms a
    /// frame in a chase (run play-20261127-104020, "a step that reads where a
    /// copy at its place writes"). Read here, on the card, the constants are
    /// what the copy and every write ahead of them on the queue left there,
    /// as for any view of a buffer's host copy; the title's own bytes around
    /// them are brought up to date by the refresh, as for those
    /// (`stale_under_flights` looks at it like at any other).
    pub(crate) fn placed_constants(&self, src: usize, len: usize) -> Option<(ID3D12Resource, u64)> {
        if !on() || len == 0 || !placed_lately(src, src + len) {
            return None;
        }
        let size = (len as u64).next_multiple_of(256);
        if size > CONSTANTS_ROOM {
            return None;
        }
        let host = host()?;
        let from = shaders_own(src, len)?;
        let (start, span) = from.span();
        if (src as u64) < start || src as u64 + len as u64 > start + span {
            return None;
        }
        let read = D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let held = self.deferred().constants.clone();
        let (buffer, at, fresh) = match held {
            Some((buffer, used)) if used + size <= CONSTANTS_ROOM => (buffer, used, false),
            _ => (self.between(host, CONSTANTS_ROOM, Kind::UavPlacedConstants)?, 0, true),
        };
        from.refresh(self, src as u64, len as u64);
        // SAFETY: an open list; live buffers (the list keeps both), ranges
        // inside them. A new buffer is in the common state and promoted by
        // the copy; one used before was left readable below.
        unsafe {
            let _rf_x2 = crate::rf_x2::range(&self.list, "vx:coh.place"); // rf-x2
            self.rf_x7_other_work();
            wait_for_copy(host, &self.list, CopyWait::Before);
            if !fresh {
                crate::rf_x2::barriers(&self.list, "vx:barrier.place", &[transition(&buffer, read, D3D12_RESOURCE_STATE_COPY_DEST)]); // rf-x2
            }
            crate::rf_x7::work(&self.list, None);
            self.list.CopyBufferRegion(&buffer, at, &from.buffer, from.offset(src as u64), len as u64);
            crate::rf_x2::barriers(&self.list, "vx:barrier.place", &[transition(&buffer, D3D12_RESOURCE_STATE_COPY_DEST, read)]); // rf-x2
        }
        self.deferred().constants = Some((buffer.clone(), at + size));
        STATS[4].fetch_add(1, Ordering::Relaxed);
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() } + at;
        Some((buffer, gpu))
    }

    /// [`HostList::placed_constants`] for a descriptor slot of a title heap
    /// (heap address, slot) that the draw points at upload memory: the slot
    /// is pointed at them when the list is submitted. Whether it was done.
    pub(crate) fn place_constants_slot(&self, src: usize, len: usize, slot: (usize, u32)) -> bool {
        let Some((buffer, gpu)) = self.placed_constants(src, len) else {
            return false;
        };
        let point = PointSlot {
            heap: slot.0,
            index: slot.1,
            buffer,
            gpu,
            len: len as u32,
        };
        self.defer_host(move || point.point());
        true
    }

    /// A buffer on the card for this recording alone.
    fn between(&self, host: &Host, len: u64, kind: Kind) -> Option<ID3D12Resource> {
        let buffer = host.uav_buffer_for(len.next_multiple_of(1 << 16), kind)?;
        name(&buffer, || "bytes between copies".into());
        self.keep(&buffer);
        Some(buffer)
    }
}

#[cfg(test)]
mod tests {
    /// What copies with two sides cost to make (ignored: a measurement for
    /// `VICTORY_CARD_ALL`): buffers of 64 KiB on the card and in the PC's
    /// memory.
    #[test]
    #[ignore]
    fn small_buffers_cost() {
        let Some(host) = super::host() else {
            return;
        };
        const N: usize = 1000;
        let used = || host.video_memory().map_or(0, |m| m[1] >> 20);
        let before = used();
        let t = std::time::Instant::now();
        let card: Vec<_> = (0..N).map(|_| host.uav_buffer(1 << 16)).collect();
        let card_us = t.elapsed().as_micros() as f64 / N as f64;
        let after = used();
        let t = std::time::Instant::now();
        let pc: Vec<_> = (0..N).map(|_| host.shared_buffer(1 << 16)).collect();
        let pc_us = t.elapsed().as_micros() as f64 / N as f64;
        println!(
            "{N} buffers of 64 KiB: on the card {card_us:.0} us each, {} MB of its memory; in the PC's memory {pc_us:.0} us each",
            after.saturating_sub(before)
        );
        drop((card, pc));
    }
}
