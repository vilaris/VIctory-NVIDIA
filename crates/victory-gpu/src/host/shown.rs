//! The swap chain's Present on a thread of its own (`VICTORY_PRESENT_THREAD`, on unless `=0`).
//!
//! With DLSS Frame Generation the Present call blocks while frame generation paces the frames
//! it shows: 12.4 ms a frame on average (16.7 the longest) in the release run of 2026-11-27
//! 15:29, on the thread that prepares and sends submissions (`crate::queue::behind`), which was
//! the frame's wall (42 ms of a 44 ms frame). Here the submitting thread records and sends the
//! present's copy, sets the frame's markers and tags, and hands the Present call to this
//! thread; it goes on preparing the next frame's submissions meanwhile.
//!
//! The host queue's order is kept: from the hand-over until the Present call has returned,
//! whatever any thread gives the host queue through [`Host::queue_send`] (the title's
//! submissions, our copies and fetches, their fence signals) is held, and sent in the order
//! given as soon as it returns. So the frame generation work the Present puts on the queue,
//! which reads the frame's tagged depth and motion vectors, still comes before the next
//! frame's lists. The next present waits until this one is over (the back buffer's index and
//! the frame's markers go in order).
//!
//! Bounded hold (`VICTORY_PRESENT_HOLD_MS`, 250 by default, 0: no bound): a Present that has
//! not returned that long after the hand-over may be waiting for the very work held behind it
//! (frame pacing or the latency markers waiting for the card to reach a signal that is held:
//! the queue, the Present and every thread waiting on those signals would stand for good, the
//! title's watchdog or the driver's reset ending the run). Past the bound, what was held is
//! sent at once, in order, and nothing more is held for that Present. The order against frame
//! generation's own work for that one frame may then differ; a warning says so.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandList, ID3D12CommandQueue, ID3D12Fence};
use windows::Win32::Graphics::Dxgi::{DXGI_PRESENT, IDXGISwapChain3};

use super::Host;
use super::render_trace;
use windows::core::Interface;

/// What was given to the host queue while a Present was pending: lists in calls cut at
/// `cuts`, then a fence signal.
struct Held {
    lists: Vec<Option<ID3D12CommandList>>,
    cuts: Vec<usize>,
    signal: Option<(ID3D12Fence, u64)>,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Held {}

struct Gate {
    /// A Present was handed over and has not returned (or what it held is being sent).
    pending: bool,
    held: Vec<Held>,
    /// When the pending Present was handed over (`super::now_us`).
    since_us: u64,
    /// The Present has returned: what it held is being sent by the presenting thread.
    returned: bool,
}

static GATE: Mutex<Gate> = Mutex::new(Gate { pending: false, held: Vec::new(), since_us: 0, returned: false });

/// [`Gate`]'s bound on holding, in microseconds (0: none).
fn hold_limit_us() -> u64 {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_PRESENT_HOLD_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(250) * 1000
    })
}

/// Presents past [`hold_limit_us`] since the last frame-rate line.
static OVERRAN: AtomicU64 = AtomicU64::new(0);

/// Whether the presenting thread is doing a present (set by the submitting thread when it
/// hands one over, cleared when everything of it is done).
static BUSY: Mutex<bool> = Mutex::new(false);
static IDLE: Condvar = Condvar::new();

/// Since the last frame-rate line: presents handed over, microseconds in the Present call,
/// sends held behind it, microseconds sending them, microseconds the submitting thread waited
/// for the one before at a present, presents done where they were (no thread).
static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
const HANDED: usize = 0;
const IN_PRESENT: usize = 1;
const HELD: usize = 2;
const RELEASING: usize = 3;
const WAITED: usize = 4;
const INLINE: usize = 5;

/// Whether the Present goes to a thread of its own.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRESENT_THREAD").is_none_or(|v| v != "0"))
}

/// Calls `lists` on `queue` in calls cut at `cuts`, then the signal.
///
/// # Safety
/// Closed lists; a fence the caller may signal.
unsafe fn run_on(
    queue: &ID3D12CommandQueue,
    lists: &[Option<ID3D12CommandList>],
    cuts: &[usize],
    signal: Option<(&ID3D12Fence, u64)>,
) -> windows::core::Result<()> {
    let mut from = 0;
    for to in cuts.iter().copied().chain([lists.len()]) {
        let to = to.clamp(from, lists.len());
        if to > from {
            if render_trace::active() {
                render_trace::execute(queue, &lists[from..to], signal.map(|(fence, value)| (fence.as_raw() as usize, value)));
            }
            // SAFETY: the caller's.
            crate::rf_x2::submitted(&lists[from..to]); // rf-x2
            unsafe { queue.ExecuteCommandLists(&lists[from..to]) };
        }
        from = to;
    }
    match signal {
        // SAFETY: the caller's.
        Some((fence, value)) => unsafe { queue.Signal(fence, value) },
        None => Ok(()),
    }
}

impl Host {
    /// Gives the host queue `lists` (in calls of their own from each of `cuts` on) and then
    /// the signal: now, or, while a Present handed to the presenting thread is pending, as
    /// soon as it has returned, in the order given. Every user of the host queue whose work
    /// has to stay in order with the title's submissions goes through here.
    pub(super) fn queue_send(
        &self,
        lists: &[Option<ID3D12CommandList>],
        cuts: &[usize],
        signal: Option<(&ID3D12Fence, u64)>,
    ) -> windows::core::Result<()> {
        let mut gate = GATE.lock().unwrap_or_else(|e| e.into_inner());
        let limit = hold_limit_us();
        if gate.pending && !gate.returned && limit != 0 && super::now_us().saturating_sub(gate.since_us) > limit {
            // The Present stands past the bound: what it held goes now, in order, and this
            // with it; nothing more is held for it.
            let held = std::mem::take(&mut gate.held);
            gate.pending = false;
            OVERRAN.fetch_add(1, Relaxed);
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, Relaxed) {
                tracing::warn!(
                    held = held.len(),
                    ms = super::now_us().saturating_sub(gate.since_us) / 1000,
                    "a Present on the presenting thread stood past VICTORY_PRESENT_HOLD_MS: the host queue's work held behind it was sent without waiting for it (it may have waited for that work)"
                );
            }
            for h in held {
                // SAFETY: lists closed and a fence given by earlier callers.
                unsafe { run_on(&self.queue, &h.lists, &h.cuts, h.signal.as_ref().map(|(f, v)| (f, *v)))? };
            }
            // SAFETY: the caller's lists and fence.
            return unsafe { run_on(&self.queue, lists, cuts, signal) };
        }
        if gate.pending {
            if render_trace::active() {
                render_trace::event("queue_held_for_present", format_args!("lists={}\tfence={:?}", lists.len(), signal.map(|(_, value)| value)));
            }
            gate.held.push(Held { lists: lists.to_vec(), cuts: cuts.to_vec(), signal: signal.map(|(f, v)| (f.clone(), v)) });
            STATS[HELD].fetch_add(1, Relaxed);
            return Ok(());
        }
        // (Under the gate's lock: a hand-over cannot come between the look and the call.)
        // SAFETY: the caller's lists and fence.
        unsafe { run_on(&self.queue, lists, cuts, signal) }
    }

    /// Before a present touches the swap chain: the one before is over.
    pub(super) fn present_wait(&self) {
        let began = std::time::Instant::now();
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        if !*busy {
            return;
        }
        let _waiting = crate::timeline::span("a present waits for the one before on the presenting thread");
        while *busy {
            busy = IDLE.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        STATS[WAITED].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

/// A Present handed over: the swap chain, what frame generation began for it.
pub(super) struct Job {
    pub(super) swap_chain: IDXGISwapChain3,
    pub(super) presenting: Option<crate::streamline::Presenting>,
    pub(super) frame: u64,
}

// SAFETY: DXGI and Streamline's frame token are used by one thread at a time: the submitting
// thread hands them over and touches the swap chain again only after `present_wait`.
unsafe impl Send for Job {}

/// Hands the Present of `job` to the presenting thread; with no thread, does it here.
pub(super) fn hand_over(job: Job) {
    static SENDER: OnceLock<Option<Mutex<std::sync::mpsc::Sender<Job>>>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new().name("victory-present".into()).spawn(move || {
            use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
            // SAFETY: this thread's own priority.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
            crate::cores::keep_on_fast_cores("victory-present");
            for job in receiver {
                let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| present(job)));
                if ended.is_err() {
                    // What it held would never be sent, and the next present would wait for good.
                    super::fail(format_args!("the thread that presents panicked"));
                }
            }
        });
        match spawned {
            Ok(_) => {
                tracing::info!("the swap chain's Present runs on a thread of its own (VICTORY_PRESENT_THREAD); the host queue's work waits behind it in order");
                Some(Mutex::new(sender))
            }
            Err(e) => {
                tracing::warn!(error = %e, "no presenting thread: frames are presented where they are sent");
                None
            }
        }
    });
    *BUSY.lock().unwrap_or_else(|e| e.into_inner()) = true;
    {
        let mut gate = GATE.lock().unwrap_or_else(|e| e.into_inner());
        gate.pending = true;
        gate.returned = false;
        gate.since_us = super::now_us();
    }
    STATS[HANDED].fetch_add(1, Relaxed);
    let job = match sender {
        Some(sender) => match sender.lock().unwrap_or_else(|e| e.into_inner()).send(job) {
            Ok(()) => return,
            Err(std::sync::mpsc::SendError(job)) => job,
        },
        None => job,
    };
    STATS[INLINE].fetch_add(1, Relaxed);
    present(job);
}

/// The Present, what was held behind it, frame generation's end of the frame and the meter.
fn present(job: Job) {
    let Job { swap_chain, presenting, frame } = job;
    let mut showing = crate::timeline::span("the host's Present (presenting thread)");
    let called = crate::streamline::present_called(&presenting);
    let began = std::time::Instant::now();
    let trace_start = render_trace::active().then(super::now_us);
    // SAFETY: our swap chain; nothing else uses it until `present_wait` returns.
    let result = unsafe { swap_chain.Present(0, DXGI_PRESENT(0)) };
    if let Some(start) = trace_start {
        render_trace::event("present", format_args!("present_frame={frame}\tstart_us={start}\tduration_us={}\tstatus={:#x}\tasync=true", began.elapsed().as_micros(), result.0));
    }
    STATS[IN_PRESENT].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    crate::streamline::present_returned(called);
    showing.next("sending what waited behind the present");
    release();
    showing.next("the frame's end");
    crate::streamline::present_ends(presenting);
    if let Some(host) = super::host() {
        let mut presenter = host.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = presenter.as_mut() {
            p.panel.frame(p.meter.line_due());
            p.menu.frame();
            p.meter.frame(p.hwnd);
        }
    }
    showing.end();
    *BUSY.lock().unwrap_or_else(|e| e.into_inner()) = false;
    IDLE.notify_all();
}

/// Sends what was held behind the present, in order, until nothing more comes; then the
/// queue is free again.
fn release() {
    let began = std::time::Instant::now();
    GATE.lock().unwrap_or_else(|e| e.into_inner()).returned = true;
    let Some(host) = super::host() else {
        let mut gate = GATE.lock().unwrap_or_else(|e| e.into_inner());
        gate.held.clear();
        gate.pending = false;
        return;
    };
    loop {
        let held = {
            let mut gate = GATE.lock().unwrap_or_else(|e| e.into_inner());
            if gate.held.is_empty() {
                gate.pending = false;
                break;
            }
            std::mem::take(&mut gate.held)
        };
        for h in held {
            // SAFETY: lists closed and a fence given by `queue_send`'s callers.
            let sent = unsafe { run_on(&host.queue, &h.lists, &h.cuts, h.signal.as_ref().map(|(f, v)| (f, *v))) };
            if let Err(e) = sent {
                super::fail(format_args!("work held behind a present could not be sent to the host queue ({e})"));
            }
        }
    }
    STATS[RELEASING].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
}

/// For the frame-rate line.
pub(super) fn line(frames: f64) {
    let s: [f64; 6] = std::array::from_fn(|i| STATS[i].swap(0, Relaxed) as f64);
    if s[HANDED] + s[INLINE] == 0.0 {
        return;
    }
    tracing::info!(
        "presents on a thread of their own, per frame (VICTORY_PRESENT_THREAD): {:.2} handed over ({:.2} done where they were), {:.1} ms in the Present call, {:.1} sends held behind it and {:.2} ms sending them; the submitting thread waited {:.2} ms at a present for the one before; {} presents stood past the hold bound (VICTORY_PRESENT_HOLD_MS)",
        s[HANDED] / frames,
        s[INLINE] / frames,
        s[IN_PRESENT] / 1000.0 / frames,
        s[HELD] / frames,
        s[RELEASING] / 1000.0 / frames,
        s[WAITED] / 1000.0 / frames,
        OVERRAN.swap(0, Relaxed)
    );
}
