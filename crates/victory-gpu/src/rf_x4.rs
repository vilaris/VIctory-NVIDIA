//! Descriptor sync experiments. GPU-computed indices have no CPU readback.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct PickedRange {
    past: u32,
}

impl PickedRange {
    pub(super) fn whole(count: u32) -> Self {
        Self { past: count }
    }

    pub(super) fn contains(self, slot: u32) -> bool {
        slot < self.past
    }

    pub(super) fn intersects(self, first: u32, past: u32) -> bool {
        first < past.min(self.past)
    }
}

pub(super) fn speculative_rounds(gate: bool, late: bool) -> bool {
    !(gate && late)
}

const VIEWS: u8 = 1;
const SAMPLERS: u8 = 2;
const BOTH: u8 = VIEWS | SAMPLERS;

fn emitted_heaps(features: Option<u64>, strides: bool) -> u8 {
    let Some(bits) = features else { return BOTH };
    (u8::from(strides || bits & 0x200_0000 != 0) * VIEWS)
        | (u8::from(bits & 0x400_0000 != 0) * SAMPLERS)
}

/// Equal ranges are common even on dirty pages. Split unequal ranges into
/// small blocks before inspecting individual records. Returns the number of
/// record comparisons executed after the block checks.
pub(super) fn changes(last: &mut [u8], now: &[u8], step: usize, first: u32, changed: &mut Vec<u32>) -> u64 {
    debug_assert_eq!(last.len(), now.len());
    if last == now {
        return 0;
    }
    const GROUP: usize = 512;
    const BLOCK: usize = 64;
    let mut compared = 0;
    for (group, (old, new)) in last.chunks_mut(GROUP).zip(now.chunks(GROUP)).enumerate() {
        if old == new {
            continue;
        }
        for (block, (old, new)) in old.chunks_mut(BLOCK).zip(new.chunks(BLOCK)).enumerate() {
            if old == new {
                continue;
            }
            let base = first + ((group * GROUP + block * BLOCK) / step) as u32;
            compared += (old.len() / step) as u64;
            for (n, (old, new)) in old.chunks_exact_mut(step).zip(new.chunks_exact(step)).enumerate() {
                if old != new {
                    old.copy_from_slice(new);
                    changed.push(base + n as u32);
                }
            }
        }
    }
    compared
}

/// Counts for one round or one reporting window. Each row has a single unit.
#[derive(Default)]
pub(super) struct Sample {
    pub rounds: u64,
    pub requested: u64,
    pub dirty: u64,
    pub skipped_heaps: u64,
    pub read_slots: u64,
    pub compared_slots: u64,
    pub mirrored: u64,
    pub bytes: u64,
    pub asking_ns: u64,
    pub reading_ns: u64,
    pub comparing_ns: u64,
    pub making_ns: u64,
    pub ring: u64,
    pub other: u64,
    pub picked: u64,
    pub table_only: u64,
    pub unknown: u64,
}

impl Sample {
    pub(super) fn add(&mut self, rhs: &Self) {
        self.rounds += rhs.rounds;
        self.requested += rhs.requested;
        self.dirty += rhs.dirty;
        self.skipped_heaps += rhs.skipped_heaps;
        self.read_slots += rhs.read_slots;
        self.compared_slots += rhs.compared_slots;
        self.mirrored += rhs.mirrored;
        self.bytes += rhs.bytes;
        self.asking_ns += rhs.asking_ns;
        self.reading_ns += rhs.reading_ns;
        self.comparing_ns += rhs.comparing_ns;
        self.making_ns += rhs.making_ns;
        self.ring += rhs.ring;
        self.other += rhs.other;
        self.picked += rhs.picked;
        self.table_only += rhs.table_only;
        self.unknown += rhs.unknown;
    }
}

#[cfg(windows)]
mod runtime {
    use super::{BOTH, SAMPLERS, Sample, VIEWS, emitted_heaps, speculative_rounds};
    use crate::host::HostList;
    use crate::object::This;
    use crate::pipeline::{Shader, Translated, host::HostPipeline};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use victory_shader::emit::{SLOT_STRIDES, SLOT_TABLE_STRIDES};

    const STATS: u8 = 1;
    const PICK_GATE: u8 = 2;
    const PICKED: u8 = 4;
    const VERIFY: u8 = 8;

    fn switches() -> u8 {
        static SWITCHES: OnceLock<u8> = OnceLock::new();
        *SWITCHES.get_or_init(|| {
            let on = |name| u8::from(std::env::var_os(name).is_some_and(|v| v == "1"));
            (on("VICTORY_RF_X4_STATS") * STATS)
                | (on("VICTORY_RF_X4_PICK_GATE") * PICK_GATE)
                | (on("VICTORY_RF_X4_PICKED") * PICKED)
                | (on("VICTORY_RF_X4_VERIFY") * VERIFY)
        })
    }

    pub(crate) fn stats() -> bool {
        switches() & STATS != 0
    }

    pub(crate) fn picked() -> bool {
        switches() & PICKED != 0
    }

    pub(crate) fn verify() -> bool {
        switches() & VERIFY != 0
    }

    pub(crate) fn late_lane() -> bool {
        crate::host::late() && crate::queue::cuts()
    }

    pub(crate) fn speculative() -> bool {
        switches() & PICK_GATE == 0 || speculative_rounds(true, late_lane())
    }

    pub(crate) fn note_enabled() -> bool {
        switches() != 0 || crate::rf_x9::enabled()
    }

    /// Mark the actual heaps bound at this operation, retaining the list bit
    /// across thread handoffs, delayed waits and additional submissions.
    fn note(list: &HostList, bound: [usize; 2], mask: u8, shader: &Shader, pso: &HostPipeline, title: This) {
        if mask == 0 {
            return;
        }
        if switches() & (PICK_GATE | PICKED) != 0 || crate::rf_x9::enabled() {
            crate::heap_marks::note_pick(list);
        }
        for at in bound.into_iter().filter(|&at| at != 0) {
            let heap = crate::descriptor::heap(at as crate::object::This);
            let bit = if heap.samplers() { SAMPLERS } else { VIEWS };
            if mask & bit != 0 {
                if verify() || crate::rf_x9::verify() {
                    heap.note_verify_reader(shader, pso, title);
                }
                heap.note_indexed();
            }
        }
    }

    thread_local! {
        // Shader containers are interned for the process lifetime. Cache the
        // proof per recording thread so draws need no shared lock.
        static PROOFS: std::cell::RefCell<std::collections::HashMap<usize, u8>> = Default::default();
    }

    pub(crate) fn note_shader(list: &HostList, bound: [usize; 2], shader: &Shader, t: &Translated, pso: &HostPipeline, title: This, uncertain: bool) {
        let mask = PROOFS.with_borrow_mut(|proofs| {
            *proofs.entry(std::ptr::from_ref(shader) as usize).or_insert_with(|| reachable(t))
        });
        note(list, bound, if uncertain { BOTH } else { mask }, shader, pso, title);
    }

    fn reachable(t: &Translated) -> u8 {
        use victory_shader::{Container, FourCC};
        // The emitter sets these SFI0 features whenever it emits a heap
        // handle. They cover images and samplers without a stride root SRV.
        let features = Container::parse(&t.host.dxil).ok().and_then(|c| {
            let bytes = c.part(FourCC::SFI0)?.data.get(..8)?.try_into().ok()?;
            Some(u64::from_le_bytes(bytes))
        });
        // Static guest provenance only identifies a root and an offset.
        // The emitted handle can still refer to the mutable title heap
        // when a table is direct or the snapshot is unavailable.
        let strides = t.host.root_srvs.iter().any(|slot| matches!(*slot, SLOT_STRIDES | SLOT_TABLE_STRIDES));
        emitted_heaps(features, strides)
    }

    struct Window {
        started: Instant,
        frames: f64,
        sample: Sample,
    }

    fn window() -> std::sync::MutexGuard<'static, Window> {
        static WINDOW: OnceLock<Mutex<Window>> = OnceLock::new();
        WINDOW.get_or_init(|| Mutex::new(Window { started: Instant::now(), frames: 0.0, sample: Sample::default() }))
            .lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(in crate::descriptor) fn record(sample: &Sample) {
        window().sample.add(sample);
    }

    pub(in crate::descriptor) fn line(frames: f64) {
        let mut window = window();
        window.frames += frames;
        if window.started.elapsed() < Duration::from_secs(5) {
            return;
        }
        let f = window.frames.max(1.0);
        let s = std::mem::take(&mut window.sample);
        window.started = Instant::now();
        window.frames = 0.0;
        drop(window);
        let per_frame = |n: u64| n as f64 / f;
        let ms = |n| per_frame(n) / 1e6;
        tracing::info!(
            "rf-x4: per frame rounds={:.2} pages-requested={:.0} pages-dirty={:.1} ring={:.1} other={:.1} heaps-skipped={:.1} \
             slots read={:.0} compared={:.0} mirrored={:.0} MB-read={:.2} asking-ms={:.2} reading-ms={:.2} comparing-ms={:.2} making-ms={:.2} \
             changed possible-picked={:.0} table-only={:.0} unknown={:.0}",
            per_frame(s.rounds), per_frame(s.requested), per_frame(s.dirty), per_frame(s.ring), per_frame(s.other),
            per_frame(s.skipped_heaps), per_frame(s.read_slots), per_frame(s.compared_slots), per_frame(s.mirrored),
            per_frame(s.bytes) / (1 << 20) as f64, ms(s.asking_ns), ms(s.reading_ns), ms(s.comparing_ns), ms(s.making_ns),
            per_frame(s.picked), per_frame(s.table_only), per_frame(s.unknown),
        );
    }
}

#[cfg(windows)]
pub(crate) use runtime::{late_lane, note_enabled, note_shader, picked, speculative, stats, verify};
#[cfg(windows)]
pub(super) use runtime::{line, record};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x4_gate_only_removes_speculation_from_the_late_lane() {
        assert!(speculative_rounds(false, false));
        assert!(speculative_rounds(false, true));
        assert!(speculative_rounds(true, false));
        assert!(!speculative_rounds(true, true));
    }

    #[test]
    fn emitted_heap_handles_keep_coverage_even_if_guest_sources_are_static() {
        assert_eq!(emitted_heaps(None, false), BOTH);
        assert_eq!(emitted_heaps(Some(0), false), 0);
        assert_eq!(emitted_heaps(Some(0), true), VIEWS);
        assert_eq!(emitted_heaps(Some(0x200_0000), false), VIEWS);
        assert_eq!(emitted_heaps(Some(0x400_0000), false), SAMPLERS);
        assert_eq!(emitted_heaps(Some(0x400_0000), true), BOTH);
    }

    #[test]
    fn x4_picked_range_includes_every_mirrored_slot() {
        assert!(!PickedRange::default().intersects(0, 128));
        let all = PickedRange::whole(1000);
        assert!(all.contains(0));
        assert!(all.contains(999));
        assert!(all.intersects(960, 1024));
        assert!(!all.intersects(1000, 1024));
        assert!(!all.intersects(5, 5));
    }

    #[test]
    fn x4_block_compare_matches_a_record_scan_including_the_tail() {
        for step in [16, 32] {
            let mut old = vec![0; 71 * step];
            let mut now = old.clone();
            for i in [0, 3, 31, 32, 70] {
                now[(i + 1) * step - 1] = 7;
            }
            let mut changed = Vec::new();
            assert!(changes(&mut old, &now, step, 40, &mut changed) < 71);
            assert_eq!(changed, [40, 43, 71, 72, 110]);
            assert_eq!(old, now);
            changed.clear();
            assert_eq!(changes(&mut old, &now, step, 40, &mut changed), 0);
            assert!(changed.is_empty());
        }
    }

    #[test]
    fn x4_samples_add_and_reset_as_one_window() {
        let mut sample = Sample { rounds: 1, read_slots: 8, picked: 3, ..Default::default() };
        sample.add(&Sample { rounds: 2, read_slots: 7, table_only: 4, ..Default::default() });
        let previous = std::mem::take(&mut sample);
        assert_eq!((previous.rounds, previous.read_slots, previous.picked, previous.table_only), (3, 15, 3, 4));
        assert_eq!(sample.rounds, 0);
    }
}
