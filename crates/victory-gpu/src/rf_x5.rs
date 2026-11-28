//! Submission write-watch experiments and their presentation accounting.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) struct Settings {
    pub stats: bool,
    pub always_dirty: bool,
    pub lazy_rearm: bool,
    pub fast_fault: bool,
    window: u64,
    fault_ns_per_frame: u64,
    pause: u64,
}

pub(crate) fn settings() -> &'static Settings {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    SETTINGS.get_or_init(|| {
        let on = |name| std::env::var_os(name).is_some_and(|v| v == "1");
        let number = |name| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
        let always_dirty = on("VICTORY_RF_X5_ALWAYS_DIRTY");
        Settings {
            stats: on("VICTORY_RF_X5_STATS"),
            always_dirty,
            lazy_rearm: on("VICTORY_RF_X5_LAZY_REARM"),
            fast_fault: std::env::var_os("VICTORY_RF_X5_FAST_FAULT").map_or(always_dirty, |v| v == "1"),
            window: number("VICTORY_RF_X5_DIRTY_WINDOW").filter(|v| (2..=64).contains(v)).unwrap_or(16),
            fault_ns_per_frame: number("VICTORY_RF_X5_FAULT_US_PER_FRAME")
                .filter(|v| (1..=1000).contains(v)).unwrap_or(12) * 1000,
            pause: number("VICTORY_RF_X5_DIRTY_PROBE").filter(|v| (1..=600).contains(v)).unwrap_or(16),
        }
    })
}

#[derive(Default)]
pub(crate) struct Heat {
    first: u64,
    faults: u64,
    ns: u64,
    pause_until: u64,
}

impl Heat {
    /// Charge every guard opening to its block. Divide by the full observation
    /// window, including frames without faults, even when a burst qualifies early.
    pub(crate) fn fault(&mut self, frame: u64, ns: u64, settings: &Settings) {
        if self.pauses(frame) {
            return;
        }
        if self.faults == 0 || frame.saturating_sub(self.first) >= settings.window {
            *self = Self { first: frame, ..Self::default() };
        }
        self.faults += 1;
        self.ns = self.ns.saturating_add(ns);
        if self.faults >= 2 && self.ns >= settings.fault_ns_per_frame * settings.window {
            self.pause_until = frame + settings.pause;
            self.faults = 0;
            self.ns = 0;
        }
    }

    pub(crate) fn pauses(&self, frame: u64) -> bool {
        frame < self.pause_until
    }

    pub(crate) fn retain(&self, frame: u64) -> bool {
        self.pauses(frame) || self.faults != 0 && frame.saturating_sub(self.first) < settings().window
    }
}

/// Global budget for work introduced by deferring descriptor rearm. Charge
/// every deferral, including later reads of a page already left open.
#[derive(Default)]
pub(crate) struct DescriptorBudget {
    frame: u64,
    used: usize,
}

impl DescriptorBudget {
    pub(crate) const EMPTY: Self = Self { frame: 0, used: 0 };

    pub(crate) fn take(&mut self, frame: u64, eligible: impl FnOnce() -> bool) -> bool {
        const BLOCKS_PER_FRAME: usize = (1 << 20) / crate::watch::PAGE;
        if self.frame != frame {
            *self = Self { frame, ..Self::EMPTY };
        }
        if self.used == BLOCKS_PER_FRAME || !eligible() {
            return false;
        }
        self.used += 1;
        true
    }
}

/// Extend one sorted run of consecutive eligible blocks, returning the
/// preceding run when a gap starts a new one.
pub(crate) fn push_block(run: &mut Option<(usize, usize)>, block: usize) -> Option<(usize, usize)> {
    match run {
        Some((_, end)) if *end == block => {
            *end += 1;
            None
        }
        _ => run.replace((block, block + 1)),
    }
}

static FRAME: AtomicU64 = AtomicU64::new(0);

pub(crate) fn frame() -> u64 {
    FRAME.load(Relaxed)
}

pub(crate) fn submission() -> u64 {
    super::STARTED.load(Relaxed)
}

pub(crate) fn present() {
    if descriptor_heat() {
        FRAME.fetch_add(1, Relaxed);
    }
}

pub(crate) fn descriptor_heat() -> bool {
    settings().always_dirty || crate::rf_x9::defer_descriptor_rearm()
}

const FIELDS: usize = 7;
type Counts = [u64; FIELDS];
const BUCKETS: usize = 1 << 15;
static TOTAL: Counters = Counters::new();
static PAGES: [Counters; BUCKETS] = [const { Counters::new() }; BUCKETS];

struct Counters {
    key: AtomicUsize,
    values: [AtomicU64; FIELDS],
}

impl Counters {
    const fn new() -> Self {
        Self { key: AtomicUsize::new(0), values: [const { AtomicU64::new(0) }; FIELDS] }
    }

    fn add(&self, first: usize, values: &[u64]) {
        for (field, &value) in self.values[first..].iter().zip(values) {
            field.fetch_add(value, Relaxed);
        }
    }

    fn take(&self) -> Counts {
        self.values.each_ref().map(|field| field.swap(0, Relaxed))
    }
}

/// Bounded, allocation-free recording even when called from the VEH under
/// the guard lock. Totals include events that exhaust the address table.
fn record(at: usize, first: usize, values: &[u64]) {
    TOTAL.add(first, values);
    let key = at / crate::watch::PAGE + 1;
    let hash = key.wrapping_mul(2654435761) ^ (key >> 16);
    for slot in 0..8 {
        let bucket = &PAGES[hash.wrapping_add(slot) % BUCKETS];
        let found = bucket.key.load(Relaxed);
        let owned = if found == 0 {
            match bucket.key.compare_exchange(0, key, Relaxed, Relaxed) {
                Ok(_) => true,
                Err(winner) => winner == key,
            }
        } else {
            found == key
        };
        if owned {
            bucket.add(first, values);
            break;
        }
    }
}

pub(crate) fn timer() -> Option<Instant> {
    settings().stats.then(Instant::now)
}

pub(crate) fn fault_timer() -> Option<Instant> {
    (settings().stats || descriptor_heat()).then(Instant::now)
}

pub(crate) fn protected(at: usize, len: usize, start: Instant, fault: bool) {
    let ns = start.elapsed().as_nanos() as u64;
    if fault {
        FAULT_PARTS[2].fetch_add(ns, Relaxed);
    }
    record(at, 4, &[1, len.div_ceil(4096) as u64, ns]);
}

pub(crate) struct Fault {
    started: Option<Instant>,
    at: usize,
    code: usize,
    guarded: bool,
}

impl Fault {
    pub(crate) fn new(started: Option<Instant>, at: usize, code: usize, guarded: bool) -> Self {
        Self { started, at, code, guarded }
    }

    pub(crate) fn handled(&mut self) {
        self.guarded = true;
    }
}

impl Drop for Fault {
    fn drop(&mut self) {
        if self.guarded && settings().stats && let Some(start) = self.started {
            let ns = start.elapsed().as_nanos() as u64;
            let group = if title_thread(self.code) { 0 } else { 2 };
            record(self.at, group, &[1, ns]);
        }
    }
}

type QueryThread = unsafe extern "system" fn(
    windows_sys::Win32::Foundation::HANDLE, u32, *mut std::ffi::c_void, u32, *mut u32,
) -> i32;
static QUERY_THREAD: OnceLock<Option<QueryThread>> = OnceLock::new();
static TITLE: OnceLock<Vec<(usize, usize)>> = OnceLock::new();

/// Resolve modules outside the exception handler. The runtime loads the
/// title as victory-image*, even when its main thread starts in our exe.
pub(crate) fn init_stats() {
    if !settings().stats || TITLE.get().is_some() {
        return;
    }
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    QUERY_THREAD.get_or_init(|| {
        // SAFETY: ntdll stays loaded, and the signature is the system ABI.
        unsafe {
            let module = GetModuleHandleW(windows_sys::core::w!("ntdll.dll"));
            GetProcAddress(module, c"NtQueryInformationThread".as_ptr().cast())
                .map(|f| std::mem::transmute::<_, QueryThread>(f))
        }
    });
    let mut title = Vec::new();
    // SAFETY: a module snapshot of this process and a correctly sized record.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, 0);
        if snap as isize != -1 {
            let mut entry: MODULEENTRY32W = std::mem::zeroed();
            entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
            let mut next = Module32FirstW(snap, &mut entry);
            while next != 0 {
                let name = String::from_utf16_lossy(&entry.szModule);
                if name.to_ascii_lowercase().starts_with("victory-image") {
                    let start = entry.modBaseAddr as usize;
                    title.push((start, start + entry.modBaseSize as usize));
                }
                next = Module32NextW(snap, &mut entry);
            }
            CloseHandle(snap);
        }
    }
    if !title.is_empty() {
        let _ = TITLE.set(title);
    }
}

fn title_thread(code: usize) -> bool {
    let Some(title) = TITLE.get() else { return false };
    let in_title = |at| title.iter().any(|&(a, b)| a <= at && at < b);
    thread_local! {
        static RAN_TITLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static START: usize = {
            use windows_sys::Win32::System::Threading::GetCurrentThread;
            let mut start = 0usize;
            if let Some(Some(query)) = QUERY_THREAD.get() {
                // SAFETY: the current thread pseudo-handle and a pointer-sized answer.
                unsafe { query(GetCurrentThread(), 9, (&raw mut start).cast(), size_of::<usize>() as u32, std::ptr::null_mut()) };
            }
            start
        };
    }
    if in_title(code) {
        let _ = RAN_TITLE.try_with(|ran| ran.set(true));
        return true;
    }
    RAN_TITLE.try_with(|ran| ran.get()).unwrap_or(false)
        || START.try_with(|&start| in_title(start)).unwrap_or(false)
}

static FAULT_PARTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

pub(crate) fn fault_lock(wait: Duration, settle: Duration, work: Duration) {
    FAULT_PARTS[0].fetch_add(wait.as_nanos() as u64, Relaxed);
    FAULT_PARTS[1].fetch_add(settle.as_nanos() as u64, Relaxed);
    FAULT_PARTS[3].fetch_add(1, Relaxed);
    FAULT_PARTS[5].fetch_add(work.as_nanos() as u64, Relaxed);
}

pub(crate) fn fast_opened() {
    if settings().stats {
        FAULT_PARTS[4].fetch_add(1, Relaxed);
    }
}

static POLICY: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

pub(crate) fn deferred(hot: bool) {
    if settings().stats {
        POLICY[usize::from(!hot)].fetch_add(1, Relaxed);
    }
}

pub(crate) fn copied(bytes: usize) {
    if settings().stats {
        POLICY[2].fetch_add(1, Relaxed);
        POLICY[3].fetch_add(bytes as u64, Relaxed);
    }
}

pub(crate) fn descriptor_deferred() {
    if settings().stats {
        POLICY[4].fetch_add(1, Relaxed);
    }
}

#[derive(Default)]
struct Sample {
    elapsed: Duration,
    frames: u64,
    prepare_us: u64,
    split: [u64; 4],
    refresh: u64,
}

/// Called from the rf-x3 meter with the same one-second windows and units.
pub(crate) fn sample(elapsed: Duration, frames: u64, prepare_us: u64, split: [u64; 16]) {
    if !settings().stats {
        return;
    }
    init_stats();
    static SAMPLE: Mutex<Sample> = Mutex::new(Sample { elapsed: Duration::ZERO, frames: 0, prepare_us: 0, split: [0; 4], refresh: 0 });
    let mut sample = SAMPLE.lock().unwrap_or_else(|e| e.into_inner());
    sample.elapsed += elapsed;
    sample.frames += frames;
    sample.prepare_us += prepare_us;
    for (value, i) in sample.split.iter_mut().zip([0, 1, 2, 4]) {
        *value += split[i];
    }
    sample.refresh += split[6];
    if sample.elapsed < Duration::from_secs(5) || sample.frames == 0 {
        return;
    }
    let taken = std::mem::take(&mut *sample);
    drop(sample);
    let sample = taken;
    let frames = sample.frames as f64;
    let total = TOTAL.take();
    let mut objects: BTreeMap<(u64, u64), Counts> = BTreeMap::new();
    let mut unattributed = [0; FIELDS];
    for page in &PAGES {
        let key = page.key.load(Relaxed);
        if key == 0 {
            continue;
        }
        let counts = page.take();
        if counts == [0; FIELDS] {
            continue;
        }
        let at = (key - 1) * crate::watch::PAGE;
        let object = crate::descriptor::shader_heap_extent(at, 1)
            .or_else(|| crate::shadow::in_buffer(at as u64, 1));
        let dst = match object {
            Some(object) => objects.entry(object).or_default(),
            None => &mut unattributed,
        };
        for (field, value) in dst.iter_mut().zip(counts) {
            *field += value;
        }
    }
    let mut top: Vec<_> = objects.into_iter().collect();
    top.sort_unstable_by_key(|&(key, c)| (std::cmp::Reverse(c[1] + c[3] + c[6]), key));
    let missing: Counts = std::array::from_fn(|i| {
        total[i].saturating_sub(top.iter().map(|(_, c)| c[i]).sum::<u64>()).max(unattributed[i])
    });
    let format = |c: Counts| {
        let [title, title_ns, runtime, runtime_ns, calls, pages, protect_ns] = c.map(|v| v as f64 / frames);
        format!("title={title:.1}/{:.3} runtime={runtime:.1}/{:.3} vp={calls:.1}/{pages:.1}/{:.3}",
            title_ns / 1e6, runtime_ns / 1e6, protect_ns / 1e6)
    };
    let top = top.into_iter().take(10).map(|((at, len), c)| format!(" {at:#x}+{len:#x}[{}]", format(c))).collect::<String>();
    let [watch, flights, steps, card] = sample.split.map(|n| n as f64 / frames / 1e6);
    let refresh = sample.refresh as f64 / frames / 1e6;
    let [hot, deferred, copies, bytes, desc] = POLICY.each_ref().map(|c| c.swap(0, Relaxed) as f64 / frames);
    let [wait, settle, vp, locked, fast, work] = FAULT_PARTS.each_ref().map(|c| c.swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "rf-x5: frames={} fps={:.2} preparing={:.3}ms submit-ms[watch={watch:.3} flights={flights:.3} steps={steps:.3} card={card:.3} refresh={refresh:.3}] faults[count/ms] vp[calls/4K-pages/ms] {} veh[wait={:.3}ms settle={:.3}ms locked-work={:.3}ms vp={:.3}ms lock-attempts={locked:.1} unlocked={fast:.1}] policy[hot-blocks={hot:.1} lazy-blocks={deferred:.1} direct-copies={copies:.1} direct-MB={:.2} descriptor-deferrals={desc:.1}] unattributed[{}] top10{}",
        sample.frames, frames / sample.elapsed.as_secs_f64(), sample.prepare_us as f64 / frames / 1e3,
        format(total), wait / 1e6, settle / 1e6, work / 1e6, vp / 1e6, bytes / 1e6, format(missing), top,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST: Settings = Settings { stats: false, always_dirty: true, lazy_rearm: false, fast_fault: false,
        window: 4, fault_ns_per_frame: 100, pause: 8 };

    #[test]
    fn expensive_same_frame_faults_qualify() {
        let mut heat = Heat::default();
        heat.fault(1, 350, &TEST);
        assert!(!heat.pauses(1));
        heat.fault(1, 50, &TEST);
        assert!(heat.pauses(1));
    }

    #[test]
    fn cheap_or_outdated_faults_do_not_qualify_and_pause_does_not_feed_itself() {
        let mut heat = Heat::default();
        heat.fault(0, 350, &TEST);
        for frame in [1, 2, 3, 4] {
            heat.fault(frame, 10, &TEST);
        }
        assert!(!heat.pauses(4));
        heat.fault(4, 400, &TEST);
        assert!(heat.pauses(11));
        heat.fault(11, 500, &TEST);
        assert!(!heat.pauses(12));
        heat.fault(12, 500, &TEST);
        assert!(!heat.pauses(12));
    }

    #[test]
    fn descriptor_budget_limits_deferrals_per_frame() {
        let mut budget = DescriptorBudget::EMPTY;
        assert!(!budget.take(5, || false));
        for _ in 0..(1 << 20) / crate::watch::PAGE {
            assert!(budget.take(5, || true));
        }
        assert!(!budget.take(5, || panic!("the budget is exhausted")));
        assert!(budget.take(6, || true));
    }

    #[test]
    fn repeated_descriptor_rounds_bound_extra_reads_and_reduce_rearms() {
        const HEAPS: usize = 4;
        const ROUNDS: usize = 24;
        const HOT_BLOCKS: usize = 8;
        let mut heat: [Heat; HEAPS * HOT_BLOCKS] = Default::default();
        for entry in &mut heat {
            entry.fault(1, 350, &TEST);
            entry.fault(1, 50, &TEST);
        }
        let mut budget = DescriptorBudget::EMPTY;
        let mut deferred = 0;
        let mut rearmed = 0;
        for _ in 0..ROUNDS {
            for entry in &heat {
                if budget.take(2, || entry.pauses(2)) {
                    deferred += 1;
                } else {
                    rearmed += 1;
                }
            }
        }
        assert_eq!(deferred * crate::watch::PAGE, 1 << 20);
        assert_eq!(rearmed, HEAPS * ROUNDS * HOT_BLOCKS - deferred);
        assert!(!budget.take(9, || heat[0].pauses(9)));
    }

    #[test]
    fn coalesces_only_contiguous_eligible_blocks() {
        let mut run = None;
        let mut runs: Vec<_> = [1, 2, 5, 6, 9].into_iter().filter_map(|b| push_block(&mut run, b)).collect();
        runs.extend(run);
        assert_eq!(runs, [(1, 3), (5, 7), (9, 10)]);
    }
}
