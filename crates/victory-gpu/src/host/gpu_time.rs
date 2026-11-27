//! Always-on GPU timing of what runs on the host queue, in every build: when
//! each list of ours ran on the card, and how long each pass in it took. The
//! "card's clock" line counts the time something of ours sat in the queue,
//! which is the card working *or* the card standing at a wait; these
//! timestamps are the card's own clock, so the two can be told apart.
//!
//! - **Lists** (`VICTORY_GPU_SUBMITS=0` turns this off): a timestamp at the
//!   start and at the end of every host list and of our own lists on the host
//!   queue (the sends and hashes before submissions, the compares after them,
//!   the fetches, the state preludes). Per frame: the card's real busy time
//!   (the union of the lists' spans), the time between them (the card idle:
//!   waiting for us to send, or for the title to submit), and our own lists'
//!   share.
//! - **Passes** (`VICTORY_GPU_PASSES=0` turns this off): inside a list, a
//!   timestamp where a pass begins: draws until the render targets change,
//!   each compute pipeline's run of dispatches, our own steps in the list
//!   (indirect argument clamps, depth twins). A pass is named by the first
//!   pipeline it runs (the names the runtime logs, with the shaders' hashes).
//!   At most [`PER_LIST`] passes a list and [`PER_FRAME`] a frame.
//!
//! - **Steps** (with the passes): what a list records before its first draw
//!   or dispatch (the title's barriers, waits for shader writes, copies,
//!   resolves, clears), each run of one kind a pass of its own, named by the
//!   kind; and per frame, for every recorded step of the kind, how many and
//!   how many bytes ([`Step`]). These were one pass, "copies, barriers and
//!   steps before their first draw", the card's largest.
//!
//! Also here, the one change to what the card runs: a wait for every shader
//! write (a UAV barrier on no resource) that follows another with no work
//! recorded between them is left out (`VICTORY_MERGE_BARRIERS=0` keeps them).
//!
//! How: every list takes a block of [`BLOCK`] slots of one timestamp heap,
//! writes `EndQuery` at its marks and resolves its block once when it closes
//! into a read-back buffer that stays mapped. Nothing waits: the marks are
//! read when the list's last one has landed (a non-zero value: each block is
//! zeroed when taken), on the frame-rate line or every so many lists, and a
//! list that has not run after a few seconds is let go.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::*;

/// Slots in a block. A normal list uses one block, a traced list takes more
/// only when it fills its current block.
const BLOCK: u32 = 64;
/// Passes a list is timed at, at most.
const PER_LIST: u32 = BLOCK - 2;
/// Blocks in the heap: some 16,000 lists, a few seconds of frames, longer
/// than [`GIVE_UP_MS`], so that a block is not taken again while it is read.
const BLOCKS: u32 = 16_384;
const RING: u32 = BLOCK * BLOCKS;
/// Passes timed a frame, at most (lists' starts and ends are not counted).
const PER_FRAME: u32 = 2048;
/// Steps timed a frame, at most (a budget of their own).
const STEPS_PER_FRAME: u32 = 3072;
/// A list whose marks have not landed after this long is let go (left open,
/// dropped, or on a queue that never ran it).
const GIVE_UP_MS: u64 = 1_500;

/// What a pass is.
pub(crate) const DRAW: u8 = 0;
pub(crate) const COMPUTE: u8 = 1;
pub(crate) const STEPS: u8 = 2;
pub(crate) const OURS: u8 = 3;
const KINDS: [&str; 4] = ["draw", "compute", "copies/steps", "ours"];

fn submits_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_SUBMITS").is_none_or(|v| v != "0"))
}

/// Waits for every shader write left out where nothing was recorded since the
/// one before (`VICTORY_MERGE_BARRIERS`, on unless 0).
fn merge_barriers_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MERGE_BARRIERS").is_none_or(|v| v != "0"))
}

/// What a list records that is not a draw or a dispatch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    /// The title's barriers with state changes (transitions, aliasing).
    Transitions,
    /// Waits for every shader write: the title's UAV barriers, syncs, barrier
    /// groups, and its buffer barriers (buffers are host copies).
    ShaderWrites,
    BufferCopies,
    TextureCopies,
    Resolves,
    Clears,
    MemoryCopies,
    /// Moves of title textures to the states the host's copies need (inside
    /// the title's copies and clears: counted, not timed apart).
    OurStates,
    /// Texture uploads recorded into a list (counted, not timed apart).
    Uploads,
}

const STEP_NAMES: [&str; 9] = [
    "steps: the title's barriers with state changes",
    "steps: waits for every shader write (UAV barriers, syncs, buffer barriers)",
    "steps: the title's buffer copies",
    "steps: the title's texture copies",
    "steps: the title's resolves",
    "steps: the title's clears",
    "steps: the title's memory copies",
    "steps: our moves of title textures' states",
    "steps: texture uploads into lists",
];

/// Per step kind since the last frame-rate line: recorded, bytes.
static STEP_COUNTS: [[AtomicU64; 2]; 9] = [const { [const { AtomicU64::new(0) }; 2] }; 9];
/// Barriers recorded by the title's barrier calls: transitions, aliasing,
/// UAV barriers on a resource, waits for every shader write recorded, those
/// left out as repeated.
static BARRIERS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Our sends of title bytes: copy runs as made, as recorded (joined).
static SENT_RUNS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A step of `kind` recorded, `bytes` long (0: not known).
pub(crate) fn count_step(kind: Step, bytes: u64) {
    let k = &STEP_COUNTS[kind as usize];
    k[0].fetch_add(1, Ordering::Relaxed);
    k[1].fetch_add(bytes, Ordering::Relaxed);
}

/// The title's barrier call came to these host barriers.
pub(crate) fn count_barriers(transitions: u64, aliasing: u64, uav: u64) {
    BARRIERS[0].fetch_add(transitions, Ordering::Relaxed);
    BARRIERS[1].fetch_add(aliasing, Ordering::Relaxed);
    BARRIERS[2].fetch_add(uav, Ordering::Relaxed);
}

/// Our sends' copy runs: as made, as recorded (joined).
pub(crate) fn count_sent_runs(made: u64, recorded: u64) {
    SENT_RUNS[0].fetch_add(made, Ordering::Relaxed);
    SENT_RUNS[1].fetch_add(recorded, Ordering::Relaxed);
}

/// Each step kind's pass name, interned once.
fn step_names() -> &'static [u32; 9] {
    static NAMES: OnceLock<[u32; 9]> = OnceLock::new();
    NAMES.get_or_init(|| std::array::from_fn(|i| INTERNED.intern(STEP_NAMES[i])))
}

fn passes_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| submits_on() && std::env::var_os("VICTORY_GPU_PASSES").is_none_or(|v| v != "0"))
}

struct Ring {
    heap: ID3D12QueryHeap,
    back: ID3D12Resource,
    data: usize,
    frequency: u64,
}

// SAFETY: free-threaded D3D12 objects and a mapping each list writes its own
// block of.
unsafe impl Send for Ring {}
// SAFETY: as above.
unsafe impl Sync for Ring {}

fn ring() -> Option<&'static Ring> {
    static RING_: OnceLock<Option<Ring>> = OnceLock::new();
    RING_
        .get_or_init(|| {
            if !submits_on() {
                return None;
            }
            let host = host()?;
            let mut heap: Option<ID3D12QueryHeap> = None;
            // SAFETY: a timestamp heap of the host device; the host queue's
            // timestamp frequency.
            let frequency = unsafe {
                host.device
                    .CreateQueryHeap(
                        &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: RING, NodeMask: 0 },
                        &mut heap,
                    )
                    .ok()?;
                host.queue.GetTimestampFrequency().ok()?
            };
            if frequency == 0 {
                return None;
            }
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: u64::from(RING) * 8,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut back: Option<ID3D12Resource> = None;
            // SAFETY: a read-back buffer in the state ResolveQueryData needs.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut back,
                    )
                    .ok()?;
            }
            let back = back?;
            let mut data = std::ptr::null_mut();
            // SAFETY: kept mapped for the process, with the buffer.
            unsafe { back.Map(0, None, Some(&mut data)).ok()? };
            if data.is_null() {
                return None;
            }
            tracing::info!(
                lists = BLOCKS,
                per_list = PER_LIST,
                per_frame = PER_FRAME,
                passes = passes_on(),
                "GPU timing of lists and passes on (VICTORY_GPU_SUBMITS=0 / VICTORY_GPU_PASSES=0 turn it off)"
            );
            Some(Ring { heap: heap?, back, data: data as usize, frequency })
        })
        .as_ref()
}

/// The names of passes, by number.
struct Interned(Mutex<Option<(Vec<String>, HashMap<String, u32>)>>);

impl Interned {
    fn with_names<T>(&self, f: impl FnOnce(&[String]) -> T) -> T {
        let names = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(names.as_ref().map_or(&[][..], |(all, _)| all.as_slice()))
    }

    fn intern(&self, name: &str) -> u32 {
        let mut names = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let (all, by) = names.get_or_insert_with(Default::default);
        if let Some(&n) = by.get(name) {
            return n;
        }
        let n = all.len() as u32;
        all.push(name.to_owned());
        by.insert(name.to_owned(), n);
        n
    }
}

static INTERNED: Interned = Interned(Mutex::new(None));

/// What a mark begins: the list (with its first pass), a pass, or the end.
#[derive(Clone, Copy)]
enum What {
    Start { name: u32, kind: u8, ours: bool },
    Pass { name: u32, kind: u8 },
    End,
}

/// Per block, the number of the list that took it last (its `taken` count plus one): a list
/// left open past a turn of the ring (lists made ahead and left open) finds its block taken
/// by another and is neither resolved nor read, so two lists' values are never mixed.
static OWNER: [AtomicU64; BLOCKS as usize] = [const { AtomicU64::new(0) }; BLOCKS as usize];

/// A list being recorded: its block and its marks so far.
struct Rec {
    base: u32,
    /// Its number in [`OWNER`].
    taken: u64,
    marks: Vec<(u32, What)>,
    extra: Vec<(u32, u64)>,
    trace: Option<Box<Trace>>,
}

struct Trace {
    list: usize,
    commands: Vec<TraceCommand>,
}

struct TraceCommand {
    index: usize,
    cpu_us: u64,
    thread: u32,
    frame: u64,
    command: render_trace::Command,
}

fn block(ring: &Ring) -> (u32, u64) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let block = (n % u64::from(BLOCKS)) as u32;
    OWNER[block as usize].store(n + 1, Ordering::Release);
    let base = block * BLOCK;
    // SAFETY: the block lies within the ring mapping.
    unsafe { std::ptr::write_bytes((ring.data as *mut u64).add(base as usize), 0, BLOCK as usize) };
    (base, n + 1)
}

fn owns(base: u32, taken: u64, extra: &[(u32, u64)]) -> bool {
    let owned = |base, taken| OWNER[(base / BLOCK) as usize].load(Ordering::Acquire) == taken;
    owned(base, taken) && extra.iter().all(|&(base, taken)| owned(base, taken))
}

impl Rec {
    /// A new block, zeroed, and the list's start.
    fn begin(list: &ID3D12GraphicsCommandList, name: u32, kind: u8, ours: bool) -> Option<Rec> {
        let ring = ring()?;
        let (base, taken) = block(ring);
        let mut rec = Rec { base, taken, marks: Vec::with_capacity(8), extra: Vec::new(), trace: None };
        let what = What::Start { name, kind, ours };
        if render_trace::active() {
            rec.trace_mark(list, what, render_trace::Command("list_start", [0; 5]));
        } else {
            rec.mark(list, what);
        }
        Some(rec)
    }

    fn trace_mark(&mut self, list: &ID3D12GraphicsCommandList, what: What, command: render_trace::Command) {
        let index = self.marks.len();
        let trace = self.trace.get_or_insert_with(|| {
            let identity = list.cast::<ID3D12CommandList>().map_or_else(|_| list.as_raw() as usize, |base| base.as_raw() as usize);
            Box::new(Trace { list: identity, commands: Vec::new() })
        });
        trace.commands.push(TraceCommand { index, cpu_us: now_us(), thread: thread_id(), frame: crate::frame::current(), command });
        self.mark(list, what);
    }

    fn mark(&mut self, list: &ID3D12GraphicsCommandList, what: What) -> bool {
        let Some(ring) = ring() else { return false };
        let used = self.marks.len() as u32;
        // (the last slot is the end's)
        let room = if matches!(what, What::End) { BLOCK } else { BLOCK - 1 };
        if used >= room && self.trace.is_none() {
            return false;
        }
        let chunk = (used / BLOCK) as usize;
        let base = if chunk == 0 {
            self.base
        } else {
            if self.extra.len() < chunk {
                self.extra.push(block(ring));
            }
            self.extra[chunk - 1].0
        };
        let slot = base + used % BLOCK;
        // SAFETY: an open list; the slot is this list's.
        unsafe { list.EndQuery(&ring.heap, D3D12_QUERY_TYPE_TIMESTAMP, slot) };
        self.marks.push((slot, what));
        true
    }

    /// The end, the block resolved, and the marks handed over to be read.
    fn end(mut self, list: &ID3D12GraphicsCommandList) {
        let Some(ring) = ring() else { return };
        // (its block taken by another list meanwhile: not resolved, not read)
        if !owns(self.base, self.taken, &self.extra) {
            if let Some(trace) = &self.trace {
                render_trace::event("list_trace_lost", format_args!("record={}\tlist={:#x}\treason=reused_before_close", self.taken, trace.list));
            }
            if let Some(acc) = ACC.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                acc.lost += 1;
            }
            return;
        }
        if let Some(trace) = &self.trace {
            let identity = trace.list;
            self.trace_mark(list, What::End, render_trace::Command("list_close", [0; 5]));
            render_trace::event("list_recorded", format_args!("record={}\tlist={identity:#x}\tmarks={}", self.taken, self.marks.len()));
        } else if !self.mark(list, What::End) {
            return;
        }
        let count = self.marks.len() as u32;
        // SAFETY: an open list; these queries were ended in it; the buffer
        // holds the whole ring.
        for (chunk, base) in std::iter::once(self.base).chain(self.extra.iter().map(|&(base, _)| base)).enumerate() {
            let count = (count - chunk as u32 * BLOCK).min(BLOCK);
            unsafe { list.ResolveQueryData(&ring.heap, D3D12_QUERY_TYPE_TIMESTAMP, base, count, &ring.back, u64::from(base) * 8) };
        }
        let n = {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            pending.push(Pending { at: now_ms(), base: self.base, taken: self.taken, marks: self.marks, extra: self.extra, trace: self.trace });
            pending.len()
        };
        static ENDED: AtomicU64 = AtomicU64::new(0);
        if ENDED.fetch_add(1, Ordering::Relaxed) % 256 == 255 || n > 4096 {
            collect(false);
        }
    }
}

struct Pending {
    at: u64,
    /// Its block and its number in [`OWNER`].
    base: u32,
    taken: u64,
    marks: Vec<(u32, What)>,
    extra: Vec<(u32, u64)>,
    trace: Option<Box<Trace>>,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

/// What the lists that ran since the last frame-rate line came to.
#[derive(Default)]
struct Acc {
    /// Lists' spans on the card (ticks).
    spans: Vec<(u64, u64)>,
    /// Our own lists' ticks.
    ours: u64,
    lists: u64,
    ours_lists: u64,
    /// Per pass name: kind, ticks, times.
    passes: HashMap<u32, (u8, u64, u64)>,
    /// Lists let go unread.
    lost: u64,
}

static ACC: Mutex<Option<Acc>> = Mutex::new(None);

/// Reads the lists whose marks have landed.
fn collect(wait: bool) {
    let Some(ring) = ring() else { return };
    let mut pending = if wait {
        PENDING.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match PENDING.try_lock() {
            Ok(p) => p,
            Err(_) => return,
        }
    };
    let now = now_ms();
    let data = ring.data as *const u64;
    // SAFETY: inside the mapping; the card writes whole 8-byte values.
    let read = |slot: u32| unsafe { data.add(slot as usize).read_volatile() };
    let mut landed = Vec::new();
    let mut lost = 0u64;
    pending.retain_mut(|p| {
        // (its block taken again before it was read: another list's values now)
        let reason = if !owns(p.base, p.taken, &p.extra) {
            "reused_before_read"
        } else if p.marks.last().is_some_and(|m| read(m.0) != 0) {
            let marks = p.marks.iter().map(|&(slot, what)| (read(slot), what)).collect::<Vec<_>>();
            if owns(p.base, p.taken, &p.extra) {
                landed.push((p.taken, marks, p.trace.take()));
                return false;
            }
            "reused_during_read"
        } else if now.saturating_sub(p.at) > GIVE_UP_MS {
            "unresolved"
        } else {
            return true;
        };
        lost += 1;
        if let Some(trace) = &p.trace {
            render_trace::event("list_trace_lost", format_args!("record={}\tlist={:#x}\treason={reason}", p.taken, trace.list));
        }
        false
    });
    drop(pending);
    if landed.is_empty() && lost == 0 {
        return;
    }
    for (record, marks, trace) in &landed {
        if let Some(trace) = trace {
            trace_landed(*record, marks, trace, ring.frequency);
        }
    }
    let mut acc = ACC.lock().unwrap_or_else(|e| e.into_inner());
    let acc = acc.get_or_insert_with(Acc::default);
    acc.lost += lost;
    for (_, marks, _) in landed {
        let (Some(first), Some(last)) = (marks.first(), marks.last()) else { continue };
        if first.0 == 0 || last.0 < first.0 {
            continue;
        }
        acc.spans.push((first.0, last.0));
        acc.lists += 1;
        if let What::Start { ours: true, .. } = first.1 {
            acc.ours += last.0 - first.0;
            acc.ours_lists += 1;
        }
        for pair in marks.windows(2) {
            let (at, what) = pair[0];
            let ticks = pair[1].0.saturating_sub(at);
            let (name, kind) = match what {
                What::Start { name, kind, .. } | What::Pass { name, kind } => (name, kind),
                What::End => continue,
            };
            let e = acc.passes.entry(name).or_insert((kind, 0, 0));
            e.1 += ticks;
            e.2 += 1;
        }
    }
}

fn trace_landed(record: u64, marks: &[(u64, What)], trace: &Trace, frequency: u64) {
    let mut batch = String::new();
    let list = trace.list;
    INTERNED.with_names(|names| {
        for &TraceCommand { index, cpu_us, thread, frame, command: render_trace::Command(op, args) } in &trace.commands {
            let (start, what) = marks[index];
            let end = marks.get(index + 1).map_or(start, |mark| mark.0);
            let name = match what {
                What::Start { name, .. } | What::Pass { name, .. } => &names[name as usize],
                What::End => "",
            };
            let valid = start != 0 && end >= start;
            let _ = writeln!(batch, "gpu_command\trecord={record}\tlist={list:#x}\tseq={index}\tcpu_us={cpu_us}\tthread={thread}\tframe={frame}\top={op}\targs={args:?}\tname={name}\tgpu_start={start}\tgpu_end={end}\tgpu_hz={frequency}\ttiming_valid={valid}");
        }
    });
    render_trace::write(batch);
}

/// The per-frame lines, on the frame-rate line.
pub(super) fn log(frames: f64) {
    let Some(ring) = ring() else { return };
    collect(true);
    let Some(acc) = ACC.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let frames = frames.max(1.0);
    let ms = |ticks: u64| ticks as f64 * 1e3 / ring.frequency as f64;
    let mut spans = acc.spans;
    spans.sort_unstable();
    // The union of the lists' spans: the card running ours.
    let (mut busy, mut idle, mut idle_gaps) = (0u64, 0u64, 0u64);
    let mut current: Option<(u64, u64)> = None;
    for (a, b) in spans.iter().copied() {
        match current {
            Some((s, e)) if a <= e => current = Some((s, e.max(b))),
            Some((s, e)) => {
                busy += e - s;
                idle += a - e;
                idle_gaps += 1;
                current = Some((a, b));
            }
            None => current = Some((a, b)),
        }
    }
    if let Some((s, e)) = current {
        busy += e - s;
    }
    let first_to_last = match (spans.first(), spans.iter().map(|s| s.1).max()) {
        (Some(f), Some(l)) => l.saturating_sub(f.0),
        _ => 0,
    };
    tracing::info!(
        "GPU time per frame by the card's own clock (VICTORY_GPU_SUBMITS): {:.1} lists, the card running them {:.2} ms (the union of their spans), idle between them {:.2} ms in {:.1} gaps (waiting for us to send or for the title to submit), first to last {:.1} ms (the card running {:.0} % of that: per frame the sums run high when lists of the line before land late); of the running our own lists (sends and hashes, compares, fetches, preludes) {:.2} ms in {:.1}; {} lists not read (not run in {} ms)",
        acc.lists as f64 / frames,
        ms(busy) / frames,
        ms(idle) / frames,
        idle_gaps as f64 / frames,
        ms(first_to_last) / frames,
        if first_to_last > 0 { busy as f64 * 100.0 / first_to_last as f64 } else { 0.0 },
        ms(acc.ours) / frames,
        acc.ours_lists as f64 / frames,
        acc.lost,
        GIVE_UP_MS,
    );
    if acc.passes.is_empty() {
        return;
    }
    // By kind, then the costliest passes.
    let mut kinds = [(0u64, 0u64); 4];
    for &(kind, ticks, times) in acc.passes.values() {
        let k = &mut kinds[usize::from(kind.min(3))];
        k.0 += ticks;
        k.1 += times;
    }
    let names = INTERNED.with_names(|all| all.to_vec());
    log_steps(frames, &acc.passes, &ms);
    let mut passes: Vec<(u32, u8, u64, u64)> = acc.passes.iter().map(|(&n, &(k, t, c))| (n, k, t, c)).collect();
    passes.sort_unstable_by(|a, b| b.2.cmp(&a.2));
    let mut top = String::new();
    for (i, &(name, kind, ticks, times)) in passes.iter().take(25).enumerate() {
        use std::fmt::Write as _;
        let _ = write!(
            top,
            "{}{}. {} [{}] {:.2} ms ({:.1}x)",
            if i == 0 { "" } else { "; " },
            i + 1,
            names.get(name as usize).map_or("?", String::as_str),
            KINDS[usize::from(kind.min(3))],
            ms(ticks) / frames,
            times as f64 / frames,
        );
    }
    tracing::info!(
        "GPU passes per frame (VICTORY_GPU_PASSES; at most {PER_LIST} a list and {PER_FRAME} a frame, the rest counted in the pass before): draws {:.2} ms, compute {:.2} ms, copies and steps {:.2} ms, ours {:.2} ms; the costliest: {}",
        ms(kinds[DRAW as usize].0) / frames,
        ms(kinds[COMPUTE as usize].0) / frames,
        ms(kinds[STEPS as usize].0) / frames,
        ms(kinds[OURS as usize].0) / frames,
        top,
    );
}

/// The steps line: per kind, recorded and bytes a frame and the card's time
/// of their runs before the lists' first draws; the title's barriers; our
/// sends' copy runs.
fn log_steps(frames: f64, passes: &HashMap<u32, (u8, u64, u64)>, ms: &dyn Fn(u64) -> f64) {
    use std::fmt::Write as _;
    let ids = step_names();
    let mut kinds = String::new();
    for (i, name) in STEP_NAMES.iter().enumerate() {
        let n = STEP_COUNTS[i][0].swap(0, Ordering::Relaxed);
        let bytes = STEP_COUNTS[i][1].swap(0, Ordering::Relaxed);
        let (ticks, runs) = passes.get(&ids[i]).map_or((0, 0), |&(_, t, c)| (t, c));
        if n == 0 && runs == 0 {
            continue;
        }
        let _ = write!(
            kinds,
            "{}{}: {:.1} recorded, {:.2} MB, {:.2} ms on the card in {:.1} runs before first draws",
            if kinds.is_empty() { "" } else { "; " },
            name.trim_start_matches("steps: "),
            n as f64 / frames,
            bytes as f64 / 1e6 / frames,
            ms(ticks) / frames,
            runs as f64 / frames,
        );
    }
    let b: Vec<u64> = BARRIERS.iter().map(|b| b.swap(0, Ordering::Relaxed)).collect();
    let sent: Vec<u64> = SENT_RUNS.iter().map(|s| s.swap(0, Ordering::Relaxed)).collect();
    tracing::info!(
        "GPU steps per frame (VICTORY_GPU_PASSES): {kinds}; the title's barriers came to {:.1} transitions, {:.1} aliasing, {:.1} UAV barriers on a resource, {:.1} waits for every shader write recorded and {:.1} left out as repeated with nothing recorded since the one before (VICTORY_MERGE_BARRIERS={}); our sends of title bytes {:.1} copy runs made, {:.1} recorded after joining adjacent ones (VICTORY_BATCH_COPIES={})",
        b[0] as f64 / frames,
        b[1] as f64 / frames,
        b[2] as f64 / frames,
        b[3] as f64 / frames,
        b[4] as f64 / frames,
        u8::from(merge_barriers_on()),
        sent[0] as f64 / frames,
        sent[1] as f64 / frames,
        u8::from(super::send::batch_copies_on()),
    );
}

/// Whether one more step may be timed this frame.
fn step_room() -> bool {
    static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let now = crate::frame::current();
    if FRAME.swap(now, Ordering::Relaxed) != now {
        COUNT.store(0, Ordering::Relaxed);
    }
    COUNT.fetch_add(1, Ordering::Relaxed) < STEPS_PER_FRAME
}

/// Whether one more pass may be timed this frame.
fn frame_room() -> bool {
    static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let now = crate::frame::current();
    if FRAME.swap(now, Ordering::Relaxed) != now {
        COUNT.store(0, Ordering::Relaxed);
    }
    COUNT.fetch_add(1, Ordering::Relaxed) < PER_FRAME
}

/// A host list's timing: its record while it is recorded, and the pass it is in.
#[derive(Default)]
pub(super) struct List {
    state: Mutex<ListState>,
    /// Work recorded so far (draws, dispatches, copies, clears, held indirect
    /// executions, ...): only compared, for [`List::wait_for_writes`].
    work: AtomicU64,
    /// `work` + 1 when the last wait for every shader write was recorded
    /// (0: none in this list yet).
    waited_at: AtomicU64,
    /// Whether shader/copy UAV-producing work was recorded since the last
    /// list start or UAV wait.
    uav_dirty: AtomicBool,
}

#[derive(Default)]
struct ListState {
    rec: Option<Rec>,
    /// The pass it is in (0: none yet, or the render targets changed).
    key: u64,
    /// A draw or dispatch was recorded: steps are timed only before it.
    drawn: bool,
    /// The step kind of the run it is in, + 1 (0: none).
    step: usize,
}

impl List {
    /// The list begins to be recorded (made, or reset).
    pub(super) fn start(&self, list: &ID3D12GraphicsCommandList) {
        self.work.store(0, Ordering::Relaxed);
        self.waited_at.store(0, Ordering::Relaxed);
        // Start dirty because a prior list on the host queue may have left
        // unsynchronized UAV writes before this list's first barrier.
        self.uav_dirty.store(true, Ordering::Relaxed);
        if !submits_on() {
            return;
        }
        static FIRST: OnceLock<u32> = OnceLock::new();
        let name = *FIRST.get_or_init(|| {
            INTERNED.intern("lists: from their start to the first step, draw or dispatch (states, our own steps)")
        });
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.rec = Rec::begin(list, name, STEPS, false);
        s.key = 0;
        s.drawn = false;
        s.step = 0;
    }

    /// Work was recorded into the list.
    pub(super) fn work(&self) {
        self.work.fetch_add(1, Ordering::Relaxed);
    }

    /// Shader/copy UAV-producing work was recorded into the list.
    #[allow(dead_code)]
    pub(super) fn uav_work(&self) {
        self.uav_dirty.store(true, Ordering::Relaxed);
    }

    /// Clears the shader/copy UAV dirty flag without emitting a wait.
    #[allow(dead_code)]
    pub(super) fn clear_uav_dirty(&self) {
        self.uav_dirty.store(false, Ordering::Relaxed);
    }

    /// Whether shader/copy UAV-producing work was recorded since the last
    /// start or UAV wait.
    #[allow(dead_code)]
    pub(super) fn is_uav_dirty(&self) -> bool {
        self.uav_dirty.load(Ordering::Relaxed)
    }

    /// Alias for [`Self::is_uav_dirty`] for `host.rs` wiring.
    #[allow(dead_code)]
    pub(super) fn has_uav_work(&self) -> bool {
        self.is_uav_dirty()
    }

    /// Whether a wait for every shader write (a UAV barrier on no resource)
    /// is to be recorded now: not if the last one in this list had nothing
    /// recorded after it (`VICTORY_MERGE_BARRIERS=0`: always). Counted.
    pub(super) fn wait_for_writes(&self) -> bool {
        let now = self.work.load(Ordering::Relaxed) + 1;
        let was = self.waited_at.swap(now, Ordering::Relaxed);
        if was == now && merge_barriers_on() {
            BARRIERS[4].fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.uav_dirty.store(false, Ordering::Relaxed);
        BARRIERS[3].fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Like [`Self::wait_for_writes`], but gated on [`Self::uav_dirty`]
    /// instead of generic `work` (so non-UAV draws and `OMSetRenderTargets`
    /// do not force a global UAV barrier).
    #[allow(dead_code)]
    pub(super) fn wait_for_uav_writes(&self) -> bool {
        let was = self.uav_dirty.swap(false, Ordering::Relaxed);
        if !was && merge_barriers_on() {
            BARRIERS[4].fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.waited_at.store(self.work.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        BARRIERS[3].fetch_add(1, Ordering::Relaxed);
        true
    }

    /// A step of `kind` is recorded next: a pass of its own on the card if
    /// the list has no draw or dispatch yet and the run it is in is of
    /// another kind.
    pub(super) fn step(&self, list: &ID3D12GraphicsCommandList, kind: Step, bytes: u64) {
        if !passes_on() {
            return;
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if render_trace::active() && let Some(rec) = s.rec.as_mut() {
            rec.trace_mark(list, What::Pass { name: step_names()[kind as usize], kind: STEPS }, render_trace::Command("step", [bytes as i64, 0, 0, 0, 0]));
            s.step = kind as usize + 1;
            s.key = 0;
            return;
        }
        if s.drawn || s.step == kind as usize + 1 || s.rec.as_ref().is_none_or(|r| r.marks.len() as u32 >= PER_LIST) {
            return;
        }
        s.step = kind as usize + 1;
        // (the draw or dispatch after it begins a pass of its own)
        s.key = 0;
        if !step_room() {
            return;
        }
        let name = step_names()[kind as usize];
        if let Some(rec) = s.rec.as_mut() {
            rec.mark(list, What::Pass { name, kind: STEPS });
        }
    }

    /// Work of `kind` keyed `key` is recorded next: a new pass if the key is
    /// another than the pass's.
    pub(super) fn pass(&self, list: &ID3D12GraphicsCommandList, key: u64, kind: u8, command: render_trace::Command, name: impl FnOnce() -> String) {
        if !passes_on() {
            return;
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if kind == DRAW || kind == COMPUTE {
            s.drawn = true;
        }
        if render_trace::active() && let Some(rec) = s.rec.as_mut() {
            rec.trace_mark(list, What::Pass { name: INTERNED.intern(&name()), kind }, command);
            s.key = key;
            return;
        }
        if s.key == key || s.rec.as_ref().is_none_or(|r| r.marks.len() as u32 >= PER_LIST) {
            return;
        }
        s.key = key;
        if !frame_room() {
            return;
        }
        let name = INTERNED.intern(&name());
        if let Some(rec) = s.rec.as_mut() {
            rec.mark(list, What::Pass { name, kind });
        }
    }

    /// The render targets changed: the next draw begins a pass.
    pub(super) fn break_pass(&self) {
        if passes_on() {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).key = 0;
        }
    }

    /// The list is closed.
    pub(super) fn end(&self, list: &ID3D12GraphicsCommandList) {
        let rec = self.state.lock().unwrap_or_else(|e| e.into_inner()).rec.take();
        if let Some(rec) = rec {
            rec.end(list);
        }
    }
}

/// One of our own lists on the host queue: timed as a whole, named `name`.
pub(super) struct Ours(Option<Rec>);

impl Ours {
    pub(super) fn begin(list: &ID3D12GraphicsCommandList, name: &'static str) -> Ours {
        if !submits_on() {
            return Ours(None);
        }
        Ours(Rec::begin(list, INTERNED.intern(name), OURS, true))
    }

    /// A new pass of ours in the list.
    pub(super) fn pass(&mut self, list: &ID3D12GraphicsCommandList, name: &'static str) {
        if !passes_on() {
            return;
        }
        if let Some(rec) = self.0.as_mut() {
            if render_trace::active() {
                rec.trace_mark(list, What::Pass { name: INTERNED.intern(name), kind: OURS }, render_trace::Command("host_pass", [0; 5]));
            } else if frame_room() {
                rec.mark(list, What::Pass { name: INTERNED.intern(name), kind: OURS });
            }
        }
    }

    pub(super) fn end(self, list: &ID3D12GraphicsCommandList) {
        if let Some(rec) = self.0 {
            rec.end(list);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detailed_gpu_timing_resolves_more_than_one_query_block() {
        let Some(host) = host() else { return };
        let Some(ring) = ring() else { return };
        let list = host.make_list(crate::rf_x2::Origin::Coherence).unwrap(); // rf-x2
        let name = INTERNED.intern("query block regression test");
        let slots: Vec<_> = {
            let mut state = list.gpu_time.state.lock().unwrap();
            let rec = state.rec.as_mut().unwrap();
            for _ in 0..BLOCK + 8 {
                rec.trace_mark(&list.list, What::Pass { name, kind: OURS }, render_trace::Command("test_marker", [0; 5]));
            }
            assert_eq!(rec.extra.len(), 1);
            rec.marks.iter().map(|mark| mark.0).collect()
        };
        list.close();
        let native = [Some(list.list.cast().unwrap())];
        // SAFETY: the list is closed and stays alive through its completion.
        unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            host.queue_send(&native, &[], Some((&fence, 1))).unwrap();
            fence.SetEventOnCompletion(1, HANDLE::default()).unwrap();
        }
        let timestamps: Vec<_> = slots.iter().map(|&slot| {
            // SAFETY: the queue finished resolving these slots into the mapping.
            unsafe { (ring.data as *const u64).add(slot as usize).read_volatile() }
        }).collect();
        assert!(timestamps.iter().all(|&tick| tick != 0));
        assert!(timestamps.windows(2).all(|pair| pair[0] <= pair[1]));
        collect(true);
        render_trace::flush();
    }
}
