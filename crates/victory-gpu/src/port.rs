//! Renderer port #1, as the title-memory home of the unified memory model
//! (docs/performance/UNIFIED_MEMORY_ARCHITECTURE.md): the title's 32-bit
//! graphics heap opened in place, and constant buffers in it that no shader
//! writes read by the card where the title wrote them. Always on.
//!
//! What the title does (game_scarlett_master.exe, the routine at image
//! +0x1e86440, reached at its start): one `XMemVirtualAlloc` of the five
//! sizes it is given together (kind 0x42000: a reservation of 64 KiB
//! pages, flags 7: graphics memory below 4 GiB, 185 MB at 0x34310000 in
//! every run so far), split into five placeholders (`VirtualFree` with
//! `MEM_PRESERVE_PLACEHOLDER`), each turned into graphics memory of its own
//! protection (`XMemVirtualAlloc`, kind 0x20406000, flags 0x27); then ONE
//! `XMemAllocatePhysicalPages` for all their pages and five
//! `XMemMapPhysicalPages`, in order, each from the next page of that one
//! array. `xmem` makes one section of all the pages (and gives them their
//! slices in address order, `xmem::arrange`), so the whole reservation is
//! one stretch of one section: views that follow one another in memory and
//! in the section alike. The title's 174 MB buffer (where its ring of
//! constants lies, and what the skinning passes write through one view of
//! all of it) is in it; it is never tile-mapped (`CopyPageMappings` maps
//! the GPU heap's pages, not these).
//!
//! Two homes. The 174 MB buffer is mostly geometry the card reads by the
//! hundred megabytes a list, and skinning writes it through one view of the
//! whole: it stays the card copy `crate::shadow` makes (in place, all of it
//! in the PC's memory, the card read and wrote it over the bus and was
//! slower overall on a 12 GB card). A view cannot be backed in part here
//! and in part on the card: tile mappings into a heap opened on title
//! memory remove the device (`crate::inplace`). So the split is by access:
//! a constant buffer view (root or in a table) of bytes no shader writes
//! ([`constants`]) is bound to the title's memory itself, through one
//! buffer placed over the whole stretch; every other access binds the card
//! copy, as before. For those constants nothing is refreshed, filled into
//! upload memory, sent up, merged or waited for: the card reads the title's
//! bytes when it runs, as the console's does.
//!
//! Alignment (2026-11-27, branch `perf/raw-constants`): a constant buffer
//! VIEW needs its address on 256 bytes on the host; the title places its
//! constants on 16, so 6,185 of 6,556 views a frame could not be bound here
//! (run play-20261127-111731). The translation now reads every constant
//! buffer as raw bytes (`victory_shader::emit::HostShader::root_cbvs`), and
//! one in a view slot as a structured buffer of dwords (the slot holds a
//! structured view, stride 4): both need the address on 4 bytes only
//! ([`ALIGN`]), so every constant buffer of the title can be read here. (A
//! first try read tables' through the slots' raw twins: raw views begin on
//! 16, and NVIDIA's heaps have no room for twins, run
//! play-20261127-114321: 108 read in place, 2,841-4,075 not.) The caller
//! says what binds it ([`Use`]), for the counts.
//!
//! "No shader writes it", per 4 KiB page ([`PAGE_MARKS`]): no read-back
//! into title memory was ever recorded for it ([`gpu_bound`]: for good), no
//! title buffer copy into it was recorded in the last [`COPY_FRAMES`]
//! frames ([`copy_bound`]: such a copy may be done on the card, in front of
//! the lists, `crate::host`'s `card_steps`, or at its place in the list,
//! and lands in title memory after the flight), and no landing put shader
//! output there in the last [`LANDED_FRAMES`] frames
//! (`crate::watch::landed_lately`, the window `fill_is_titles` has used by
//! default: skinning outputs land every frame and stay the card's; a ring
//! page a copy wrote once comes back).
//!
//! Why our own view: a heap opened from an address holds the pages it was
//! opened on: the title could not unmap, decommit or protect them while it
//! stands. Opened on our view, the title's own views stay free to change.
//! When the title maps other pages at such an address, the region is
//! forgotten (`crate::inplace::title_memory_freeing`): new binds take the
//! card copy; lists and slots bound before hold the region's buffer, which
//! still shows our view (shortcut: never unmapped, a few hundred MB of
//! address space at most), so nothing reads freed memory.
//!
//! What it assumes: the title does not write constants a submission still
//! in the air reads (the console's GPU reads them at execution too), and a
//! shader's first write to a page is not read through a constant buffer
//! before it lands (the same as `crate::host`'s `fill_is_titles`).

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::Win32::System::Memory as mem;

use crate::inplace::{cover, stretch};

const K64: u64 = 64 << 10;

/// The ported memory lies below this: the title's graphics memory for
/// 32-bit addresses, which only that heap is.
const LOW: u64 = 1 << 32;

/// The most bytes of one region: its pages are locked into the PC's memory
/// while the heap stands.
const MOST: u64 = 512 << 20;

/// The most bytes a constant buffer view reads (4,096 vectors).
const CONSTANTS_MOST: u64 = 64 << 10;

/// The alignment a constant buffer needs to be read in place: a root one is
/// a raw root SRV (`victory_shader::emit::HostShader::root_cbvs`), one in a
/// view slot a structured view of dwords
/// (`crate::descriptor::host::HostHeap::constant_at`): both begin on any 4
/// bytes.
pub(crate) const ALIGN: u64 = 4;

/// What binds a constant buffer asked about ([`constants`]), for the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Use {
    /// A root view (`crate::draw` `root_view`).
    Root,
    /// A table's slot made afresh for a draw (`crate::draw` `tables`).
    Table,
    /// A title slot made by the descriptor mirror
    /// (`crate::descriptor::host` `make_twinned`).
    Mirror,
}

/// Frames before memory that could not be opened is asked about again
/// (pages of a stretch may not be committed yet).
const RETRY_FRAMES: u64 = 600;

/// 4 KiB pages below [`LOW`].
const PAGES: usize = (LOW / 4096) as usize;

/// Frames a page a title buffer copy was recorded into is not read in place
/// ([`copy_bound`]): the copy's flight lands within them (the relay keeps
/// one or two frames in the air; `crate::host`'s `placed_lately` trusts
/// two).
const COPY_FRAMES: u64 = 8;

/// Frames a page shader output landed in is not read in place.
const LANDED_FRAMES: u64 = 600;

/// [`PAGE_MARKS`]' mark of a page the card writes for good.
const FOR_GOOD: u32 = u32::MAX;

/// A stretch of title memory opened as a heap through a view of ours, and
/// one buffer placed over all of it.
struct Region {
    start: u64,
    end: u64,
    whole: ID3D12Resource,
    _heap: ID3D12Heap,
}

// SAFETY: D3D12 heaps and resources are free-threaded.
unsafe impl Send for Region {}
// SAFETY: as above; nothing in it changes once made.
unsafe impl Sync for Region {}

static REGIONS: RwLock<Vec<Region>> = RwLock::new(Vec::new());

/// The lowest start and highest end of every region (both 0: none), for a
/// quick look on the paths that run thousands of times a frame.
static BOUNDS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Per 64 MiB of memory below [`LOW`]: the frame (plus one) memory there
/// last could not be opened, 0 never.
static TRIED: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// The host refused to open a stretch: none is asked for again.
static REFUSED: AtomicBool = AtomicBool::new(false);

/// Per 4 KiB page below [`LOW`]: 0 nothing; [`FOR_GOOD`] a write on the
/// card is bound for it ([`gpu_bound`]); else the frame (low 32 bits, at
/// least 1) a title buffer copy into it was last recorded ([`copy_bound`]).
/// 4 MB of address space, of which only the pages written are memory.
static PAGE_MARKS: [AtomicU32; PAGES] = [const { AtomicU32::new(0) }; PAGES];

// What [`STATS`] holds. Since the start: regions opened, their bytes; asks
// that could not open one because the memory is not one stretch of a
// section, not committed, no view of it could be made, the host refused, no
// buffer could be placed; regions forgotten because the title mapped other
// pages there. Then, since the last line: constant buffer views bound in
// place and their bytes; not, because off 4 bytes (as a root view), because
// a shader writes a page of it, because outside every region; fills of
// upload memory read out of ported memory on the CPU and their bytes; title
// buffer copies on the CPU that touch it and their bytes; small stream views
// in place and their bytes; not in place because off 4 bytes in a table's
// slot, in a title slot (mirror); in place in a table's slot, in a title
// slot (the rest of [`READ`] are root views).
const OPENED: usize = 0;
const OPENED_BYTES: usize = 1;
const NOT_STRETCH: usize = 2;
const NOT_COMMITTED: usize = 3;
const NO_VIEW: usize = 4;
const HOST_REFUSED: usize = 5;
const NO_BUFFER: usize = 6;
const FORGOTTEN: usize = 7;
const READ: usize = 8;
const READ_BYTES: usize = 9;
const MISALIGNED: usize = 10;
const SHADERS: usize = 11;
const OUTSIDE: usize = 12;
const FILLS: usize = 13;
const FILL_BYTES: usize = 14;
const COPIES: usize = 15;
const COPY_BYTES: usize = 16;
const STREAM_READ: usize = 17;
const STREAM_READ_BYTES: usize = 18;
const MISALIGNED_TABLE: usize = 19;
const MISALIGNED_MIRROR: usize = 20;
const READ_TABLE: usize = 21;
const READ_MIRROR: usize = 22;
static STATS: [AtomicU64; 23] = [const { AtomicU64::new(0) }; 23];

/// A constant buffer of `len` bytes at title address `address`, read in
/// place: the buffer to bind and hold (the caller's list and slot keep it),
/// and the host address of `address` in it (on [`ALIGN`] bytes, what every
/// view a constant buffer is read through needs). `None`: bind the card copy
/// or fill upload memory, as before. In place only if the view's bytes
/// (`len` rounded up to 256, 64 KiB at most: what a CBV of it shows) lie in
/// a region and no shader writes any of their pages. `by`: what binds it.
pub(crate) fn constants(address: u64, len: u64, by: Use) -> Option<(ID3D12Resource, u64)> {
    if len == 0 || address >= LOW {
        return None;
    }
    let misaligned = match by {
        Use::Root => MISALIGNED,
        Use::Table => MISALIGNED_TABLE,
        Use::Mirror => MISALIGNED_MIRROR,
    };
    let size = len.min(CONSTANTS_MOST).next_multiple_of(256);
    let end = address.checked_add(size)?;
    if end > LOW {
        return None;
    }
    // Regions begin on 64 KiB and their buffer with them: the host address
    // is aligned as the title's is.
    if address % ALIGN != 0 {
        if touches(address as usize, size as usize) {
            STATS[misaligned].fetch_add(1, Ordering::Relaxed);
        }
        return None;
    }
    let Some((whole, start)) = region_holding(address, end) else {
        STATS[OUTSIDE].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    // SAFETY: a live buffer.
    let gpu = unsafe { whole.GetGPUVirtualAddress() } + (address - start);
    if gpu % ALIGN != 0 {
        STATS[misaligned].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    if !written_by_no_shader(address, size) {
        STATS[SHADERS].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    STATS[READ].fetch_add(1, Ordering::Relaxed);
    STATS[READ_BYTES].fetch_add(len.min(CONSTANTS_MOST), Ordering::Relaxed);
    match by {
        Use::Root => {}
        Use::Table => {
            STATS[READ_TABLE].fetch_add(1, Ordering::Relaxed);
        }
        Use::Mirror => {
            STATS[READ_MIRROR].fetch_add(1, Ordering::Relaxed);
        }
    }
    Some((whole, gpu))
}

/// Small (`<= 64 KiB`), CPU-written vertex/index window insideported memory
/// that is neither written by shaders/copies nor inside a VRAM-resident
/// (`carded`) buffer: the host resource and GPU address in place, or `None`.
pub(crate) fn stream_bytes(address: u64, len: u64) -> Option<(ID3D12Resource, u64)> {
    if len == 0 || len > CONSTANTS_MOST || address >= LOW {
        return None;
    }
    let end = address.checked_add(len)?;
    if end > LOW {
        return None;
    }
    // Constraint 2: keep static VRAM-resident (`carded`) geometry on the card,
    // while letting repeatedly CPU-written dynamic streams (`<= 64 KiB`) read in place.
    if crate::shadow::in_card_static(address, len) {
        return None;
    }
    let (whole, start) = region_holding(address, end)?;
    if !written_by_no_shader(address, len) {
        return None;
    }
    // SAFETY: a live buffer.
    let gpu = unsafe { whole.GetGPUVirtualAddress() } + (address - start);
    STATS[STREAM_READ].fetch_add(1, Ordering::Relaxed);
    STATS[STREAM_READ_BYTES].fetch_add(len, Ordering::Relaxed);
    Some((whole, gpu))
}

/// The pages below [`LOW`] of `[at, at + len)`, if any.
fn pages(at: usize, len: usize) -> Option<std::ops::RangeInclusive<usize>> {
    let (at, end) = (at as u64, (at as u64).saturating_add(len as u64).min(LOW));
    (len > 0 && at < end).then(|| (at / 4096) as usize..=((end - 1) / 4096) as usize)
}

/// [`PAGE_MARKS`]' mark of a copy recorded in `frame`: 1 to
/// [`FOR_GOOD`] - 1.
fn stamp(frame: u64) -> u32 {
    (frame % u64::from(FOR_GOOD - 1)) as u32 + 1
}

/// A write on the card bound for title memory `[at, at + len)` was
/// recorded (a read-back): its pages are never read in place again
/// ([`constants`]).
pub(crate) fn gpu_bound(at: usize, len: usize) {
    for page in pages(at, len).into_iter().flatten() {
        if PAGE_MARKS[page].load(Ordering::Relaxed) != FOR_GOOD {
            PAGE_MARKS[page].store(FOR_GOOD, Ordering::Relaxed);
        }
    }
}

/// A title buffer copy into `[at, at + len)` was recorded (done on the CPU
/// at submission, on the card in front of the lists, or at its place in
/// the list): its pages are not read in place for [`COPY_FRAMES`] frames,
/// until what the card did of it has landed.
pub(crate) fn copy_bound(at: usize, len: usize) {
    let now = stamp(crate::frame::current());
    for page in pages(at, len).into_iter().flatten() {
        let mark = &PAGE_MARKS[page];
        let was = mark.load(Ordering::Relaxed);
        if was != FOR_GOOD && was != now {
            mark.store(now, Ordering::Relaxed);
        }
    }
}

/// Whether no shader or copy on the card writes a page of `[at, at + len)`
/// (below [`LOW`], `len` > 0), as [`PAGE_MARKS`] and the landings tell.
fn written_by_no_shader(at: u64, len: u64) -> bool {
    let now = stamp(crate::frame::current());
    let marked = |page: usize| match PAGE_MARKS[page].load(Ordering::Relaxed) {
        0 => false,
        FOR_GOOD => true,
        copied => u64::from(now.wrapping_sub(copied)) <= COPY_FRAMES,
    };
    if pages(at as usize, len as usize).into_iter().flatten().any(marked) {
        return false;
    }
    !crate::watch::landed_lately(at as usize, len as usize, LANDED_FRAMES)
}

/// The region holding `[at, end)`: its whole buffer and start, opened if
/// none does and the memory may be asked about now.
fn region_holding(at: u64, end: u64) -> Option<(ID3D12Resource, u64)> {
    let hi = BOUNDS[1].load(Ordering::Relaxed);
    if hi != 0 && at >= BOUNDS[0].load(Ordering::Relaxed) && end <= hi {
        let regions = REGIONS.read().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = regions.iter().find(|r| r.start <= at && end <= r.end) {
            return Some((r.whole.clone(), r.start));
        }
    }
    if REFUSED.load(Ordering::Relaxed) {
        return None;
    }
    let cell = &TRIED[(at >> 26) as usize];
    let now = crate::frame::current();
    let tried = cell.load(Ordering::Relaxed);
    if tried != 0 && now < tried - 1 + RETRY_FRAMES {
        return None;
    }
    let host = crate::host::host()?;
    match region_for(&host.device, at, end) {
        Ok(found) => Some(found),
        Err((why_at, why)) => {
            cell.store(now + 1, Ordering::Relaxed);
            if why_at == HOST_REFUSED {
                REFUSED.store(true, Ordering::Relaxed);
            }
            let n = STATS[why_at].fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 4 || n.is_power_of_two() {
                tracing::warn!(
                    at = format_args!("{at:#x}"),
                    why,
                    "renderer port 1: title memory not opened in place, constants there are read from the card copy"
                );
            }
            None
        }
    }
}

/// Whether the existing port already opened this range through its own view.
/// This only observes regions. It never tries to open a heap.
pub(crate) fn range_open(at: u64, end: u64) -> bool {
    let hi = BOUNDS[1].load(Ordering::Relaxed);
    hi != 0 && at >= BOUNDS[0].load(Ordering::Relaxed) && end <= hi
        && REGIONS.read().unwrap_or_else(|e| e.into_inner()).iter().any(|r| r.start <= at && end <= r.end)
}

/// The region holding the pages of `[at, end)`, opened if none does: its
/// whole buffer and start. `Err` with the statistic to count and why.
fn region_for(device: &ID3D12Device, at: u64, end: u64) -> Result<(ID3D12Resource, u64), (usize, String)> {
    let mut regions = REGIONS.write().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = regions.iter().find(|r| r.start <= at && end <= r.end) {
        return Ok((r.whole.clone(), r.start));
    }
    let region = open_region(device, at & !(K64 - 1), end.max(at + 1).next_multiple_of(K64))?;
    let found = (region.whole.clone(), region.start);
    STATS[OPENED].fetch_add(1, Ordering::Relaxed);
    STATS[OPENED_BYTES].fetch_add(region.end - region.start, Ordering::Relaxed);
    regions.push(region);
    bounds(&regions);
    Ok(found)
}

/// Opens the stretch around `[at, end)` (64 KiB aligned) as a heap through
/// a view of ours of the same pages, with one buffer over all of it.
fn open_region(device: &ID3D12Device, at: u64, end: u64) -> Result<Region, (usize, String)> {
    let Some((first, bytes, section, offset)) = stretch(at as usize, (end - at) as usize) else {
        return Err((NOT_STRETCH, format!("not one stretch of a section: {:?}", cover(at as usize, (end - at) as usize))));
    };
    // The whole stretch, kept to what may be ported: the next ask in it
    // finds the region made.
    let (mut first, mut bytes, mut offset) = (first as u64, bytes as u64, offset);
    if first < LOW && first + bytes > LOW {
        bytes = LOW.saturating_sub(first);
    }
    if bytes > MOST {
        // Only what was asked for.
        offset += at - first;
        (first, bytes) = (at, end - at);
    }
    if bytes > MOST {
        return Err((NOT_STRETCH, format!("{bytes:#x} bytes, over the most of a region")));
    }
    if first > at || first + bytes < end || offset % K64 != 0 {
        return Err((NOT_STRETCH, format!("the stretch {first:#x}+{bytes:#x} does not hold it")));
    }
    // SAFETY: the section of a view the title has; the runtime keeps its
    // handle open while a view of it is there, and the range is inside it.
    let view = unsafe { mem::MapViewOfFile(section as _, mem::FILE_MAP_WRITE, (offset >> 32) as u32, offset as u32, bytes as usize) }.Value as usize;
    if view == 0 {
        return Err((NO_VIEW, format!("no view of the stretch: {}", std::io::Error::last_os_error())));
    }
    let unmap = || {
        // SAFETY: our own view, nothing opened on it any more.
        unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
    };
    if !committed(view, bytes as usize) {
        unmap();
        return Err((NOT_COMMITTED, "pages of the stretch are not committed".to_owned()));
    }
    let heap = match crate::inplace::open(device, crate::inplace::From::Part(view, bytes as usize)) {
        Ok(heap) => heap,
        Err(e) => {
            unmap();
            return Err((HOST_REFUSED, format!("refused by the host: {e}")));
        }
    };
    let whole = match crate::inplace::place(device, &heap, 0, bytes) {
        Ok(whole) => whole,
        Err(e) => {
            drop(heap);
            unmap();
            return Err((NO_BUFFER, format!("no buffer placed over the region: {e}")));
        }
    };
    crate::host::name(&whole, || format!("title memory {first:#x}+{bytes:#x} in place"));
    tracing::info!(
        start = format_args!("{first:#x}"),
        bytes,
        section = format_args!("{section:#x}"),
        offset = format_args!("{offset:#x}"),
        view = format_args!("{view:#x}"),
        "renderer port 1: a stretch of the title's graphics memory opened as a host heap through a view of ours"
    );
    // shortcut: the view stays mapped for good (the heap holds its pages
    // while anything names it).
    Ok(Region { start: first, end: first + bytes, whole, _heap: heap })
}

/// Whether every page of `[at, at + len)` is committed.
fn committed(at: usize, len: usize) -> bool {
    let mut here = at;
    while here < at + len {
        // SAFETY: plain data filled in by the query.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { mem::VirtualQuery(here as *const _, &mut info, size_of_val(&info)) } == 0 || info.State != mem::MEM_COMMIT {
            return false;
        }
        here = (info.BaseAddress as usize).saturating_add(info.RegionSize);
    }
    true
}

fn bounds(regions: &[Region]) {
    let lo = regions.iter().map(|r| r.start).min().unwrap_or(0);
    let hi = regions.iter().map(|r| r.end).max().unwrap_or(0);
    BOUNDS[0].store(lo, Ordering::Relaxed);
    BOUNDS[1].store(hi, Ordering::Relaxed);
}

/// The title maps other pages at `[at, at + len)`: the regions over it are
/// forgotten (lists and slots that hold their buffers keep them alive).
pub(crate) fn forget(at: usize, len: usize) {
    let (at, end) = (at as u64, (at as u64).saturating_add(len as u64));
    if BOUNDS[1].load(Ordering::Relaxed) <= at || BOUNDS[0].load(Ordering::Relaxed) >= end {
        return;
    }
    let mut regions = REGIONS.write().unwrap_or_else(|e| e.into_inner());
    let before = regions.len();
    regions.retain(|r| r.end <= at || r.start >= end);
    STATS[FORGOTTEN].fetch_add((before - regions.len()) as u64, Ordering::Relaxed);
    bounds(&regions);
}

/// Whether `[at, at + len)` touches ported memory (a quick look).
fn touches(at: usize, len: usize) -> bool {
    let hi = BOUNDS[1].load(Ordering::Relaxed);
    hi != 0 && (at as u64) < hi && (at as u64).saturating_add(len as u64) > BOUNDS[0].load(Ordering::Relaxed)
}

/// A fill of upload memory read `len` bytes at `src` on the CPU.
pub(crate) fn filled(src: usize, len: usize) {
    if touches(src, len) {
        STATS[FILLS].fetch_add(1, Ordering::Relaxed);
        STATS[FILL_BYTES].fetch_add(len as u64, Ordering::Relaxed);
    }
}

/// A title buffer copy was done on the CPU.
pub(crate) fn copied(dst: usize, src: usize, len: usize) {
    if touches(dst, len) || touches(src, len) {
        STATS[COPIES].fetch_add(1, Ordering::Relaxed);
        STATS[COPY_BYTES].fetch_add(len as u64, Ordering::Relaxed);
    }
}

/// For the frame-rate line: what is in place, what was bound there, and
/// what was not and why.
pub(crate) fn log(frames: f64) {
    let s: Vec<u64> = STATS[..READ].iter().map(|s| s.load(Ordering::Relaxed)).collect();
    let taken: Vec<f64> = STATS[READ..].iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    if s[OPENED] == 0 && s[NOT_STRETCH..=NO_BUFFER].iter().all(|&n| n == 0) {
        return;
    }
    let per = |at: usize| taken[at - READ];
    let stream_reads = per(STREAM_READ);
    let stream_bytes = per(STREAM_READ_BYTES) / 1024.0;
    tracing::info!(
        "renderer port 1: {} regions of the title's graphics memory in place ({} MB); per frame: {:.0} constant views read in place ({:.1} KB: \
         nothing refreshed, filled or waited for; {:.0} in tables' slots, {:.0} in title slots, the rest root views), {:.0} small stream views in place ({:.1} KB), \
         not in place: off 4 bytes {:.0} root, {:.0} in tables' slots, {:.0} in title slots, {:.0} on pages shaders write, {:.0} outside the regions; \
         on the CPU {:.0} fills read out of them ({:.1} KB), {:.1} buffer copies touching them ({:.1} KB); since the start not opened: \
         {} not one stretch of a section, {} not committed, {} no view, {} by the host, {} no buffer; {} regions forgotten (other pages mapped there)",
        s[OPENED],
        s[OPENED_BYTES] >> 20,
        per(READ),
        per(READ_BYTES) / 1024.0,
        per(READ_TABLE),
        per(READ_MIRROR),
        stream_reads,
        stream_bytes,
        per(MISALIGNED),
        per(MISALIGNED_TABLE),
        per(MISALIGNED_MIRROR),
        per(SHADERS),
        per(OUTSIDE),
        per(FILLS),
        per(FILL_BYTES) / 1024.0,
        per(COPIES),
        per(COPY_BYTES) / 1024.0,
        s[NOT_STRETCH],
        s[NOT_COMMITTED],
        s[NO_VIEW],
        s[HOST_REFUSED],
        s[NO_BUFFER],
        s[FORGOTTEN],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the title's 32-bit memory is read in place, and only views
    /// with bytes.
    #[test]
    fn only_memory_below_4_gib_is_read_in_place() {
        assert!(constants(0xe1c80_0000, 256, Use::Table).is_none());
        assert!(constants(0xffff_ff00, 0x1000, Use::Mirror).is_none());
        assert!(constants(0x3431_0000, 0, Use::Table).is_none());
        assert!(constants(u64::MAX - 255, 16, Use::Root).is_none());
        assert!(stream_bytes(0xe1c80_0000, 64).is_none());
        assert!(stream_bytes(0x3431_0000, 0).is_none());
        assert!(stream_bytes(0x3431_0000, CONSTANTS_MOST + 1).is_none());
        assert!(stream_bytes(0xffff_ffc0, 128).is_none());
    }

    /// A page a write on the card is bound for is the shaders' for good;
    /// its neighbours are not.
    #[test]
    fn pages_bound_for_shader_writes_are_not_read_in_place() {
        let at = 0x7ff0_0000u64;
        assert!(written_by_no_shader(at, 0x3000));
        gpu_bound(at as usize + 0x1100, 0x10);
        assert!(!written_by_no_shader(at, 0x3000));
        assert!(!written_by_no_shader(at + 0x1000, 0x100));
        assert!(written_by_no_shader(at, 0x1000));
        assert!(written_by_no_shader(at + 0x2000, 0x1000));
        // A copy recorded into a page: not read in place this frame.
        assert!(written_by_no_shader(at + 0x4000, 0x100));
        copy_bound(at as usize + 0x4080, 0x20);
        assert!(!written_by_no_shader(at + 0x4000, 0x100));
        assert!(written_by_no_shader(at + 0x5000, 0x100));
        // Past the memory read in place: nothing noted, nothing panics.
        gpu_bound(0xffff_f000, 0x10_0000);
        gpu_bound(0x1_0000_0000, 0x1000);
        copy_bound(0x1_0000_0000, 0x1000);
        assert_eq!(stamp(0), 1);
        assert_ne!(stamp(u64::from(u32::MAX)), FOR_GOOD);
    }

    /// A stretch of a reserved section, mapped as `xmem` maps physical
    /// pages (views of consecutive pages in turn, committed), is opened
    /// through a view of ours: the card reads the bytes the title's views
    /// show, at the buffer's offset, and its writes show in them; a second
    /// ask in the stretch finds the same region.
    #[test]
    fn warp_a_stretch_of_a_reserved_section_is_used_in_place_through_a_view_of_ours() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = crate::inplace::tests::warp() else { return };
        if windows::core::Interface::cast::<ID3D12Device13>(&device).is_err() {
            return;
        }
        const K: usize = 64 << 10;
        // SAFETY (the whole test): the section, placeholder and views made
        // here.
        unsafe {
            let process = windows_sys::Win32::System::Threading::GetCurrentProcess();
            let section = mem::CreateFileMappingW(
                windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
                std::ptr::null(),
                mem::PAGE_READWRITE | mem::SEC_RESERVE,
                0,
                8 * K as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let hole = mem::VirtualAlloc2(process, std::ptr::null(), 8 * K, mem::MEM_RESERVE | mem::MEM_RESERVE_PLACEHOLDER, mem::PAGE_NOACCESS, std::ptr::null_mut(), 0)
                .cast::<u8>();
            assert!(!hole.is_null());
            // Two views, pages 0..3 and 3..8 of the section, in turn.
            mem::VirtualFree(hole.cast(), 3 * K, mem::MEM_RELEASE | mem::MEM_PRESERVE_PLACEHOLDER);
            let mut views = Vec::new();
            for (page, n) in [(0usize, 3usize), (3, 5)] {
                let view = mem::MapViewOfFile3(section, process, hole.add(page * K).cast(), (page * K) as u64, n * K, mem::MEM_REPLACE_PLACEHOLDER, mem::PAGE_READWRITE, std::ptr::null_mut(), 0);
                assert!(!view.Value.is_null(), "{}", std::io::Error::last_os_error());
                assert!(!mem::VirtualAlloc(view.Value, n * K, mem::MEM_COMMIT, mem::PAGE_READWRITE).is_null());
                crate::inplace::title_memory_view(view.Value as usize, n * K, section as usize, (page * K) as u64);
                views.push(view);
            }
            let base = hole as u64;
            let (start, len) = (base + K as u64 + 0x40, 5 * K as u64);
            let (whole, first) = region_for(&device, start, start + len).unwrap_or_else(|e| panic!("not opened: {e:?}"));
            assert!(first <= start && first % K as u64 == 0);
            let offset = start - first;
            let mut rig = crate::inplace::Rig::new(&device).unwrap();
            for (i, b) in std::slice::from_raw_parts_mut(start as *mut u8, len as usize).iter_mut().enumerate() {
                *b = (i * 7 + 3) as u8;
            }
            let got = rig.fetch(&whole, offset, len as usize).unwrap();
            assert!(got == std::slice::from_raw_parts(start as *const u8, len as usize), "the card reads the title's bytes, across both views");
            rig.store(&whole, offset + 3 * K as u64, &[0x5a; 256]).unwrap();
            assert!(std::slice::from_raw_parts((start + 3 * K as u64) as *const u8, 256).iter().all(|&b| b == 0x5a), "the card's write shows in the title's view");
            // Another ask in the same stretch: the same region.
            let regions = REGIONS.read().unwrap().len();
            let (second, again) = region_for(&device, base + 6 * K as u64, base + 6 * K as u64 + 100).unwrap_or_else(|e| panic!("not opened: {e:?}"));
            assert_eq!(REGIONS.read().unwrap().len(), regions, "one region for the stretch");
            assert_eq!(again, first);
            // The title's views may change under the heap.
            drop((whole, second));
            forget(base as usize, 8 * K);
            for view in views {
                assert!(mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER) != 0, "the title's own view can be unmapped");
            }
            crate::inplace::views_gone(base as usize, 8 * K);
            mem::VirtualFree(hole.cast(), 0, mem::MEM_RELEASE);
            mem::VirtualFree(hole.add(3 * K).cast(), 0, mem::MEM_RELEASE);
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
    }
}
