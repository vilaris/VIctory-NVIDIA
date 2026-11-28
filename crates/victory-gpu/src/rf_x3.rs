//! Attribution for title-memory traffic. The measurements are advisory:
//! neither binding counts nor observed write history establish heap safety.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

const MB: u64 = 1_000_000;
const DEFAULT_READ_LIMIT_MB: u64 = 4;
const TOP: usize = 20;

#[inline]
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RF_X3_STATS").is_some_and(|v| v == "1"))
}

pub(crate) fn timer() -> Option<Instant> {
    enabled().then(Instant::now)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FastSkip {
    Off,
    On,
    Verify,
}

pub(crate) fn fastskip() -> FastSkip {
    static MODE: OnceLock<FastSkip> = OnceLock::new();
    *MODE.get_or_init(|| {
        if std::env::var_os("VICTORY_RF_X3_FASTSKIP").is_none_or(|v| v != "1") {
            FastSkip::Off
        } else if std::env::var_os("VICTORY_RF_X3_VERIFY").is_some_and(|v| v == "1") {
            FastSkip::Verify
        } else {
            FastSkip::On
        }
    })
}

pub(crate) fn single_block(from: usize, to: usize, block_size: usize) -> Option<usize> {
    let first = from / block_size;
    (from < to && to.div_ceil(block_size) == first + 1).then_some(first)
}

pub(crate) fn clean_matches(known: (usize, usize), asked: (usize, usize), saved: u64, current: u64) -> bool {
    saved != 0 && known.0 <= asked.0 && asked.1 <= known.1 && known.0 < known.1 && asked.0 < asked.1 && saved == current
}

#[repr(usize)]
#[derive(Clone, Copy)]
pub(crate) enum Check {
    AheadSpan,
    QueueSpan,
    MergeSpan,
    MemoGuard,
    MemoKept,
    MemoMiss,
    JournalGuard,
    JournalGuardDirty,
    JournalKept,
    JournalKeptDirty,
    JournalUnknown,
    EarlyIneligible,
    UpdateClean,
    UpdateCopied,
    WatchAhead,
    WatchPublished,
    WatchKernel,
    WatchGuards,
    WatchUntracked,
    FastSkip,
    FastMiss,
    Verify,
    VerifyMiss,
    ReusedMiss,
    FrontWall,
    BeforeWritesWall,
    FlushWall,
    OneWall,
}

const CHECKS: [&str; 28] = [
    "ahead-span", "queue-span", "merge-span", "memo-guard", "memo-kept", "memo-miss",
    "journal-guard-clean", "journal-guard-dirty", "journal-kept-clean", "journal-kept-dirty",
    "journal-unknown", "early-ineligible",
    "update-clean", "update-copied", "watch-ahead", "watch-published", "watch-kernel",
    "watch-guards", "watch-untracked", "fastskip", "fast-miss", "verify", "verify-miss", "reused-miss",
    "front-wall", "before-writes-wall", "flush-wall", "one-wall",
];

fn checked_ns(check: Check, ns: u64) {
    let _ = BANK.try_with(|&bank| {
        let entry = &CHECK_BANKS[bank];
        entry.count[check as usize].fetch_add(1, Relaxed);
        entry.ns[check as usize].fetch_add(ns, Relaxed);
    });
}

pub(crate) fn refresh_wall(check: Check, ns: u64) {
    if enabled() {
        checked_ns(check, ns);
    }
}

struct CheckBank {
    count: [AtomicU64; CHECKS.len()],
    ns: [AtomicU64; CHECKS.len()],
}

static CHECK_BANKS: [CheckBank; BANKS] = [const {
    CheckBank {
        count: [const { AtomicU64::new(0) }; CHECKS.len()],
        ns: [const { AtomicU64::new(0) }; CHECKS.len()],
    }
}; BANKS];

pub(crate) fn checked(started: Option<Instant>, check: Check) {
    if let Some(started) = started {
        checked_ns(check, started.elapsed().as_nanos() as u64);
    }
}

pub(crate) fn journal_result(started: Option<Instant>, published: bool, clean: bool) {
    let check = match (published, clean) {
        (true, true) => Check::JournalGuard,
        (true, false) => Check::JournalGuardDirty,
        (false, true) => Check::JournalKept,
        (false, false) => Check::JournalKeptDirty,
    };
    checked(started, check);
}

pub(crate) fn divergence(from: usize, len: usize, copied: usize) {
    static DIVERGED: AtomicU64 = AtomicU64::new(0);
    let n = DIVERGED.fetch_add(1, Relaxed) + 1;
    if n <= 8 || n.is_power_of_two() {
        tracing::warn!("rf-x3 verify diverge at {from:#x}+{len:#x}, legacy copied {copied} bytes (event {n})");
    }
}

type Verification = HashMap<(u64, usize, usize), u64>;
static VERIFY_MISSES: Mutex<Option<Verification>> = Mutex::new(None);

pub(crate) fn verification_miss(id: u64, from: usize, len: usize) {
    if enabled() {
        checked_ns(Check::VerifyMiss, 0);
    }
    let mut misses = VERIFY_MISSES.lock().unwrap_or_else(|e| e.into_inner());
    let misses = misses.get_or_insert_with(HashMap::new);
    let frame = FRAME.load(Relaxed);
    if misses.len() >= 4096 {
        misses.retain(|_, seen| *seen == frame);
    }
    if misses.len() < 4096 {
        misses.insert((id, from, len), frame);
    }
}

/// This flush already rejected the quick memo for this exact span.
/// Go straight to the original page-stamp comparison. A freshly
/// available memo can only save the old path work, so missing it here
/// cannot authorize a new skip.
#[cfg(windows)]
pub(crate) fn after_memo_miss(shadow: &crate::shadow::Shadow, from: usize, len: usize) -> bool {
    if fastskip() != FastSkip::On {
        return shadow.nothing_new(from, len);
    }
    if enabled() {
        checked_ns(Check::ReusedMiss, 0);
    }
    shadow.nothing_new_after_memo(from, len)
}

#[cfg(windows)]
pub(crate) fn verify_candidate(shadow: &crate::shadow::Shadow, from: usize, to: usize) -> bool {
    fastskip() == FastSkip::Verify && shadow.rf_x3_guard_clean(from, to - from)
}

#[cfg(windows)]
pub(crate) fn verify_update(shadow: &crate::shadow::Shadow, from: usize, len: usize, copied: usize, candidate: bool) {
    if fastskip() != FastSkip::Verify {
        return;
    }
    let pending = VERIFY_MISSES.lock().unwrap_or_else(|e| e.into_inner())
        .as_mut().and_then(|misses| misses.remove(&(shadow.id(), from, len)));
    if copied != 0 && (candidate || pending == Some(FRAME.load(Relaxed))) {
        divergence(from, len, copied);
    }
}

fn read_limit() -> u64 {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_RF_X3_READ_LIMIT_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_READ_LIMIT_MB)
            .saturating_mul(MB)
    })
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum Kind {
    Buffer,
    Window,
    DescriptorHeap,
    Texture,
    Constants,
    Stream,
    TitleCopy,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Object {
    base: u64,
    size: u64,
    kind: Kind,
}

#[repr(usize)]
#[derive(Clone, Copy, Debug)]
enum Backing {
    AddressPrivate,
    SectionCommit,
    ReserveAlreadyOpen,
    ReserveNeedsAlias,
    Soft,
    Fragmented,
    Unknown,
}

const BACKINGS: [&str; 7] = [
    "address1-private", "filemap-sec-commit", "address1-sec-reserve-open",
    "sec-reserve-filemap-refused", "soft", "fragmented", "unknown",
];

#[cfg(windows)]
#[derive(Default)]
struct BackingCensus(std::collections::BTreeMap<usize, usize>);

#[cfg(windows)]
impl BackingCensus {
    fn sample(&mut self, object: Object) -> Backing {
        use crate::inplace::Cover;
        let at = object.base as usize;
        let len = object.size as usize;
        const ALIGN: usize = 64 << 10;
        let Some(end) = at.checked_add(len).and_then(|end| end.checked_add(ALIGN - 1)) else {
            return Backing::Unknown;
        };
        let end = end / ALIGN * ALIGN;
        let (cover, committed) = crate::inplace::section_cover(at, len);
        match (cover, committed) {
            (Cover::Run(..), Some(true)) => return Backing::SectionCommit,
            (Cover::Run(..), Some(false)) => {
                return if crate::port::range_open(object.base, object.end()) {
                    Backing::ReserveAlreadyOpen
                } else {
                    Backing::ReserveNeedsAlias
                };
            }
            (Cover::Scattered, _) => return Backing::Fragmented,
            _ => {}
        }
        if !crate::watch::soft_runs(at, len).is_empty() {
            return Backing::Soft;
        }
        if cover != Cover::None {
            return Backing::Fragmented;
        }
        let from = at / ALIGN * ALIGN;
        if self.0.range(..=from).next_back().is_some_and(|(_, &past)| end <= past) {
            return Backing::AddressPrivate;
        }
        if let Some((base, past)) = crate::inplace::private_readwrite_region(from) {
            self.0.insert(base, past);
            if end <= past {
                return Backing::AddressPrivate;
            }
        }
        Backing::Unknown
    }
}

#[cfg(not(windows))]
#[derive(Default)]
struct BackingCensus;

#[cfg(not(windows))]
impl BackingCensus {
    fn sample(&mut self, _: Object) -> Backing {
        Backing::Unknown
    }
}

impl Object {
    pub(crate) fn new(base: u64, size: u64, kind: Kind) -> Self {
        Self { base, size, kind }
    }

    #[cfg(windows)]
    pub(crate) fn shadow(shadow: &crate::shadow::Shadow) -> Self {
        let (base, size) = shadow.span();
        let kind = if shadow.is_window() { Kind::Window } else { Kind::Buffer };
        Self::new(base, size, kind)
    }

    #[cfg(windows)]
    pub(crate) fn title_copy(base: usize, size: usize) -> Self {
        if let Some((at, len)) = crate::descriptor::shader_heap_extent(base, size) {
            Self::new(at, len, Kind::DescriptorHeap)
        } else if let Some((at, len)) = crate::shadow::in_buffer(base as u64, size as u64) {
            Self::new(at, len, Kind::Buffer)
        } else {
            Self::new(base as u64, size as u64, Kind::TitleCopy)
        }
    }

    fn end(self) -> u64 {
        self.base.saturating_add(self.size)
    }
}

#[repr(usize)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Access {
    Cbv,
    Srv,
    Uav,
    Vertex,
    Index,
    Indirect,
}

const ACCESS: [&str; 6] = ["CBV", "SRV", "UAV", "vertex", "index", "indirect"];

#[repr(usize)]
#[derive(Clone, Copy)]
pub(crate) enum Source {
    RefreshWatch,
    RefreshWhole,
    TitleCopy,
    Fill,
    Placed,
    TextureRefresh,
    Plane,
    CardBefore,
    DemandFetch,
    DescriptorSync,
    TextureDigest,
}

const SOURCES: [&str; 11] = [
    "refresh-watch", "refresh-unwatched", "title-copy", "fill", "placed", "texture-refresh",
    "plane", "card_before", "demand_fetch", "descriptor-sync", "of-which-digest",
];
const COPIES: usize = Source::TextureRefresh as usize;
const TEXTURES: std::ops::Range<usize> = Source::TextureRefresh as usize..Source::CardBefore as usize;

#[derive(Clone, Copy, Default)]
struct Work {
    bytes: u64,
    us: u64,
}

impl Work {
    fn add(&mut self, rhs: Self) {
        self.bytes += rhs.bytes;
        self.us += rhs.us;
    }
}

#[derive(Default)]
struct Row {
    work: [Work; SOURCES.len()],
    uses: [u64; ACCESS.len()],
    view_bytes: u64,
    rewatched: HashSet<u64>,
    shader_write: bool,
    carded: bool,
}

impl Row {
    fn merge(&mut self, other: Self) {
        for (lhs, rhs) in self.work.iter_mut().zip(other.work) {
            lhs.add(rhs);
        }
        for (lhs, rhs) in self.uses.iter_mut().zip(other.uses) {
            *lhs += rhs;
        }
        self.view_bytes += other.view_bytes;
        self.rewatched.extend(other.rewatched);
        self.shader_write |= other.shader_write;
        self.carded |= other.carded;
    }

    fn total(&self) -> Work {
        sum(&self.work[..Source::TextureDigest as usize])
    }
}

fn sum(work: &[Work]) -> Work {
    work.iter().copied().fold(Work::default(), |mut result, each| {
        result.add(each);
        result
    })
}

type Rows = HashMap<Object, Row>;
const BANKS: usize = 32;
static ROWS: [Mutex<Option<Rows>>; BANKS] = [const { Mutex::new(None) }; BANKS];
static FRAME: AtomicU64 = AtomicU64::new(0);
static NEXT_BANK: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static BANK: usize = NEXT_BANK.fetch_add(1, Relaxed) % BANKS;
}

fn with_row(object: Object, f: impl FnOnce(&mut Row)) {
    if object.size == 0 {
        return;
    }
    let _ = BANK.try_with(|bank| {
        let mut rows = ROWS[*bank].lock().unwrap_or_else(|e| e.into_inner());
        f(rows.get_or_insert_with(HashMap::new).entry(object).or_default());
    });
}

/// Inputs have already passed the stats switch.
pub(crate) fn work(object: Object, source: Source, bytes: u64, us: u64) {
    if bytes != 0 || us != 0 {
        with_row(object, |row| row.work[source as usize].add(Work { bytes, us }));
    }
}

pub(crate) fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_micros() as u64
}

pub(crate) fn rewatched(object: Object) {
    with_row(object, |row| {
        row.rewatched.insert(FRAME.load(Relaxed));
    });
}

#[cfg(windows)]
pub(crate) fn refresh(shadow: &crate::shadow::Shadow, bytes: usize, us: u64) {
    if enabled() {
        let object = Object::shadow(shadow);
        let source = if shadow.unwatched() { Source::RefreshWhole } else { Source::RefreshWatch };
        checked_ns(if bytes == 0 { Check::UpdateClean } else { Check::UpdateCopied }, us * 1_000);
        with_row(object, |row| {
            row.work[source as usize].add(Work { bytes: bytes as u64, us });
            row.carded |= shadow.carded();
            row.shader_write |= shadow.written();
        });
    }
}

#[cfg(windows)]
pub(crate) fn picked_writes(ranges: &[crate::picked::Range]) {
    if enabled() {
        for (shadow, _, _) in ranges {
            with_row(Object::shadow(shadow), |row| row.shader_write = true);
        }
    }
}

/// Assign a batch's CPU elapsed time in proportion to its item bytes.
fn share(us: u64, from: u64, to: u64, total: u64) -> u64 {
    let prefix = |bytes| (u128::from(us) * u128::from(bytes) / u128::from(total)) as u64;
    if total == 0 { 0 } else { prefix(to) - prefix(from) }
}

pub(crate) fn batch_work(
    source: Source,
    us: u64,
    items: impl Iterator<Item = (Object, u64)> + Clone,
) {
    let total = items.clone().map(|(_, n)| n).sum();
    let mut end = 0;
    for (object, bytes) in items {
        let start = end;
        end += bytes;
        work(object, source, bytes, share(us, start, end, total));
    }
}

pub(crate) fn fill_object(src: usize, len: usize, constants: bool) -> Object {
    Object::new(src as u64, len as u64, if constants { Kind::Constants } else { Kind::Stream })
}

pub(crate) fn observe_fills(fills: &[(usize, usize, usize, bool)]) -> Option<Vec<(Object, u64)>> {
    enabled().then(|| fills.iter().map(|&(_, src, len, constants)| {
        (fill_object(src, len, constants), len as u64)
    }).collect())
}

pub(crate) fn fills(observed: Option<Vec<(Object, u64)>>, took: Duration) {
    if let Some(items) = observed {
        batch_work(Source::Fill, took.as_micros() as u64, items.iter().copied());
    }
}

pub(crate) fn plane(src: usize, len: usize, us: u64) {
    if enabled() {
        work(fill_object(src, len, false), Source::Plane, len as u64, us);
    }
}

#[cfg(windows)]
pub(crate) fn constant(list: &crate::host::HostList, address: u64, len: u64) {
    list.rf_x3_use(Object::new(address, len, Kind::Constants), Access::Cbv, len, false);
}

#[cfg(windows)]
pub(crate) fn table_constant(list: &crate::host::HostList, address: u64, len: u32, named: bool, selected: &[u32], slot: u32) {
    if enabled() && (named || selected.binary_search(&slot).is_ok()) {
        constant(list, address, u64::from(len));
    }
}

#[cfg(windows)]
pub(crate) fn stream(list: &crate::host::HostList, access: Access, address: u64, len: u64) {
    list.rf_x3_use(Object::new(address, len, Kind::Stream), access, len, false);
}

#[cfg(windows)]
pub(crate) fn texture_use(list: &crate::host::HostList, resource: usize, access: Access, writes: bool) {
    if enabled()
        && let Some((memory, desc)) = crate::resource::texture_memory(resource as crate::object::This)
        && let Some(layout) = crate::resource::texture_layout(&desc)
    {
        let len = layout.planes[0].layout.size;
        list.rf_x3_use(Object::new(memory as u64, len, Kind::Texture), access, len, writes);
    }
}

#[cfg(windows)]
pub(crate) fn listed_srv(list: &crate::host::HostList, target: &crate::descriptor::host::ListedTarget) {
    use crate::descriptor::host::ListedTarget;
    if enabled() {
        match target {
            ListedTarget::Buffer { shadow, address, len } => list.rf_x3_shadow(shadow, Access::Srv, *address, *len, false),
            ListedTarget::ShaderTexture { resource, .. } => texture_use(list, *resource, Access::Srv, false),
            _ => {}
        }
    }
}

/// The fast path skips refresh on reused tables. Observe their explicit
/// slots without creating any copies or decoding arbitrary bindless indices.
#[cfg(windows)]
pub(crate) fn cached_table(list: &crate::host::HostList, table: u64, entries: &[victory_shader::UsedResource], only_constants: bool) {
    use crate::descriptor::{View, host::{buffer_extent, buffer_range}};
    use victory_shader::ResourceType;
    if !enabled() {
        return;
    }
    for entry in entries {
        if only_constants && entry.kind != ResourceType::Cbv {
            continue;
        }
        let Some(view) = crate::descriptor::view(table + u64::from(entry.table_offset_dwords) * 4) else {
            continue;
        };
        match &view {
            View::Constant(address, len) => constant(list, *address, u64::from(*len)),
            _ if entry.kind == ResourceType::Srv => {
                if let Some((address, len)) = buffer_range(&view) {
                    let shadow = buffer_extent(&view).and_then(|(at, n)| crate::shadow::peek(at, n))
                        .or_else(|| crate::shadow::peek(address, len));
                    if let Some(shadow) = shadow {
                        list.rf_x3_shadow(&shadow, Access::Srv, address, len, false);
                    }
                } else if let View::Shader(resource, _) = view {
                    texture_use(list, resource, Access::Srv, false);
                }
            }
            _ => {}
        }
    }
}

#[cfg(windows)]
pub(crate) fn card_before(dirty: &[(std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)], took: Option<Duration>) {
    if enabled() {
        let items = dirty.iter().map(|(shadow, ranges)| {
            (Object::shadow(shadow), ranges.iter().map(|&(a, b)| (b - a) as u64).sum())
        });
        batch_work(Source::CardBefore, took.unwrap_or_default().as_micros() as u64, items);
    }
}

#[derive(Clone, Copy, Default)]
struct Binding {
    uses: u64,
    view_bytes: u64,
    writes: bool,
    carded: bool,
}

/// Accumulate bindings per recorded list, then count them on each execution.
type BindingMap = HashMap<(Object, Access), Binding>;

#[derive(Default)]
pub(crate) struct Bindings(Option<Box<BindingMap>>);

impl Bindings {
    pub(crate) fn clear(&mut self) {
        if let Some(bindings) = &mut self.0 {
            bindings.clear();
        }
    }

    pub(crate) fn note(&mut self, object: Object, access: Access, bytes: u64, writes: bool, carded: bool) {
        if bytes == 0 {
            return;
        }
        let binding = self.0.get_or_insert_with(Box::default).entry((object, access)).or_default();
        binding.uses += 1;
        binding.view_bytes += bytes;
        binding.writes |= writes;
        binding.carded |= carded;
    }

    pub(crate) fn mark_write(&mut self, object: Object, carded: bool) {
        let binding = self.0.get_or_insert_with(Box::default).entry((object, Access::Uav)).or_default();
        binding.writes = true;
        binding.carded |= carded;
    }

    pub(crate) fn submit(&self) {
        if let Some(bindings) = self.0.as_deref() {
            for (&(object, access), &binding) in bindings {
                with_row(object, |row| {
                    row.uses[access as usize] += binding.uses;
                    row.view_bytes += binding.view_bytes;
                    row.shader_write |= binding.writes;
                    row.carded |= binding.carded;
                });
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Screen {
    ShaderWrite,
    IneligibleKind,
    Vram,
    ReadsUnknown,
    ReadLimit,
    NoRepeatedWatchWrites,
    NeedsSafetyProof,
}

fn screen(object: Object, row: &Row, frames: u64, limit: u64, shader_write: bool) -> Screen {
    if shader_write {
        Screen::ShaderWrite
    } else if !matches!(object.kind, Kind::Buffer | Kind::Window | Kind::Constants) {
        Screen::IneligibleKind
    } else if row.carded {
        Screen::Vram
    } else if row.view_bytes == 0 {
        Screen::ReadsUnknown
    } else if row.view_bytes > limit.saturating_mul(frames) {
        Screen::ReadLimit
    } else if frames < 2 || (row.rewatched.len() as u64) < frames {
        Screen::NoRepeatedWatchWrites
    } else {
        Screen::NeedsSafetyProof
    }
}

fn write_spans(rows: &Rows) -> Vec<(u64, u64)> {
    let mut spans: Vec<_> = rows.iter().filter(|(_, row)| row.shader_write)
        .map(|(object, _)| (object.base, object.end())).collect();
    spans.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in spans {
        if let Some(last) = merged.last_mut()
            && last.1 >= start
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn may_write(spans: &[(u64, u64)], object: Object) -> bool {
    let at = spans.partition_point(|&(_, end)| end <= object.base);
    spans.get(at).is_some_and(|&(start, _)| start < object.end())
}

#[derive(Default)]
pub(crate) struct Meter {
    started: Option<Instant>,
    frames: u64,
    fps: [Work; 2],
    submit_ns: [u64; 16],
    flushes: [u64; 4],
    frame_prepare_us: u64,
}

impl Meter {
    pub(crate) fn present(&self) {
        if enabled() || fastskip() == FastSkip::Verify {
            FRAME.fetch_add(1, Relaxed);
        }
    }

    pub(crate) fn sample(
        &mut self,
        interval: Duration,
        frames: u64,
        counters: [u64; 4],
        prepare_us: u64,
        splits: [u64; 16],
        flushes: [u64; 4],
    ) {
        if !enabled() {
            return;
        }
        let started = *self.started.get_or_insert_with(|| Instant::now() - interval);
        self.frames += frames;
        self.fps[0].add(Work { us: counters[0], bytes: counters[1] });
        self.fps[1].add(Work { us: counters[2], bytes: counters[3] });
        self.frame_prepare_us += prepare_us;
        for (total, sample) in self.submit_ns.iter_mut().zip(splits) {
            *total += sample;
        }
        for (total, sample) in self.flushes.iter_mut().zip(flushes) {
            *total += sample;
        }
        if started.elapsed() < Duration::from_secs(5) {
            return;
        }
        let report = std::mem::take(self);
        let mut rows = Rows::new();
        for bank in &ROWS {
            let taken = bank.lock().unwrap_or_else(|e| e.into_inner()).take();
            for (object, row) in taken.into_iter().flatten() {
                rows.entry(object).or_default().merge(row);
            }
        }
        report.log(rows);
    }

    fn log(self, rows: Rows) {
        use std::fmt::Write;
        let f = self.frames as f64;
        let ms = |us| us as f64 / 1_000.0 / f;
        let mb = |bytes| bytes as f64 / MB as f64 / f;
        let mut accounted = [Work::default(); 2];
        let mut other = Work::default();
        let mut ranked = Vec::new();
        let mut census = BackingCensus::default();
        let mut backing_bytes = [[0u64; 2]; BACKINGS.len()];
        let writes = write_spans(&rows);
        let limit = read_limit();
        for (&object, row) in &rows {
            let copies = sum(&row.work[..COPIES]);
            let textures = sum(&row.work[TEXTURES]);
            accounted[0].add(copies);
            accounted[1].add(textures);
            other.add(sum(&row.work[Source::CardBefore as usize..Source::TextureDigest as usize]));
            let total = row.total();
            if total.bytes != 0 || total.us != 0 {
                let backing = census.sample(object);
                backing_bytes[backing as usize][0] += copies.bytes;
                backing_bytes[backing as usize][1] += textures.bytes;
                ranked.push((object, row, total, backing));
            }
        }
        ranked.sort_unstable_by_key(|&(object, _, total, _)| {
            (std::cmp::Reverse(total.us), std::cmp::Reverse(total.bytes), object)
        });
        let budget = |i: usize| {
            let (fps, events) = (self.fps[i], accounted[i]);
            format!(
                "fps={:.3}ms/{:.3}MB attributed={:.3}ms/{:.3}MB delta={:+.3}ms/{:+.3}MB",
                ms(fps.us), mb(fps.bytes), ms(events.us), mb(events.bytes),
                ms(fps.us) - ms(events.us), mb(fps.bytes) - mb(events.bytes),
            )
        };
        let mut top = String::new();
        for (object, row, total, backing) in ranked.into_iter().take(TOP) {
            let shader_write = may_write(&writes, object);
            let screen = screen(object, row, self.frames, limit, shader_write);
            let _ = write!(
                top, " | {:#x}+{:#x}/{:?}/{} {:.3}ms/{:.3}MB views={:.3}MB rewrites={}/{} shader_write={} {:?}",
                object.base, object.size, object.kind, BACKINGS[backing as usize], ms(total.us), mb(total.bytes),
                mb(row.view_bytes), row.rewatched.len(), self.frames,
                if shader_write { "possible" } else { "unobserved" }, screen,
            );
            for (name, count) in ACCESS.into_iter().zip(row.uses).filter(|(_, n)| *n != 0) {
                let _ = write!(top, " {name}={:.2}", count as f64 / f);
            }
            for (name, work) in SOURCES.into_iter().zip(row.work).filter(|(_, w)| w.us != 0 || w.bytes != 0) {
                let _ = write!(top, " {name}={:.3}ms/{:.3}MB", ms(work.us), mb(work.bytes));
            }
        }
        let mut checks = String::new();
        for (i, name) in CHECKS.iter().enumerate() {
            let count: u64 = CHECK_BANKS.iter().map(|b| b.count[i].swap(0, Relaxed)).sum();
            let ns: u64 = CHECK_BANKS.iter().map(|b| b.ns[i].swap(0, Relaxed)).sum();
            let _ = write!(checks, " {name}={:.1}/{:.3}ms", count as f64 / f, ns as f64 / 1e6 / f);
        }
        let mut backing = String::new();
        for (name, [copies, textures]) in BACKINGS.into_iter().zip(backing_bytes) {
            let _ = write!(backing, " {name}={:.3}/{:.3}", mb(copies), mb(textures));
        }
        let per_ns = |i: usize| self.submit_ns[i] as f64 / 1e6 / f;
        let [flushes, ranges, clean, parallel] = self.flushes.map(|n| n as f64 / f);
        let prepare = ms(self.frame_prepare_us);
        let (watch, flights, steps, card, refresh) = (per_ns(0), per_ns(1), per_ns(2), per_ns(4), per_ns(6));
        tracing::info!(
            "rf-x3: frames={} limit={:.1}MB/frame preparing={prepare:.3}ms submit-ms[watch={watch:.3} flights={flights:.3} steps={steps:.3} card={card:.3} refresh={refresh:.3}] flushes={flushes:.1} ranges={ranges:.1} known-clean={clean:.1} parallel={parallel:.1} checks[count/frame/ms/frame{}] copies[{}] textures[{}] other={:.3}ms/{:.3}MB backing[copy-MB/texture-MB{}] top20{}",
            self.frames, limit as f64 / MB as f64, checks, budget(0), budget(1), ms(other.us), mb(other.bytes), backing, top,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_requires_rewrites_and_applies_inclusive_budget() {
        let object = Object::new(0x1000, 4096, Kind::Buffer);
        let mut row = Row { view_bytes: 3 * MB, ..Row::default() };
        assert_eq!(screen(object, &row, 3, MB, false), Screen::NoRepeatedWatchWrites);
        row.rewatched.extend(1..=3);
        assert_eq!(screen(object, &row, 3, MB, false), Screen::NeedsSafetyProof);
        assert_eq!(screen(object, &row, 3, MB - 1, false), Screen::ReadLimit);
        assert_eq!(screen(object, &row, 3, MB, true), Screen::ShaderWrite);
        row.carded = true;
        assert_eq!(screen(object, &row, 3, MB, false), Screen::Vram);
    }

    #[test]
    fn merges_counts_but_deduplicates_rewrite_frames() {
        let mut a = Row { view_bytes: 20, ..Row::default() };
        a.work[0] = Work { bytes: 19, us: 2 };
        a.uses[1] = 2;
        a.rewatched.extend([1, 2]);
        let mut b = Row { view_bytes: 40, shader_write: true, ..Row::default() };
        b.work[1] = Work { bytes: 23, us: 3 };
        b.uses[1] = 3;
        b.rewatched.extend([2, 3]);
        a.merge(b);
        assert_eq!(a.total().bytes, 42);
        assert_eq!(a.total().us, 5);
        assert_eq!(a.uses[1], 5);
        assert_eq!(a.view_bytes, 60);
        assert_eq!(a.rewatched.len(), 3);
        assert!(a.shader_write);
    }

    #[test]
    fn shader_write_rejects_overlapping_aliases() {
        let object = Object::new(0x1000, 4096, Kind::Buffer);
        let rows = Rows::from([(object, Row { shader_write: true, ..Row::default() })]);
        let spans = write_spans(&rows);
        assert!(may_write(&spans, Object::new(0x800, 4096, Kind::Constants)));
        assert!(!may_write(&spans, Object::new(0x2000, 4096, Kind::Window)));
    }

    #[test]
    fn apportioned_time_keeps_the_batch_total() {
        assert_eq!([share(17, 0, 2, 8), share(17, 2, 3, 8), share(17, 3, 8, 8)], [4, 2, 11]);
        assert_eq!(share(17, 0, 0, 0), 0);
    }

    #[test]
    fn a_fast_memo_requires_the_same_generation_and_contains_the_entire_span() {
        let block = 64 << 10;
        assert_eq!(single_block(block, 2 * block, block), Some(1));
        assert_eq!(single_block(block - 4, block + 4, block), None);
        assert_eq!(single_block(block, block, block), None);
        assert!(clean_matches((100, 200), (120, 200), 8, 8));
        assert!(!clean_matches((100, 200), (120, 200), 8, 9));
        assert!(!clean_matches((100, 200), (120, 201), 8, 8));
        assert!(!clean_matches((100, 200), (120, 200), 0, 0));
        assert!(!clean_matches((100, 200), (150, 150), 8, 8));
    }

    #[test]
    fn accounting_excludes_follow_up_transfer_and_nested_digest_from_preparing() {
        let mut row = Row::default();
        for (source, bytes, us) in [
            (Source::RefreshWatch, 40, 8),
            (Source::Fill, 10, 2),
            (Source::TextureRefresh, 20, 4),
            (Source::TextureDigest, 20, 3),
            (Source::CardBefore, 30, 2),
            (Source::DemandFetch, 5, 1),
        ] {
            row.work[source as usize].add(Work { bytes, us });
        }
        let copies = sum(&row.work[..COPIES]);
        let textures = sum(&row.work[TEXTURES]);
        assert_eq!((copies.bytes, copies.us), (50, 10));
        assert_eq!((textures.bytes, textures.us), (20, 4));
        assert_eq!((row.total().bytes, row.total().us), (105, 17));
    }
}
