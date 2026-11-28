//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost. Passes over the
//! screen (no vertex input, no depth) have workers and a longer budget of
//! their own ([`screen_budget`]): the title fills its lighting tables with
//! such draws once, at the world's first frame, and behind a world's worth of
//! pipelines they were dropped; that run had no lamps and no diffuse light to
//! its end (run textures-d2, the first after a new translation; Belobug
//! 18e57db8 found the same).

use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    /// Taken by a worker from [`Queue::slow`].
    was_slow: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// A pass over the screen ([`GraphicsState::screen_pass`]) or a confirmed
    /// opening-scene / bake producer that must never sit behind cold RT shaders.
    screen: bool,
    /// Confirmed first-scene / bake producer prepared early even on a cold cache.
    scene_input: bool,
    /// Why this pipeline is intentionally skipped (e.g. RT resolve without BVH).
    left_out: Option<&'static str>,
    /// Made ahead of the title's own creation of it ([`prewarm_add`]).
    prewarmed: AtomicBool,
}

fn named(shaders: &[Arc<Shader>], name: &[u8]) -> bool {
    shaders
        .iter()
        .any(|s| s.stage == "compute" && s.container.windows(name.len()).any(|w| w == name))
}

/// Shader hashes whose pipelines are left out in every build
/// (`VICTORY_LEAVE_OUT=<16 hex digits>[,..]`; `VICTORY_SKIP_PIPELINES` does
/// the same in diagnostics builds only). The decal heap's
/// `CS_DefragHeap_ComputeRelocations` (02384a60fda6f2d0), translatable for
/// 32-lane hosts through host waves that meet by spinning, took 11.85 ms of
/// the card a frame in the first gameplay area (timing run, 2026-11-26).
fn leave_out() -> &'static [String] {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    HASHES.get_or_init(|| {
        std::env::var("VICTORY_LEAVE_OUT")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect()
    })
}

/// The light in the water's volume, in tiles (`water_shading:CS_TransmissionVolumetrics_Tiled`
/// and `_Tiled_LocalLights`, GTA VI 1.0.2.1): programs of one 64-lane wave a group, which a
/// host of 32-lane waves gets in the wave-wide form only (`victory_shader::emit`, 4 and 7
/// branches taken wave-wide). In that form they leave whole tiles of open water dark, around
/// boats, along shores and under bridges, and take about 15 ms of the card a frame (RTX 5070,
/// runs r_water1 and r_water3 of 2026-11-26: 25 fps with them, 30 without, the blocks gone;
/// fxceless/victory-fork's README of 2026-11-27 names the same two programs and form). Left
/// out there, the water has no light of its own depth (darker, bluer) and no blocks.
/// foxhound 0.2.0-pre.3 fixed the blocks (64-lane programs on 32-lane waves:
/// `VICTORY_LOOPS_WAVE`, `VICTORY_LDS_FIRST_INIT`), so they run now unless
/// `VICTORY_WATER_LIGHT=0` (an empty value, an earlier version's saved OFF, runs them too).
const WATER_LIGHT: [&str; 2] = ["5dc3b25986622949", "6f225b641c5b0708"];

fn water_light_left_out(shaders: &[Arc<Shader>]) -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    *ASKED.get_or_init(|| std::env::var_os("VICTORY_WATER_LIGHT").is_some_and(|v| v == "0"))
        && victory_shader::emit::host_settings()[0] == 32
        && shaders.iter().any(|s| WATER_LIGHT.contains(&s.hash.as_str()))
}

fn left_out(shaders: &[Arc<Shader>]) -> Option<&'static str> {
    if !leave_out().is_empty()
        && shaders.iter().any(|s| {
            leave_out().contains(&s.hash) && !super::rf_x6::restore_left_out(&s.hash)
        })
    {
        return Some("named in VICTORY_LEAVE_OUT, left out");
    }
    if water_light_left_out(shaders) {
        return Some("the water's tiled light on a host of 32-lane waves, left out (VICTORY_WATER_LIGHT=0)");
    }
    static SKIP_RT_RESOLVE: OnceLock<bool> = OnceLock::new();
    let skip_rt = *SKIP_RT_RESOLVE.get_or_init(|| {
        let builder = std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1");
        if builder {
            return false;
        }
        // Without BVH, ~1,000 CS_RaytracingResolve shaders shade no hits while
        // clogging the cold compiler queue; skip unless VICTORY_RT_RESOLVE=1.
        std::env::var_os("VICTORY_RT_RESOLVE").is_none_or(|v| v == "0")
    });
    if skip_rt && named(shaders, b"CS_RaytracingResolve") {
        return Some("shades ray-tracing hits without BVH, left out");
    }
    // The screen probes' ray activation runs without the structure builder
    // too: left out, the probes kept stale values and surfaces lit mostly by
    // them took a magenta or violet cast (papers on a wall, irises), at no
    // frame-rate gain (bisected and measured by a tester, issue #2 of the
    // public repository, 2026-11-27). `VICTORY_ACTIVATE_RAYS=0`: left out,
    // as before.
    static SKIP_ACTIVATE_RAYS: OnceLock<bool> = OnceLock::new();
    let skip_activate =
        *SKIP_ACTIVATE_RAYS.get_or_init(|| std::env::var_os("VICTORY_ACTIVATE_RAYS").is_some_and(|v| v == "0"));
    if skip_activate && named(shaders, b"CS_ScreenProbes_ActivateRays") {
        return Some("ray activation shader without BVH, left out");
    }
    None
}

/// Confirmed first-scene producers. Prepare these at creation even on a cold
/// launch so cutscene lighting, tiling, and bakes never miss their first frame.
fn scene_input(shaders: &[Arc<Shader>]) -> bool {
    shaders.iter().any(|s| {
        matches!(
            s.hash.as_str(),
            "970c4fbb3b017c0e"
                | "068c90badc86f1fd"
                | "e4a7e088c7780ce8"
                | "ebe4e53f031f5249"
                | "2cc4c197ee29e291"
                | "369cbe906ea72764"
        ) || crate::draw::BAKE_PIXEL_SHADERS.contains(&s.hash.as_str())
    })
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        let scene_input = scene_input(&shaders);
        let screen = (graphics.as_ref().is_some_and(|g| g.screen_pass())
            && shaders.iter().all(|s| s.stage != "mesh"))
            || scene_input;
        let left_out = left_out(&shaders);
        Pending {
            screen,
            scene_input,
            left_out,
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            was_slow: AtomicBool::new(false),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            prewarmed: AtomicBool::new(false),
        }
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    pub(super) fn drop_reason(&self) -> &'static str {
        if let Some(why) = self.left_out {
            return why;
        }
        match self.made.get() {
            None => "host pipeline pending",
            Some(Some(_)) => "host pipeline became ready after lookup",
            Some(None) if self.shaders.iter().any(|s| matches!(s.translated.get(), Some(None))) => {
                "shader translation failed"
            }
            Some(None) => "host pipeline creation refused",
        }
    }

    fn make(&self) {
        self.made.get_or_init(|| {
            let began = Instant::now();
            let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
            super::library::note_time(&self.key, began.elapsed().as_millis() as u64);
            if self.prewarmed.load(Ordering::Relaxed) {
                PRE_MADE.fetch_add(1, Ordering::Relaxed);
            }
            made
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): left out on purpose
/// ([`left_out`]), its shaders that do not translate, else "refused" or
/// "being made"; `None` if it has one. (A pipeline left out is never made:
/// it was counted as "being made" in every frame, some 630 indirect
/// ray-tracing resolves a frame in gameplay, while no pipeline was queued.)
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    if let Some(why) = p.left_out {
        return Some(format!("left out on purpose: {why}"));
    }
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// The hashes of the shaders `p` is made of.
pub(crate) fn hashes(p: &Pending) -> String {
    let hashes: Vec<&str> = p.shaders.iter().map(|s| s.hash.as_str()).collect();
    hashes.join("+")
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// As [`wait_budget`], for passes over the screen (`VICTORY_SCREEN_WAIT_MS`,
/// 10 s; 0: like the others). Below the title's 30 s watchdog.
fn screen_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_SCREEN_WAIT_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(10_000);
        Duration::from_millis(millis)
    })
}

/// Whether passes over the screen go before the others.
fn screens_first() -> bool {
    !screen_budget().is_zero()
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Draws and dispatches (direct and indirect) asked of a pipeline left out on
/// purpose ([`left_out`]), since the last [`take_left_out`].
static LEFT_OUT: AtomicU64 = AtomicU64::new(0);

/// See [`LEFT_OUT`] (for the frame line).
pub(crate) fn take_left_out() -> u64 {
    LEFT_OUT.swap(0, Ordering::Relaxed)
}

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    note_draw();
    if p.left_out.is_some() {
        LEFT_OUT.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        if p.prewarmed.load(Ordering::Relaxed) {
            LATE.fetch_add(1, Ordering::Relaxed);
        }
        start(p, REQUESTED);
        wait(p, crate::frame::current());
    }
    p.made.get()?.as_ref()
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    if p.left_out.is_some() {
        return false;
    }
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it),
/// or if it is a confirmed opening-scene / bake producer.
pub(crate) fn early(p: &Arc<Pending>) {
    if p.left_out.is_some() {
        return;
    }
    if !p.shaders.is_empty() && (p.scene_input || super::library::wanted_early(&p.key)) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, if p.scene_input { REQUESTED } else { EARLY });
    }
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let screen = p.screen && screens_first();
    let budget = if screen { screen_budget() } else { wait_budget() };
    if budget.is_zero() {
        return;
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now); of the passes over the screen too.
    static SPENT: [Mutex<(u64, Duration)>; 2] = [const { Mutex::new((u64::MAX, Duration::ZERO)) }; 2];
    let spent_of = &SPENT[usize::from(screen)];
    let shared = shared_budget();
    let left = {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    if screen && waited > Duration::from_millis(500) {
        tracing::info!(
            ms = waited.as_millis() as u64,
            made = p.made.get().is_some(),
            shaders = ?p.shaders,
            "a pass over the screen was waited for (VICTORY_SCREEN_WAIT_MS=0: dropped past half a second a frame)"
        );
    }
    {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

/// How many workers may make pipelines at once that are known to take long
/// and that no draw has asked for: a quarter of them. On a fresh cache (a
/// change of the translator or the driver) the early list begins with
/// programs that take tens of seconds each: all 16 workers were in them
/// through the whole of the title's loading, 97 pipelines made in 20 s, and
/// the first minute and a half of the world ran at 0.5 to 1.6 s a frame,
/// dropping up to 2,900 draws a frame (run shaders-1).
/// `VICTORY_PIPELINE_SLOW=0`: in the order the title creates them, as before.
fn slow_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        if std::env::var_os("VICTORY_PIPELINE_SLOW").is_some_and(|v| v == "0") {
            usize::MAX
        } else {
            (worker_count(None, std::thread::available_parallelism().map_or(2, |n| n.get())) / 4).max(1)
        }
    })
}

/// An early pipeline in the queue: the one an earlier run first used soonest
/// goes first (`rank`, see `library::rank`); those with no recorded place
/// (`u64::MAX`) and equal places keep the order they were queued in (`n`).
struct Ranked {
    rank: u64,
    n: u64,
    p: Arc<Pending>,
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        (self.rank, self.n) == (other.rank, other.n)
    }
}

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    /// Reversed: a `BinaryHeap` gives the greatest first.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.rank, other.n).cmp(&(self.rank, self.n))
    }
}

struct Queue {
    /// Requested passes over the screen: before all others, and the only ones
    /// [`SCREEN_WORKERS`] take.
    screen: VecDeque<Arc<Pending>>,
    /// In the order earlier runs first used them (`VICTORY_PIPELINE_ORDER=0`:
    /// the title's creation order, as before).
    early: BinaryHeap<Ranked>,
    /// Those of the early ones known to take long, and how many of them are
    /// being made ([`slow_limit`]).
    slow: BinaryHeap<Ranked>,
    slow_now: usize,
    requested: VecDeque<Arc<Pending>>,
    queued: u64,
}

/// Whether early pipelines are made in the order earlier runs first used them.
fn by_first_use() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_ORDER").is_none_or(|v| v != "0"))
}

impl Queue {
    const fn new() -> Self {
        Self {
            screen: VecDeque::new(),
            early: BinaryHeap::new(),
            slow: BinaryHeap::new(),
            slow_now: 0,
            requested: VecDeque::new(),
            queued: 0,
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        self.push_as(p, priority, slow_limit() != usize::MAX && super::library::slow(&p.key))
    }

    fn push_as(&mut self, p: &Arc<Pending>, priority: u8, slow: bool) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED && p.screen && screens_first() {
            self.screen.push_back(p.clone());
        } else if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else {
            self.queued += 1;
            let rank = if by_first_use() { super::library::rank(&p.key) } else { u64::MAX };
            let entry = Ranked { rank, n: self.queued, p: p.clone() };
            if slow {
                self.slow.push(entry);
            } else {
                self.early.push(entry);
            }
        }
        Some(prev == NOT_QUEUED)
    }

    /// Whether the next known-slow pipeline is one an earlier run used before
    /// the next other early one.
    fn slow_is_sooner(&self) -> bool {
        matches!((self.early.peek(), self.slow.peek()), (Some(early), Some(slow)) if slow > early)
    }

    /// The next known-slow pipeline, if fewer than `limit` are being made.
    fn take_slow(&mut self, limit: usize) -> Option<Arc<Pending>> {
        if self.slow_now >= limit {
            return None;
        }
        let p = unclaimed(&mut self.slow)?;
        p.was_slow.store(true, Ordering::Relaxed);
        self.slow_now += 1;
        Some(p)
    }

    fn pop(&mut self, screens_only: bool, slow_limit: usize) -> Option<Arc<Pending>> {
        let mut p = self.screen.pop_front();
        if p.is_none() && !screens_only {
            p = self.requested.pop_front();
            // While the title loads, all go in the order earlier runs used
            // them in, the slow ones among them (as many at once as the limit
            // allows); otherwise the slow ones wait behind the others.
            if p.is_none() && LOADING.load(Ordering::Relaxed) && by_first_use() && self.slow_is_sooner() {
                p = self.take_slow(slow_limit);
            }
            if p.is_none() {
                p = unclaimed(&mut self.early);
            }
            if p.is_none() {
                p = self.take_slow(slow_limit);
            }
        }
        let p = p?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

/// The next entry of `list` that was not promoted and claimed already.
fn unclaimed(list: &mut BinaryHeap<Ranked>) -> Option<Arc<Pending>> {
    std::iter::from_fn(|| list.pop().map(|r| r.p)).find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            let waiting = WAITING.fetch_add(1, Ordering::Relaxed) + 1;
            PEAK.fetch_max(waiting, Ordering::Relaxed);
            if waiting >= BOOST_PENDING && boost_level().is_some() {
                BOOST.store(true, Ordering::Relaxed);
            }
        }
        // (One woken could be a worker that takes passes over the screen
        // alone: it would leave the pipeline to nobody.)
        AVAILABLE.notify_all();
    }
}

/// Workers that make passes over the screen alone, besides the others.
const SCREEN_WORKERS: usize = 2;

/// Pipelines waiting at once from which the workers are in a hurry (a world
/// entry or a cold cache queues hundreds; a draw or two does not).
const BOOST_PENDING: u64 = 32;

/// Whether the workers are in a hurry: set when [`BOOST_PENDING`] pipelines
/// wait, cleared when they are all made ([`drained`]). Then the workers run
/// above their usual priority and may all take the pipelines known to take long.
static BOOST: AtomicBool = AtomicBool::new(false);

/// How many workers there are (0 until started).
static WORKERS: AtomicUsize = AtomicUsize::new(0);

/// The priority of workers in a hurry: `VICTORY_PIPELINE_BOOST` unset or
/// `below`: below normal, `normal`: normal, `0`: none (as before).
fn boost_level() -> Option<i32> {
    use windows::Win32::System::Threading::{THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL};
    static LEVEL: OnceLock<Option<i32>> = OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var("VICTORY_PIPELINE_BOOST").ok().as_deref().map(str::trim) {
        Some("0") => None,
        Some("normal") => Some(THREAD_PRIORITY_NORMAL.0),
        _ => Some(THREAD_PRIORITY_BELOW_NORMAL.0),
    })
}

/// The most workers that take known-slow pipelines now: the usual limit, or
/// while [`BOOST`] all but one (that one is left for what draws ask for).
fn slow_limit_now() -> usize {
    let usual = slow_limit();
    let hurry = LOADING.load(Ordering::Relaxed) || (BOOST.load(Ordering::Relaxed) && boost_level().is_some());
    if usual == usize::MAX || !hurry {
        return usual;
    }
    usual.max(WORKERS.load(Ordering::Relaxed).saturating_sub(1))
}

fn next(screens_only: bool) -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop(screens_only, slow_limit_now()) {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// How many workers make pipelines: `asked` (`VICTORY_PIPELINE_WORKERS`, 1 to
/// 64), else two thirds of the logical processors, 2 to 16. They run below the
/// title's own threads, and a cold cache queues hundreds at once.
fn worker_count(asked: Option<usize>, processors: usize) -> usize {
    worker_count_for(asked, processors, false)
}

fn worker_count_for(asked: Option<usize>, processors: usize, limited_ram: bool) -> usize {
    match asked {
        Some(n) => n.clamp(1, 64),
        None if limited_ram => (processors * 3 / 4).clamp(2, 8),
        None => (processors * 2 / 3).clamp(2, 16),
    }
}

/// Starts the workers ([`worker_count`]) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count_for(
            std::env::var("VICTORY_PIPELINE_WORKERS")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            std::thread::available_parallelism().map_or(2, |n| n.get()),
            crate::residency::limited_host_ram(),
        );
        WORKERS.store(workers, Ordering::Relaxed);
        tracing::info!(workers, "pipeline workers");
        let screens = if screens_first() { SCREEN_WORKERS } else { 0 };
        for i in 0..workers + screens {
            let screens_only = i >= workers;
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording),
                    // and the streaming one, which is below normal itself
                    // (`VICTORY_PIPELINE_LOWEST=0`: level with it, as before).
                    let lowest = !screens_only && std::env::var_os("VICTORY_PIPELINE_LOWEST").is_none_or(|v| v != "0");
                    use windows::Win32::System::Threading::*;
                    let usual = if lowest { THREAD_PRIORITY_LOWEST } else { THREAD_PRIORITY_BELOW_NORMAL };
                    let set = |priority: THREAD_PRIORITY| {
                        // SAFETY: a plain call on our own thread.
                        let _ = unsafe { SetThreadPriority(GetCurrentThread(), priority) };
                    };
                    set(usual);
                    let mut now = usual.0;
                    loop {
                        let p = next(screens_only);
                        // In a hurry (a world's worth of pipelines waiting, the
                        // title's thread waiting for them): above the usual
                        // lowest, until they are all made.
                        // While the title loads and pipelines of earlier runs are
                        // being made ahead ([`LOADING`]): all workers but the
                        // first at normal priority.
                        let wanted = match boost_level() {
                            _ if lowest && i != 0 && LOADING.load(Ordering::Relaxed) => THREAD_PRIORITY_NORMAL.0,
                            Some(level) if lowest && BOOST.load(Ordering::Relaxed) => level.max(usual.0),
                            _ => usual.0,
                        };
                        if wanted != now {
                            set(THREAD_PRIORITY(wanted));
                            now = wanted;
                        }
                        p.make();
                        if p.was_slow.load(Ordering::Relaxed) {
                            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                            queue.slow_now -= 1;
                            AVAILABLE.notify_all();
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save_while_busy();
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    BOOST.store(false, Ordering::Relaxed);
    loading_over("everything queued is made");
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
        super::library::log_summary(true);
    }
    super::library::save(false);
}

/// Whether the title is loading and pipelines of earlier runs are being made
/// ahead of its asking for them ([`prewarm_begin`]): the workers but the first
/// run at normal priority, all but one of them may be in known-slow pipelines,
/// and the queue goes in the order earlier runs first used pipelines in, slow
/// ones included. Cleared by [`loading_over`].
static LOADING: AtomicBool = AtomicBool::new(false);
static LOADING_SINCE: Mutex<Option<Instant>> = Mutex::new(None);

/// Pipelines planned from earlier runs, queued by the prewarm, made so far,
/// found by the title's creation of the same pipeline, and asked for by a draw
/// before they were made.
pub(super) static PLANNED: AtomicU64 = AtomicU64::new(0);
pub(super) static QUEUED: AtomicU64 = AtomicU64::new(0);
static PRE_MADE: AtomicU64 = AtomicU64::new(0);
static ADOPTED: AtomicU64 = AtomicU64::new(0);
static LATE: AtomicU64 = AtomicU64::new(0);

/// By key: the planned pipelines, `None` until one exists (made by the prewarm
/// or created by the title, whichever came first). The title's creation of a
/// pipeline gets the one that is here ([`adopt`]), made or in the queue.
static PREWARMED: Mutex<Option<HashMap<String, Option<Arc<Pending>>>>> = Mutex::new(None);

/// A frame in which this many draws and dispatches are asked for is a world's
/// (the menu and loading screen draw 1 to 11); two in a row end the loading.
const WORLD_DRAWS: u64 = 300;
static DRAW_FRAME: AtomicU64 = AtomicU64::new(0);
static DRAW_COUNT: AtomicU64 = AtomicU64::new(0);
static WORLD_FRAMES: AtomicU64 = AtomicU64::new(0);

/// The prewarm starts: counts as a pipeline waiting until [`prewarm_end`], so
/// that the workers do not run out of work between the loader's entries.
pub(super) fn prewarm_begin() {
    *LOADING_SINCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    WAITING.fetch_add(1, Ordering::Relaxed);
    LOADING.store(true, Ordering::Relaxed);
}

/// The loader has queued all it will.
pub(super) fn prewarm_end() {
    if QUEUED.load(Ordering::Relaxed) == 0 {
        loading_over("nothing to make ahead");
    }
    if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
        drained();
    }
}

/// The keys the prewarm will queue.
pub(super) fn prewarm_plan(keys: &[String]) {
    let mut map = PREWARMED.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_default();
    for key in keys {
        map.entry(key.clone()).or_insert(None);
    }
    PLANNED.store(keys.len() as u64, Ordering::Relaxed);
}

/// Queues a pipeline of an earlier run, before the title creates it; `false`
/// if the title created it first (its own is the one that is made).
pub(super) fn prewarm_add(p: Pending) -> bool {
    p.prewarmed.store(true, Ordering::Relaxed);
    let p = Arc::new(p);
    {
        let mut map = PREWARMED.lock().unwrap_or_else(|e| e.into_inner());
        let slot = map.get_or_insert_default().entry(p.key.clone()).or_insert(None);
        if slot.is_some() {
            return false;
        }
        *slot = Some(p.clone());
    }
    if p.left_out.is_none() && !p.shaders.is_empty() {
        QUEUED.fetch_add(1, Ordering::Relaxed);
        start(&p, EARLY);
    }
    true
}

/// The pipeline the title's creation of `p` gets: the one the prewarm made or
/// queued for the same key, else `p` itself.
pub(crate) fn adopt(p: Pending) -> Arc<Pending> {
    let mut map = PREWARMED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = map.as_mut().and_then(|m| m.get_mut(&p.key)) {
        if let Some(there) = slot.as_ref() {
            ADOPTED.fetch_add(1, Ordering::Relaxed);
            return there.clone();
        }
        let p = Arc::new(p);
        *slot = Some(p.clone());
        return p;
    }
    Arc::new(p)
}

/// Counts a draw or dispatch of the title while it loads: a frame of a
/// world's worth of them ends the loading.
fn note_draw() {
    if !LOADING.load(Ordering::Relaxed) {
        return;
    }
    let frame = crate::frame::current();
    let last = DRAW_FRAME.load(Ordering::Relaxed);
    if frame != last && DRAW_FRAME.compare_exchange(last, frame, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        if DRAW_COUNT.swap(0, Ordering::Relaxed) >= WORLD_DRAWS {
            if WORLD_FRAMES.fetch_add(1, Ordering::Relaxed) >= 1 {
                loading_over("the title draws a world");
            }
        } else {
            WORLD_FRAMES.store(0, Ordering::Relaxed);
        }
    }
    DRAW_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// The title has loaded (or everything planned is made): the workers go back
/// to their usual priority and limits.
fn loading_over(why: &str) {
    if !LOADING.swap(false, Ordering::Relaxed) {
        return;
    }
    let seconds = LOADING_SINCE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map_or(0.0, |t| t.elapsed().as_secs_f64());
    tracing::info!(
        why,
        seconds = (seconds * 10.0).round() / 10.0,
        planned = PLANNED.load(Ordering::Relaxed),
        queued = QUEUED.load(Ordering::Relaxed),
        made = PRE_MADE.load(Ordering::Relaxed),
        found_by_the_title = ADOPTED.load(Ordering::Relaxed),
        asked_before_made = LATE.load(Ordering::Relaxed),
        waiting = waiting(),
        "pipeline prewarm: loading over, workers back to their usual priority"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_count_follows_the_processors_and_the_switch() {
        assert_eq!(worker_count(None, 1), 2);
        assert_eq!(worker_count(None, 24), 16);
        assert_eq!(worker_count(None, 64), 16);
        assert_eq!(worker_count(Some(0), 24), 1);
        assert_eq!(worker_count(Some(20), 24), 20);
        assert_eq!(worker_count_for(None, 8, true), 6);
        assert_eq!(worker_count_for(None, 16, true), 8);
        assert_eq!(worker_count_for(None, 24, true), 8);
        assert_eq!(worker_count_for(Some(12), 8, true), 12);
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    /// A pipeline left out on purpose (a ray-tracing resolve without the structure builder)
    /// is never queued, so never made: it is counted as left out, and named so, not as
    /// "being made" (some 630 indirect dispatches a frame were, in every gameplay frame).
    #[test]
    fn pipelines_left_out_are_counted_and_never_queued() {
        let mut p = Pending::new(Vec::new(), None);
        p.left_out = Some("shades ray-tracing hits without BVH, left out");
        let p = Arc::new(p);
        let before = LEFT_OUT.load(Ordering::Relaxed);
        assert!(ready(&p).is_none());
        assert!(ready(&p).is_none());
        assert!(!now(&p));
        assert_eq!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        assert!(p.made.get().is_none());
        assert!(LEFT_OUT.load(Ordering::Relaxed) >= before + 2);
        #[cfg(feature = "diagnostics")]
        assert_eq!(
            missing(&p).as_deref(),
            Some("left out on purpose: shades ray-tracing hits without BVH, left out")
        );
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    /// The title's creation of a pipeline gets the one made ahead for its key,
    /// or its own if it came first (and then the prewarm leaves it alone).
    #[test]
    fn the_title_gets_the_pipeline_made_ahead() {
        let named = |key: &str| {
            let mut p = Pending::new(Vec::new(), None);
            p.key = key.to_owned();
            p
        };
        prewarm_plan(&["ahead-test".to_owned(), "title-first-test".to_owned()]);
        assert!(prewarm_add(named("ahead-test")));
        let got = adopt(named("ahead-test"));
        assert!(got.prewarmed.load(Ordering::Relaxed));
        assert!(Arc::ptr_eq(&got, &adopt(named("ahead-test"))));

        let own = adopt(named("title-first-test"));
        assert!(!own.prewarmed.load(Ordering::Relaxed));
        assert!(!prewarm_add(named("title-first-test")));
        assert!(Arc::ptr_eq(&own, &adopt(named("title-first-test"))));

        // Not planned: not kept.
        let (a, b) = (adopt(named("unplanned-test")), adopt(named("unplanned-test")));
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &earlier));
        assert!(queue.pop(false, 1).is_none());
    }

    #[test]
    fn pipelines_known_to_take_long_leave_workers_for_the_others() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (long, longer, asked, quick) = (new(), new(), new(), new());
        let mut queue = Queue::new();
        for p in [&long, &longer, &asked] {
            assert_eq!(queue.push_as(p, EARLY, true), Some(true));
        }
        assert_eq!(queue.push_as(&quick, EARLY, false), Some(true));
        // The quick one first, then one long one at a time; one a draw asks
        // for waits for nobody.
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &quick));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &long));
        assert!(queue.pop(false, 1).is_none());
        assert_eq!(queue.push_as(&asked, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &asked));
        assert!(!asked.was_slow.load(Ordering::Relaxed));
        assert!(queue.pop(false, 1).is_none());
        queue.slow_now -= 1;
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &longer));
        assert!(queue.pop(false, 1).is_none());
    }

    /// Early pipelines come out by the place earlier runs first used them in,
    /// those with none after them, and equal places in the order queued.
    #[test]
    fn early_pipelines_go_in_first_use_order() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let mut heap = BinaryHeap::new();
        let ps = [new(), new(), new(), new()];
        for (n, (rank, p)) in [(u64::MAX, &ps[0]), (5, &ps[1]), (1, &ps[2]), (u64::MAX, &ps[3])].into_iter().enumerate() {
            heap.push(Ranked { rank, n: n as u64, p: p.clone() });
        }
        let order: Vec<_> = std::iter::from_fn(|| heap.pop()).map(|r| r.p).collect();
        for (got, want) in order.iter().zip([&ps[2], &ps[1], &ps[0], &ps[3]]) {
            assert!(Arc::ptr_eq(got, want));
        }
    }

    /// Passes over the screen go before all others, and workers of their own
    /// take nothing else.
    #[test]
    fn passes_over_the_screen_go_first() {
        let of = |screen| {
            let mut p = Pending::new(Vec::new(), None);
            p.screen = screen;
            Arc::new(p)
        };
        let (world, screen) = (of(false), of(true));
        let mut queue = Queue::new();
        assert_eq!(queue.push(&world, REQUESTED), Some(true));
        assert_eq!(queue.push(&screen, REQUESTED), Some(true));
        assert!(Arc::ptr_eq(&queue.pop(true, 1).unwrap(), &screen));
        assert!(queue.pop(true, 1).is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &world));
    }
}
