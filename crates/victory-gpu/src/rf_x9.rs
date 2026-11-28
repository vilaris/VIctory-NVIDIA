//! Ownership backend configuration and fault handoff.

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default (measured: faster while streaming, even when standing); "0" turns it off.
    *ON.get_or_init(|| std::env::var_os("VICTORY_RF_X9").is_none_or(|v| v != "0"))
}

#[cfg(windows)]
pub(crate) fn incremental_bindless() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| enabled() && std::env::var_os("VICTORY_RF_X9_BINDLESS_INCREMENTAL").is_some_and(|v| v == "1"))
}

#[cfg(windows)]
pub(crate) fn defer_descriptor_rearm() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| enabled() && std::env::var_os("VICTORY_RF_X9_FAULTS").is_some_and(|v| v == "1"))
}

#[cfg(windows)]
pub(crate) fn verify() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| enabled() && std::env::var_os("VICTORY_RF_X9_VERIFY").is_some_and(|v| v == "1"))
}

/// Check the immutable upload destination against a read of the same
/// bytes through the title mapping. Changing/unmapped samples are inconclusive.
#[cfg(windows)]
pub(crate) fn verify_memory(class: &'static str, source: usize, destination: usize, len: usize) {
    verify_memory_at(class, source, source, destination, len);
}

#[cfg(windows)]
pub(crate) fn verify_memory_at(class: &'static str, address: usize, source: usize, destination: usize, len: usize) {
    const SAMPLE: usize = 64;
    let n = len.min(SAMPLE);
    if n == 0 {
        return;
    }
    let (mut expected, mut seen, mut reread) = ([0; SAMPLE], [0; SAMPLE], [0; SAMPLE]);
    let read = |at, bytes: &mut [u8]| crate::host::read_into(at as *const u8, &mut bytes[..n]);
    if read(source, &mut expected)
        && read(destination, &mut seen)
        && expected != seen
        && read(source, &mut reread)
        && expected == reread
    {
        tracing::warn!(
            class,
            address = format_args!("{address:#x}"),
            len,
            sampled = n,
            "rf-x9 diverge"
        );
    }
}

#[cfg(windows)]
fn stats() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RF_X9_STATS").is_some_and(|v| v == "1"))
}

#[cfg(windows)]
pub(crate) fn init_stats() {
    let _ = stats();
}

/// Disjoint nonempty intervals of one host copy, sorted by address.
#[derive(Clone, Default)]
pub(crate) struct Ranges(Vec<(usize, usize)>);

impl Ranges {
    pub(crate) fn insert(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let first = self.0.partition_point(|r| r.1 < from);
        let end = first + self.0[first..].partition_point(|r| r.0 <= to);
        if first == end {
            self.0.insert(first, (from, to));
        } else {
            let merged = (from.min(self.0[first].0), to.max(self.0[end - 1].1));
            self.0[first] = merged;
            self.0.drain(first + 1..end);
        }
    }

    pub(crate) fn covers(&self, from: usize, to: usize) -> bool {
        self.0
            .get(self.0.partition_point(|r| r.1 <= from))
            .is_some_and(|r| r.0 <= from && to <= r.1)
    }

    pub(crate) fn intersects(&self, from: usize, to: usize) -> bool {
        from < to && self.0.get(self.0.partition_point(|r| r.1 <= from)).is_some_and(|r| r.0 < to)
    }

    pub(crate) fn spans(&self) -> &[(usize, usize)] {
        &self.0
    }

    /// Visit the parts of an interval that still need to be refreshed.
    pub(crate) fn uncovered(&self, from: usize, to: usize, mut visit: impl FnMut(usize, usize)) {
        if from >= to {
            return;
        }
        let mut next = from;
        for &(lo, hi) in &self.0[self.0.partition_point(|r| r.1 <= from)..] {
            if lo >= to {
                break;
            }
            if next < lo {
                visit(next, lo);
            }
            next = next.max(hi);
        }
        if next < to {
            visit(next, to);
        }
    }
}

/// Split upload copies into disjoint jobs with a bounded byte count. The
/// offsets already belong to the submission, including any alignment gaps.
#[cfg(any(windows, test))]
pub(crate) fn upload_jobs(
    moves: &[(usize, usize, usize)],
    limit: usize,
) -> (Vec<(usize, usize, usize)>, Vec<usize>) {
    assert!(limit > 0);
    let mut copies = Vec::with_capacity(moves.len());
    let mut starts = vec![0];
    let mut used = 0;
    for &(mut source, mut target, mut bytes) in moves {
        while bytes != 0 {
            let take = bytes.min(limit - used);
            copies.push((source, target, take));
            source += take;
            target += take;
            bytes -= take;
            used += take;
            if used == limit {
                starts.push(copies.len());
                used = 0;
            }
        }
    }
    if used != 0 {
        starts.push(copies.len());
    }
    (copies, starts)
}

/// Jobs can fill separate upload ranges at once. If a destination is
/// reused, all overlapping writes stay in one job in original order.
#[cfg(any(windows, test))]
pub(crate) fn fill_jobs(fills: &[(usize, usize, usize, bool)]) -> (Vec<usize>, Vec<usize>) {
    const MIN_JOB: usize = 32;
    let mut order: Vec<_> = (0..fills.len()).collect();
    order.sort_unstable_by_key(|&i| fills[i].0);
    let mut starts = vec![0];
    let (mut first, mut end) = (0, 0);
    for (position, &i) in order.iter().enumerate() {
        let (target, _, bytes, _) = fills[i];
        if target >= end && position - first >= MIN_JOB {
            starts.push(position);
            first = position;
        }
        end = end.max(target.saturating_add(bytes));
    }
    if !order.is_empty() {
        starts.push(order.len());
    }
    for bounds in starts.windows(2) {
        order[bounds[0]..bounds[1]].sort_unstable();
    }
    (order, starts)
}

/// Tracks the live slices of one persistently mapped upload buffer. A
/// reservation never crosses the physical end. Out-of-order retirement
/// cannot release an older live slice or the padding before it.
#[cfg(any(windows, test))]
pub(crate) struct UploadRing {
    capacity: u64,
    head: u64,
    tail: u64,
    pending: std::collections::VecDeque<(u64, bool)>,
}

#[cfg(any(windows, test))]
impl UploadRing {
    const ALIGN: u64 = 256;

    pub(crate) fn new(capacity: u64) -> Self {
        assert!(capacity != 0 && capacity.is_multiple_of(Self::ALIGN));
        Self { capacity, head: 0, tail: 0, pending: Default::default() }
    }

    /// Returns the buffer offset and the retirement token.
    pub(crate) fn reserve(&mut self, bytes: u64) -> Option<(u64, u64)> {
        if bytes == 0 || bytes > self.capacity {
            return None;
        }
        let bytes = bytes.next_multiple_of(Self::ALIGN);
        let offset = self.tail % self.capacity;
        let padding = if bytes > self.capacity - offset { self.capacity - offset } else { 0 };
        let start = self.tail.checked_add(padding)?;
        let end = start.checked_add(bytes)?;
        if end - self.head > self.capacity {
            return None;
        }
        self.tail = end;
        self.pending.push_back((end, false));
        Some((start % self.capacity, end))
    }

    pub(crate) fn release(&mut self, token: u64) {
        let retired = self.pending.iter_mut().find(|(end, _)| *end == token).expect("live upload lease");
        retired.1 = true;
        while let Some(&(end, true)) = self.pending.front() {
            self.pending.pop_front();
            self.head = end;
        }
        if self.pending.is_empty() {
            self.head = 0;
            self.tail = 0;
        }
    }
}

#[cfg(windows)]
mod telemetry {
    use super::{Ordering, stats};
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    #[derive(Clone, Copy)]
    pub(crate) enum Work {
        Refresh,
        Descriptor,
        Capture,
        Fault,
        RecordHeapWait,
        UploadPack,
        Texture,
        Fill,
        Planning,
        HeapDiscovery,
        UnderFlights,
        SettleStale,
    }

    #[derive(Clone, Copy)]
    pub(crate) enum Traffic {
        UploadScratch,
        CardUpload,
        CardReadback,
        DescriptorRead,
        KernelRefresh,
        GuardRefresh,
        UntrackedRefresh,
        TextureRead,
        UploadRing,
        CpuStreamDirect,
    }

    static WORK: [[AtomicU64; 2]; 12] =
        [const { [const { AtomicU64::new(0) }; 2] }; 12];
    static TRAFFIC: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
    static BITMAP: AtomicU64 = AtomicU64::new(0);
    static GAPS: AtomicU64 = AtomicU64::new(0);
    static WATCH_CHANGED: AtomicU64 = AtomicU64::new(0);
    static CPU_READ: AtomicU64 = AtomicU64::new(0);
    static GPU_WRITER: AtomicU64 = AtomicU64::new(0);
    static STALE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    static BINDLESS_BUFFERS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    static DESCRIPTOR_REARMS_DEFERRED: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn descriptor_rearm_deferred() {
        if stats() {
            DESCRIPTOR_REARMS_DEFERRED.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn bindless_buffers(fresh_frame: bool, slots: usize, refreshes: usize) {
        if stats() {
            BINDLESS_BUFFERS[usize::from(!fresh_frame)].fetch_add(1, Ordering::Relaxed);
            BINDLESS_BUFFERS[2].fetch_add(slots as u64, Ordering::Relaxed);
            BINDLESS_BUFFERS[3].fetch_add(refreshes as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn stale_reason(reason: usize) {
        if stats() {
            STALE[reason].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) struct Timer(Work, Instant);

    impl Drop for Timer {
        fn drop(&mut self) {
            let row = &WORK[self.0 as usize];
            row[0].fetch_add(1, Ordering::Relaxed);
            row[1].fetch_add(self.1.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn timer(kind: Work) -> Option<Timer> {
        stats().then(|| Timer(kind, Instant::now()))
    }

    pub(crate) fn traffic(kind: Traffic, bytes: impl FnOnce() -> u64) {
        if stats() {
            TRAFFIC[kind as usize].fetch_add(bytes(), Ordering::Relaxed);
        }
    }

    pub(crate) fn bitmap_fault() {
        if stats() {
            BITMAP.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn excluded_gaps(bytes: usize) {
        if stats() {
            GAPS.fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn watch_changed() {
        if stats() {
            WATCH_CHANGED.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn eager_landing() {
        if stats() {
            CPU_READ.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn gpu_writer_exposed() {
        if stats() {
            GPU_WRITER.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn sample(frames: f64, split: [u64; 16], flushes: [u64; 4]) {
        if !stats() || frames <= 0.0 {
            return;
        }
        let per_frame = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed) as f64 / frames;
        let [refresh, descriptor, capture, fault, record_heap_wait, pack, texture, fill, plan, discovery, under, settle] =
            WORK.each_ref().map(|row| [per_frame(&row[0]), per_frame(&row[1]) / 1e6]);
        let [scratch, upload, readback, descriptors, kernel, guard, untracked, texture_read, ring, stream] =
            TRAFFIC.each_ref().map(|value| per_frame(value) / (1 << 20) as f64);
        let ms = split.map(|ns| ns as f64 / 1e6 / frames);
        tracing::info!(
            "rf-x9: enabled={} frames={frames:.0} submit-ms[lock={:.3} plan={:.3} watch={:.3} flights={:.3} flight-prereq={:.3} steps={:.3} refresh={:.3} snapshots={:.3} card={:.3} heaps={:.3} send={:.3}] jobs[count/ms per frame refresh={:.1}/{:.3} descriptor={:.1}/{:.3} capture={:.1}/{:.3} record-heap-wait={:.1}/{:.3}] faults[count/ms per frame={:.1}/{:.3} bitmap={:.1}] MiB/frame[upload-scratch={scratch:.2} card-up={upload:.2} gpu-down={readback:.2} descriptors-read={descriptors:.2} kernel-refresh={kernel:.2} guard-refresh={guard:.2} untracked-refresh={untracked:.2} gaps-excluded={:.2}] transitions/frame[watch={:.1} parked-eager={:.1} gpu-writer={:.1}]",
            super::enabled(), ms[11], ms[12] + ms[14], ms[0], ms[1], ms[13], ms[2],
            ms[6], ms[3], ms[4], ms[15], ms[5], refresh[0], refresh[1], descriptor[0],
            descriptor[1], capture[0], capture[1], record_heap_wait[0], record_heap_wait[1],
            fault[0], fault[1], per_frame(&BITMAP),
            per_frame(&GAPS) / (1 << 20) as f64,
            per_frame(&WATCH_CHANGED), per_frame(&CPU_READ), per_frame(&GPU_WRITER),
        );
        tracing::info!(
            "rf-x9: jobs[count/ms per frame upload-pack={:.1}/{:.3} texture={:.1}/{:.3} fill={:.1}/{:.3} planning={:.1}/{:.3} heap-discovery={:.1}/{:.3}] MiB/frame[texture-read={texture_read:.2} upload-ring={ring:.2} cpu-stream-direct={stream:.2}]",
            pack[0], pack[1], texture[0], texture[1], fill[0], fill[1], plan[0], plan[1], discovery[0], discovery[1],
        );
        let [untracked, unknown, budget, snapshot] = STALE.each_ref().map(per_frame);
        tracing::info!(
            "rf-x9: flights[count/ms per frame merge={:.1}/{:.3} settle-and-recheck={:.1}/{:.3}] merge-fallbacks/frame[untracked-or-dirty={untracked:.1} unknown-writes={unknown:.1} changed-runs={budget:.1} no-snapshot={snapshot:.1}]",
            under[0], under[1], settle[0], settle[1],
        );
        let [frame_start, later, slots, scheduled] = BINDLESS_BUFFERS.each_ref().map(per_frame);
        let [waves, checks, clean, parallel] = flushes.map(|n| n as f64 / frames);
        tracing::info!(
            "rf-x9: refreshes/frame[flushes={waves:.1} checks={checks:.1} clean={clean:.1} parallel={parallel:.1}] bindless-buffers/frame[first={frame_start:.1} later={later:.1} slots={slots:.1} refreshes={scheduled:.1}] incremental={}",
            super::incremental_bindless(),
        );
        if super::defer_descriptor_rearm() {
            tracing::info!(
                "rf-x9: descriptor-rearms-deferred/frame={:.1}",
                per_frame(&DESCRIPTOR_REARMS_DEFERRED),
            );
        }
    }
}

#[cfg(windows)]
pub(crate) use telemetry::*;

/// Fast guard openings must finish before xmem changes the mapping. Its
/// existing write lock serializes remappers; the handler only uses atomics.
pub(crate) struct MappingGate {
    state: AtomicU64,
}

impl MappingGate {
    const CLOSED: u64 = 1 << 63;

    pub(crate) const fn new() -> Self {
        Self { state: AtomicU64::new(0) }
    }

    pub(crate) fn try_pin(&self) -> Option<MappingPin<'_>> {
        self.state.fetch_update(Ordering::Acquire, Ordering::Relaxed, |state| {
            (state & Self::CLOSED == 0).then_some(state + 1)
        }).ok()?;
        Some(MappingPin(self))
    }

    pub(crate) fn close(&self) {
        self.state.fetch_or(Self::CLOSED, Ordering::AcqRel);
        while self.state.load(Ordering::Acquire) != Self::CLOSED {
            std::thread::yield_now();
        }
    }

    pub(crate) fn reopen(&self) {
        self.state.store(0, Ordering::Release);
    }
}

pub(crate) struct MappingPin<'a>(&'a MappingGate);

impl Drop for MappingPin<'_> {
    fn drop(&mut self) {
        self.0.state.fetch_sub(1, Ordering::Release);
    }
}

// One bit per 64 KiB protection block in the title's 32-bit graphics
// window. A summary lets an empty drain cost one load and a sparse drain
// visit only groups that were written. All storage exists before the VEH.
const WORDS_PER_GROUP: usize = 16;
const GROUPS: usize = 64;
pub(crate) const FAULT_BLOCKS: usize = GROUPS * WORDS_PER_GROUP * 64;

pub(crate) struct FaultBitmap {
    groups: AtomicU64,
    blocks: [AtomicU64; GROUPS * WORDS_PER_GROUP],
}

impl FaultBitmap {
    pub(crate) const fn new() -> Self {
        Self {
            groups: AtomicU64::new(0),
            blocks: [const { AtomicU64::new(0) }; GROUPS * WORDS_PER_GROUP],
        }
    }

    pub(crate) fn record(&self, block: usize) {
        let word = block / 64;
        self.blocks[word].fetch_or(1 << (block % 64), Ordering::Release);
        self.groups.fetch_or(1 << (word / WORDS_PER_GROUP), Ordering::Release);
    }

    // A single consumer. If a producer records during a drain, its bit
    // is either taken in this drain or remains advertised for the next.
    pub(crate) fn drain(&self, mut visit: impl FnMut(usize)) {
        if self.groups.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut groups = self.groups.swap(0, Ordering::AcqRel);
        while groups != 0 {
            let group = groups.trailing_zeros() as usize;
            groups &= groups - 1;
            let first = group * WORDS_PER_GROUP;
            for word in first..first + WORDS_PER_GROUP {
                let mut blocks = self.blocks[word].swap(0, Ordering::AcqRel);
                while blocks != 0 {
                    visit(word * 64 + blocks.trailing_zeros() as usize);
                    blocks &= blocks - 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_jobs_split_large_runs_and_preserve_offsets_and_gaps() {
        let (moves, starts) = upload_jobs(&[(100, 0, 0), (200, 16, 12), (400, 64, 3), (500, 80, 2)], 8);
        assert_eq!(moves, [(200, 16, 8), (208, 24, 4), (400, 64, 3), (500, 80, 1), (501, 81, 1)]);
        assert_eq!(starts, [0, 1, 4, 5]);
        assert_eq!(upload_jobs(&[(1, 2, 0)], 8), (Vec::new(), vec![0]));
    }

    #[test]
    fn upload_ring_wraps_after_older_slices_retire() {
        let mut ring = UploadRing::new(1024);
        let (a, first) = ring.reserve(500).unwrap();
        let (b, second) = ring.reserve(250).unwrap();
        assert_eq!((a, b), (0, 512));
        assert!(ring.reserve(500).is_none());
        ring.release(first);
        let (c, third) = ring.reserve(500).unwrap();
        assert_eq!(c, 0);
        ring.release(third);
        assert!(ring.reserve(1).is_none());
        ring.release(second);
        assert_eq!(ring.reserve(1024).map(|r| r.0), Some(0));
    }

    #[test]
    fn fill_jobs_keep_a_chain_of_overlapping_writes_ordered() {
        let mut fills: Vec<_> = (0..30).map(|i| (i * 10, 0, 2, false)).collect();
        fills.extend([(330, 0, 20, false), (300, 0, 20, false), (310, 0, 30, false), (400, 0, 2, false)]);
        let (order, starts) = fill_jobs(&fills);
        assert_eq!(starts, [0, 33, 34]);
        assert_eq!(order, (0..34).collect::<Vec<_>>());
    }

    #[test]
    fn bitmap_coalesces_and_drains_across_group_boundaries() {
        let map = FaultBitmap::new();
        for block in [FAULT_BLOCKS - 1, 64, 63, 1024, 63] {
            map.record(block);
        }
        let mut blocks = Vec::new();
        map.drain(|block| blocks.push(block));
        map.drain(|_| panic!("drained block reappeared"));
        assert_eq!(blocks, [63, 64, 1024, FAULT_BLOCKS - 1]);
    }

    #[test]
    fn a_write_during_drain_is_visible_on_the_next_drain() {
        let map = FaultBitmap::new();
        map.record(0);
        map.drain(|block| {
            assert_eq!(block, 0);
            map.record(0);
        });
        let mut blocks = Vec::new();
        map.drain(|block| blocks.push(block));
        assert_eq!(blocks, [0]);
    }

    #[test]
    fn disjoint_ranges_keep_gaps_out_of_completion() {
        let mut ranges = Ranges::default();
        ranges.insert(20, 30);
        ranges.insert(0, 10);
        ranges.insert(40, 50);
        assert!(!ranges.covers(5, 45));
        assert!(!ranges.covers(10, 20));
        assert!(!ranges.intersects(10, 20));
        assert!(ranges.intersects(10, 21));
        assert!(!ranges.intersects(5, 5));
        ranges.insert(10, 40);
        assert_eq!(ranges.spans(), [(0, 50)]);
        assert!(ranges.covers(5, 45));
    }

    #[test]
    fn remap_closes_gate_until_the_old_pin_and_mapping_are_done() {
        let gate = MappingGate::new();
        let pin = gate.try_pin().unwrap();
        std::thread::scope(|scope| {
            let change = scope.spawn(|| gate.close());
            while gate.state.load(Ordering::Acquire) & MappingGate::CLOSED == 0 {
                std::thread::yield_now();
            }
            assert!(gate.try_pin().is_none());
            assert!(!change.is_finished());
            drop(pin);
            change.join().unwrap();
            assert!(gate.try_pin().is_none());
        });
        gate.reopen();
        assert!(gate.try_pin().is_some());
    }
}
