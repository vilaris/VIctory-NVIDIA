//! Opt-in release instrumentation for the X2 GPU synchronization experiment.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, Interface};
use windows_sys::Win32::System::{Performance::*, ProcessStatus::*, Threading::GetCurrentProcess};

struct Switches {
    markers: u8,
    stats: bool,
}

#[inline]
fn switches() -> &'static Switches {
    static SWITCHES: OnceLock<Switches> = OnceLock::new();
    SWITCHES.get_or_init(|| {
        let markers = match std::env::var("VICTORY_RF_MARKERS").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            _ => 0,
        };
        let stats = markers != 0 || std::env::var_os("VICTORY_RF_X2_MEMSTATS").is_some_and(|v| v == "1");
        Switches { markers, stats }
    })
}

#[inline]
pub(crate) fn begin(list: &ID3D12GraphicsCommandList, name: &str) {
    if switches().markers == 0 {
        return;
    }
    // All callers use short strings. Metadata 0 is the UTF-16 convention
    // used by the existing SetMarker instrumentation.
    let mut text = [0u16; 64];
    let mut len = 0;
    for (slot, unit) in text[..63].iter_mut().zip(name.encode_utf16()) {
        *slot = unit;
        len += 1;
    }
    // SAFETY: the caller has an open list. The runtime copies the payload.
    unsafe { list.BeginEvent(0, Some(text.as_ptr().cast()), (2 * (len + 1)) as u32) };
}

#[inline]
pub(crate) fn end(list: &ID3D12GraphicsCommandList) {
    if switches().markers != 0 {
        // SAFETY: the caller is ending a range on its open list.
        unsafe { list.EndEvent() };
    }
}

pub(crate) struct Range<'a>(Option<&'a ID3D12GraphicsCommandList>);

#[inline]
pub(crate) fn range<'a>(list: &'a ID3D12GraphicsCommandList, name: &str) -> Range<'a> {
    if switches().markers == 0 {
        return Range(None);
    }
    begin(list, name);
    Range(Some(list))
}

impl Drop for Range<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(list) = self.0 {
            end(list);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Title,
    Thin,
    Coherence,
    Copy,
    Prelude,
}

pub(crate) fn title_origin(lowered: bool) -> Origin {
    if lowered { Origin::Thin } else { Origin::Title }
}

fn ambient(stage: &str) -> &'static str {
    match stage {
        "ambient: keep" => "vx:ambient.keep",
        "ambient: mean" => "vx:ambient.mean",
        "ambient: air" => "vx:ambient.air",
        "ambient: place" => "vx:ambient.place",
        "ambient: gather" => "vx:ambient.gather",
        "ambient: raise" => "vx:ambient.raise",
        "ambient: reflect" => "vx:ambient.reflect",
        _ => "vx:ambient.other",
    }
}

#[inline]
pub(crate) fn ambient_range<'a>(list: &'a ID3D12GraphicsCommandList, stage: &str) -> Range<'a> {
    if switches().markers == 0 {
        return Range(None);
    }
    range(list, ambient(stage))
}

impl Origin {
    fn index(self) -> usize {
        match self {
            Self::Title | Self::Thin => 0,
            Self::Coherence => 1,
            Self::Copy => 2,
            Self::Prelude => 3,
        }
    }

    fn name(self) -> &'static str {
        ["list:title", "list:aux.coherence", "list:aux.copy", "list:prelude"][self.index()]
    }
}

// Keep the origin on the COM object so coverage follows the actual
// submissions, including replay of a closed list and held Present batches.
const ORIGIN: GUID = GUID::from_u128(0xcf87fdb9_2364_42bc_857f_0de07637d10a);

#[inline]
pub(crate) fn open(list: &ID3D12GraphicsCommandList, origin: Origin) {
    if switches().stats {
        let index = origin.index() as u8;
        // SAFETY: this is one byte copied by D3D12 into the live list.
        let _ = unsafe { list.SetPrivateData(&ORIGIN, 1, Some((&raw const index).cast())) };
    }
    begin(list, origin.name());
}

pub(crate) struct Recording {
    origin: Origin,
    open: AtomicBool,
    title: Mutex<Title>,
}

impl Recording {
    pub(crate) fn new(list: &ID3D12GraphicsCommandList, origin: Origin) -> Option<Box<Self>> {
        open(list, origin);
        if switches().markers == 0 {
            return None;
        }
        let recording = Box::new(Self {
            origin,
            open: AtomicBool::new(true),
            title: Mutex::new(Title { thin: origin == Origin::Thin, ..Title::default() }),
        });
        recording.start_title(list);
        Some(recording)
    }

    fn start_title(&self, list: &ID3D12GraphicsCommandList) {
        if self.origin.index() == Origin::Title.index() {
            if self.title.lock().unwrap_or_else(|e| e.into_inner()).thin {
                begin(list, "vx:thin");
            }
            begin(list, "title:unbound");
        }
    }

    pub(crate) fn reopen(recording: Option<&Self>, list: &ID3D12GraphicsCommandList) {
        if let Some(recording) = recording {
            begin(list, recording.origin.name());
            recording.start_title(list);
            recording.open.store(true, Relaxed);
        }
    }

    pub(crate) fn close(recording: Option<&Self>, list: &ID3D12GraphicsCommandList) {
        if let Some(recording) = recording
            && recording.open.swap(false, Relaxed)
        {
            if recording.origin.index() == Origin::Title.index() {
                let mut title = recording.title.lock().unwrap_or_else(|e| e.into_inner());
                let thin = title.thin;
                *title = Title { thin, ..Title::default() };
                end(list);
                if thin {
                    end(list);
                }
            }
            end(list);
        }
    }
}

/// A pooled list is already open and empty when assigned to a title list.
#[inline]
pub(crate) fn reuse_title(list: crate::host::HostList, thin: bool) -> crate::host::HostList {
    if let Some(recording) = list.rf_x2.as_deref() {
        let mut title = recording.title.lock().unwrap_or_else(|e| e.into_inner());
        if title.thin != thin {
            end(&list.list); // title:unbound
            if thin {
                begin(&list.list, "vx:thin");
            } else {
                end(&list.list); // vx:thin
            }
            begin(&list.list, "title:unbound");
            title.thin = thin;
        }
    }
    list
}

const GROUP_SIZE: u8 = 32;

#[derive(Default)]
struct Title {
    thin: bool,
    binding: Option<(usize, u64)>,
    group: Option<usize>,
    commands: u8,
}

impl Title {
    fn work(&mut self, mode: u8) -> Option<u64> {
        let (pso, key) = self.binding?;
        if self.group.is_none() || (mode == 2 && self.group != Some(pso)) || (mode == 1 && self.commands == GROUP_SIZE) {
            self.group = Some(pso);
            self.commands = 1;
            return Some(key);
        }
        self.commands = self.commands.saturating_add(1);
        None
    }
}

fn title_name(key: u64) -> String {
    format!("title:{key:016x}")
}

pub(crate) fn pipeline_key(library_key: &str) -> Option<u64> {
    if switches().markers == 0 {
        return None;
    }
    u64::from_str_radix(library_key.get(..16)?, 16).ok()
}

#[inline]
pub(crate) fn bind_title(list: &crate::host::HostList, pipeline: &crate::pipeline::host::HostPipeline) {
    if let Some(recording) = list.rf_x2.as_deref()
        && recording.origin.index() == Origin::Title.index()
    {
        let mut title = recording.title.lock().unwrap_or_else(|e| e.into_inner());
        title.binding = pipeline.rf_x2_key.map(|key| (pipeline.pso.as_raw() as usize, key));
    }
}

#[inline]
pub(crate) fn title_work(list: &crate::host::HostList) {
    if let Some(recording) = list.rf_x2.as_deref()
        && recording.origin.index() == Origin::Title.index()
        && let Some(key) = recording.title.lock().unwrap_or_else(|e| e.into_inner()).work(switches().markers)
    {
        end(&list.list);
        begin(&list.list, &title_name(key));
    }
}

const BAR_UAV: usize = 0;
const BAR_TRANSITION: usize = 1;
const BAR_ALIASING: usize = 2;
const TITLE_BARRIERS: usize = 3;
const LISTS: usize = 6;
const DETILE_US: usize = 10;
const GLOBAL_BARRIERS: usize = 11;
const COUNT: usize = 12;
static TOTALS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];

fn barrier_counts(barriers: &[D3D12_RESOURCE_BARRIER]) -> [u64; 3] {
    let mut by_type = [0; 3];
    for barrier in barriers {
        match barrier.Type {
            D3D12_RESOURCE_BARRIER_TYPE_UAV => by_type[BAR_UAV] += 1,
            D3D12_RESOURCE_BARRIER_TYPE_TRANSITION => by_type[BAR_TRANSITION] += 1,
            D3D12_RESOURCE_BARRIER_TYPE_ALIASING => by_type[BAR_ALIASING] += 1,
            _ => (),
        }
    }
    by_type
}

#[inline]
fn count_barriers(barriers: &[D3D12_RESOURCE_BARRIER], offset: usize) {
    for (slot, count) in TOTALS[offset..].iter().zip(barrier_counts(barriers)) {
        if count != 0 {
            slot.fetch_add(count, Relaxed);
        }
    }
}

/// Records the enhanced global synchronization used by placed copies.
#[inline]
#[track_caller]
pub(crate) unsafe fn global_barrier(base: &ID3D12GraphicsCommandList, list: &ID3D12GraphicsCommandList7, groups: &[D3D12_BARRIER_GROUP]) {
    crate::rf_x7::enhanced_globals(base, groups); // rf-x7
    if let Some(merged) = unsafe { crate::rf_x7::merge_global_groups(groups) } {
        let group = D3D12_BARRIER_GROUP {
            Type: D3D12_BARRIER_TYPE_GLOBAL,
            NumBarriers: 1,
            Anonymous: D3D12_BARRIER_GROUP_0 { pGlobalBarriers: &merged },
        };
        unsafe { record_global(list, "vx:barrier.place", &[group]) };
        return;
    }
    unsafe { record_global(list, "vx:barrier.place", groups) };
}

pub(crate) unsafe fn record_global(list: &ID3D12GraphicsCommandList7, reason: &str, groups: &[D3D12_BARRIER_GROUP]) {
    if switches().stats {
        let count = groups.iter().map(|group| u64::from(group.NumBarriers)).sum();
        TOTALS[GLOBAL_BARRIERS].fetch_add(count, Relaxed);
    }
    let _range = range(list, reason);
    // SAFETY: the original Barrier call's preconditions apply.
    unsafe { list.Barrier(groups) };
}

/// Records Victory's own barriers at the original call site.
#[inline]
#[track_caller]
pub(crate) unsafe fn barriers(list: &ID3D12GraphicsCommandList, reason: &'static str, barriers: &[D3D12_RESOURCE_BARRIER]) {
    unsafe { crate::rf_x7::victory_barriers(list, reason, barriers, None) }; // rf-x7
}

#[inline]
pub(crate) unsafe fn record_barriers(list: &ID3D12GraphicsCommandList, reason: &str, barriers: &[D3D12_RESOURCE_BARRIER]) {
    if switches().stats {
        count_barriers(barriers, 0);
    }
    let _range = range(list, reason);
    // SAFETY: the original ResourceBarrier call's preconditions apply.
    unsafe { list.ResourceBarrier(barriers) };
}

/// A single physical call with the leading title barrier adjacent to Victory's.
pub(crate) unsafe fn record_mixed(list: &ID3D12GraphicsCommandList, reason: &str, barriers: &[D3D12_RESOURCE_BARRIER]) {
    let (title, victory) = barriers.split_at(1);
    if switches().stats {
        count_barriers(title, TITLE_BARRIERS);
        count_barriers(victory, 0);
    }
    let _range = range(list, reason);
    unsafe { list.ResourceBarrier(barriers) };
}

/// Barriers emitted to implement an ordering command from the title.
#[inline]
pub(crate) unsafe fn title_barriers(list: &ID3D12GraphicsCommandList, barriers: &[D3D12_RESOURCE_BARRIER]) {
    if switches().stats {
        count_barriers(barriers, TITLE_BARRIERS);
    }
    // SAFETY: the original ResourceBarrier call's preconditions apply.
    unsafe { list.ResourceBarrier(barriers) };
}

#[inline]
pub(crate) fn submitted(lists: &[Option<ID3D12CommandList>]) {
    if !switches().stats {
        return;
    }
    for list in lists.iter().flatten() {
        let mut index = u8::MAX;
        let mut size = 1;
        // SAFETY: size describes the one writable byte; list is live.
        let result = unsafe { list.GetPrivateData(&ORIGIN, &mut size, Some((&raw mut index).cast())) };
        if result.is_ok() && size == 1 && index < 4 {
            TOTALS[LISTS + index as usize].fetch_add(1, Relaxed);
        }
    }
}

pub(crate) struct Detile(Option<Instant>);

#[inline]
pub(crate) fn detile() -> Detile {
    Detile(switches().stats.then(Instant::now))
}

impl Drop for Detile {
    #[inline]
    fn drop(&mut self) {
        if let Some(start) = self.0 {
            TOTALS[DETILE_US].fetch_add(start.elapsed().as_micros() as u64, Relaxed);
        }
    }
}

type Counts = [u64; COUNT];

fn per_frame(before: Counts, after: Counts, frames: u64) -> [f64; COUNT] {
    std::array::from_fn(|i| after[i].wrapping_sub(before[i]) as f64 / frames.max(1) as f64)
}

fn faults_per_second(old: u32, new: u32, elapsed: Duration) -> f64 {
    f64::from(new.wrapping_sub(old)) / elapsed.as_secs_f64()
}

fn process_memory() -> Option<PROCESS_MEMORY_COUNTERS_EX> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = std::mem::size_of_val(&counters) as u32;
    counters.cb = size;
    // SAFETY: the extended struct starts with PROCESS_MEMORY_COUNTERS and
    // the size tells Windows it can write the extended layout.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast(), size) };
    (ok != 0).then_some(counters)
}

fn system_commit() -> Option<u64> {
    let mut info = PERFORMANCE_INFORMATION::default();
    let size = std::mem::size_of_val(&info) as u32;
    info.cb = size;
    // SAFETY: the structure is writable for the specified size.
    let ok = unsafe { K32GetPerformanceInfo(&mut info, size) };
    (ok != 0).then(|| info.CommitTotal as u64 * info.PageSize as u64)
}

struct PagesIn {
    // Opaque PDH handles, used only under the sampler lock.
    query: usize,
    counter: usize,
}

impl PagesIn {
    fn new() -> Option<Self> {
        let mut query = std::ptr::null_mut();
        let mut counter = std::ptr::null_mut();
        // SAFETY: writable handle slots and a static, NUL-terminated path.
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return None;
            }
            if PdhAddEnglishCounterW(query, windows_sys::w!("\\Memory\\Pages Input/sec"), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            let sampler = Self { query: query as usize, counter: counter as usize };
            PdhCollectQueryData(query);
            Some(sampler)
        }
    }

    fn sample(&self) -> Option<f64> {
        let mut value = PDH_FMT_COUNTERVALUE::default();
        // SAFETY: the query still owns the counter. Read the union as DOUBLE
        // only after PDH reports valid data for that format.
        unsafe {
            if PdhCollectQueryData(self.query as PDH_HQUERY) != 0
                || PdhGetFormattedCounterValue(self.counter as PDH_HCOUNTER, PDH_FMT_DOUBLE, std::ptr::null_mut(), &mut value) != 0
                || !matches!(value.CStatus, PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA)
            {
                return None;
            }
            Some(value.Anonymous.doubleValue)
        }
    }
}

struct Sample {
    at: Instant,
    frame: u64,
    counts: Counts,
    faults: Option<(Instant, u32)>,
    pages_in: Option<PagesIn>,
}

#[inline]
pub(crate) fn frame(frame: u64) {
    crate::rf_x7::frame(frame); // rf-x7
    if !switches().stats {
        return;
    }
    static SAMPLE: Mutex<Option<Sample>> = Mutex::new(None);
    let now = Instant::now();
    let mut sample = SAMPLE.lock().unwrap_or_else(|e| e.into_inner());
    let previous = sample.get_or_insert_with(|| Sample {
        at: now,
        frame,
        counts: TOTALS.each_ref().map(|count| count.load(Relaxed)),
        faults: process_memory().map(|c| (now, c.PageFaultCount)),
        pages_in: PagesIn::new(),
    });
    if now.saturating_duration_since(previous.at) < Duration::from_secs(5) || frame <= previous.frame {
        return;
    }
    let process = process_memory();
    let fault_rate = process.and_then(|c| {
        previous.faults.replace((now, c.PageFaultCount))
            .map(|(at, faults)| faults_per_second(faults, c.PageFaultCount, now - at))
    });
    let counts = TOTALS.each_ref().map(|count| count.load(Relaxed));
    let frames = frame - previous.frame;
    let [uav, transition, aliasing, title_uav, title_transition, title_aliasing, title, coherence, copy, prelude, detile_us, global] =
        per_frame(previous.counts, counts, frames);
    previous.at = now;
    previous.frame = frame;
    previous.counts = counts;
    let pages_in_sec = previous.pages_in.as_ref().and_then(PagesIn::sample);
    tracing::info!(
        frame, frames,
        page_faults_sec = ?fault_rate,
        working_set_bytes = ?process.map(|c| c.WorkingSetSize),
        private_bytes = ?process.map(|c| c.PrivateUsage),
        system_commit_bytes = ?system_commit(),
        system_pages_input_sec = ?pages_in_sec,
        uav_per_frame = uav, transition_per_frame = transition, aliasing_per_frame = aliasing,
        global_barrier_per_frame = global,
        title_uav_per_frame = title_uav, title_transition_per_frame = title_transition, title_aliasing_per_frame = title_aliasing,
        list_title_per_frame = title, list_aux_coherence_per_frame = coherence,
        list_aux_copy_per_frame = copy, list_prelude_per_frame = prelude,
        "vx:tex.detile_cpu_us_per_frame" = detile_us,
        "rf-x2:"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_range_names() {
        assert_eq!(Origin::Title.name(), "list:title");
        assert_eq!(Origin::Thin.name(), Origin::Title.name());
        assert_eq!(Origin::Coherence.name(), "list:aux.coherence");
        assert_eq!(Origin::Copy.name(), "list:aux.copy");
        assert_eq!(Origin::Prelude.name(), "list:prelude");
        assert_eq!(title_name(0x123), "title:0000000000000123");
        assert_eq!(ambient("ambient: gather"), "vx:ambient.gather");
    }

    #[test]
    fn title_groups_follow_the_selected_granularity() {
        let mut title = Title { binding: Some((1, 10)), ..Title::default() };
        assert_eq!(title.work(1), Some(10));
        title.binding = Some((2, 20));
        for _ in 1..GROUP_SIZE {
            assert_eq!(title.work(1), None);
        }
        assert_eq!(title.work(1), Some(20));
        assert_eq!(title.work(2), None);
        title.binding = Some((3, 20));
        assert_eq!(title.work(2), Some(20));
    }

    #[test]
    fn rates_use_counter_deltas_and_handle_wraparound() {
        let mut before = [10; COUNT];
        let mut after = [20; COUNT];
        before[BAR_ALIASING] = u64::MAX - 2;
        after[BAR_ALIASING] = 2;
        let mut expected = [2.0; COUNT];
        expected[BAR_ALIASING] = 1.0;
        assert_eq!(per_frame(before, after, 5), expected);
        assert_eq!(faults_per_second(u32::MAX - 2, 2, Duration::from_secs(2)), 2.5);
    }

    #[test]
    fn a_batch_counts_barrier_descriptors_by_type() {
        let batch = [
            D3D12_RESOURCE_BARRIER_TYPE_UAV,
            D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
        ].map(|kind| D3D12_RESOURCE_BARRIER { Type: kind, ..Default::default() });
        assert_eq!(barrier_counts(&batch), [1, 2, 1]);
    }
}
