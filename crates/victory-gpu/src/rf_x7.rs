//! Resource ordering and barrier attribution for RF X7.

/// A whole-resource access, with an optional conservative range in title memory.
/// Two shadows of the same title bytes must also be treated as aliases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Access {
    resource: u64,
    start: u64,
    end: u64,
    writes: bool,
    uav: bool,
}

impl Access {
    fn overlaps(self, other: Self) -> bool {
        self.resource == other.resource || self.start < other.end && other.start < self.end
    }

    fn hazards(self, other: Self) -> bool {
        (self.writes || other.writes) && self.overlaps(other)
    }
}

#[derive(Clone, Default)]
struct Footprint {
    accesses: Vec<Access>,
    unknown: bool,
}

impl Footprint {
    fn add(&mut self, access: Access) {
        if !self.unknown && !self.accesses.contains(&access) {
            if self.accesses.len() == 128 {
                self.accesses.clear();
                self.unknown = true;
            } else {
                self.accesses.push(access);
            }
        }
    }

    fn conflicts(&self, next: &Self) -> bool {
        self.unknown || next.unknown ||
            self.accesses.iter().any(|&a| next.accesses.iter().any(|&b| a.hazards(b)))
    }

    fn conservative_conflict(&self, next: &Self) -> bool {
        self.unknown || next.unknown ||
            self.accesses.iter().any(|&a| next.accesses.iter().any(|&b| {
                // Assume every UAV can write and include adjacent title
                // spans whose bounds the tracker could have underestimated.
                (a.uav || b.uav || a.writes || b.writes) &&
                    (a.resource == b.resource || a.start <= b.end && b.start <= a.end)
            }))
    }
}

/// Work before the pending wait and work since the last effective global wait.
/// Keep both: a later wait can absorb the pending wait and covers more work.
struct Frontier {
    all: Footprint,
    pending: Option<Footprint>,
}

impl Default for Frontier {
    fn default() -> Self {
        Self { all: Footprint { unknown: true, ..Footprint::default() }, pending: None }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Wait {
    Record,
    Delay,
    Merge,
    Covered,
}

impl Frontier {
    fn wait(&mut self) -> Wait {
        if self.all.unknown {
            self.flush();
            return Wait::Record;
        }
        if self.all.accesses.is_empty() && self.pending.is_none() {
            return Wait::Covered;
        }
        let merged = self.pending.is_some();
        self.pending = Some(self.all.clone());
        if merged { Wait::Merge } else { Wait::Delay }
    }

    fn flush(&mut self) -> bool {
        self.all.accesses.clear();
        self.all.unknown = false;
        self.pending.take().is_some()
    }

    fn record(&mut self, next: Footprint) {
        if next.unknown {
            self.all.unknown = true;
        } else {
            for access in next.accesses {
                self.all.add(access);
            }
        }
    }
}

fn redundant_transition(before: i32, after: i32, full: bool) -> bool {
    full && before == after
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::mem::ManuallyDrop;
    use std::panic::Location;
    use std::sync::{Mutex, OnceLock};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::core::Interface;

    #[derive(Default)]
    struct Switches {
        stats: bool,
        title: bool,
        narrow: bool,
        verify: bool,
    }

    fn switches() -> &'static Switches {
        static SWITCHES: OnceLock<Switches> = OnceLock::new();
        SWITCHES.get_or_init(|| {
            let on = |name| std::env::var_os(name).is_some_and(|v| v == "1");
            Switches {
                stats: on("VICTORY_RF_X7_STATS"),
                title: on("VICTORY_RF_X7_TITLE_UAV"),
                narrow: on("VICTORY_RF_X7_NARROW"),
                verify: on("VICTORY_RF_X7_VERIFY"),
            }
        })
    }

    pub(crate) fn narrow() -> bool {
        switches().narrow
    }

    pub(crate) fn tracking() -> bool {
        switches().title || switches().verify
    }

    #[derive(Default)]
    struct Gap {
        draws: u64,
        dispatches: u64,
        other: bool,
    }

    #[derive(Default)]
    struct Stats {
        title_named: u64,
        title_global: u64,
        title_requests: HashMap<&'static str, u64>,
        title_emitted_named: u64,
        title_emitted_global: u64,
        globals: HashMap<Site, u64>,
        transitions: HashMap<(Site, i32, i32), u64>,
        redundant: u64,
        dropped_transition: u64,
        narrowed: u64,
        scoped_global: u64,
        consecutive: u64,
        draws: u64,
        dispatches: u64,
        gaps: u64,
        max_draws: u64,
        max_dispatches: u64,
        would_omit: u64,
        omitted: u64,
        conservative_hazards: u64,
        conservative_omissions: u64,
        delayed_past_work: u64,
        tracked_work: u64,
        gaps_by_list: HashMap<usize, Option<Gap>>,
    }

    static STATS: OnceLock<Mutex<Stats>> = OnceLock::new();

    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    struct Site {
        reason: &'static str,
        file: &'static str,
        line: u32,
    }

    impl Site {
        fn at(reason: &'static str, at: &'static Location<'static>) -> Self {
            Self { reason, file: at.file(), line: at.line() }
        }
    }

    impl std::fmt::Debug for Site {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let file = self.file.rsplit(['/', '\\']).next().unwrap_or(self.file);
            write!(f, "{}@{}:{}", self.reason, file, self.line)
        }
    }

    fn stats(f: impl FnOnce(&mut Stats)) {
        if switches().stats || switches().verify {
            let mut s = STATS.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner());
            f(&mut s);
        }
    }

    pub(crate) fn open(list: &ID3D12GraphicsCommandList) {
        stats(|s| { s.gaps_by_list.insert(list.as_raw() as usize, None); });
    }

    pub(crate) fn close(list: &ID3D12GraphicsCommandList) {
        stats(|s| { s.gaps_by_list.remove(&(list.as_raw() as usize)); });
    }

    pub(crate) fn collecting() -> bool {
        switches().stats || switches().verify
    }

    pub(crate) fn work(list: &ID3D12GraphicsCommandList, compute: Option<bool>) {
        stats(|s| {
            if let Some(Some(gap)) = s.gaps_by_list.get_mut(&(list.as_raw() as usize)) {
                match compute {
                    Some(true) => gap.dispatches += 1,
                    Some(false) => gap.draws += 1,
                    None => gap.other = true,
                }
            }
        });
    }

    fn boundary(s: &mut Stats, list: &ID3D12GraphicsCommandList, count: u64) {
        if count == 0 {
            return;
        }
        if let Some(gap) = s.gaps_by_list.get_mut(&(list.as_raw() as usize)).and_then(|gap| gap.replace(Gap::default())) {
            s.gaps += 1;
            s.draws += gap.draws;
            s.dispatches += gap.dispatches;
            s.max_draws = s.max_draws.max(gap.draws);
            s.max_dispatches = s.max_dispatches.max(gap.dispatches);
            s.consecutive += u64::from(gap.draws == 0 && gap.dispatches == 0 && !gap.other);
        }
        s.consecutive += count - 1;
    }

    pub(crate) fn title_requests(site: &'static str, named: u64, global: u64) {
        stats(|s| {
            s.title_named += named;
            s.title_global += global;
            *s.title_requests.entry(site).or_default() += named + global;
        });
    }

    #[track_caller]
    pub(crate) fn enhanced_globals(groups: &[D3D12_BARRIER_GROUP]) {
        let site = Site::at("vx:barrier.place (enhanced)", Location::caller());
        stats(|s| {
            let count = groups.iter().map(|group| u64::from(group.NumBarriers)).sum::<u64>();
            *s.globals.entry(site).or_default() += count;
        });
    }

    fn redundant(b: &D3D12_RESOURCE_BARRIER) -> bool {
        if b.Type != D3D12_RESOURCE_BARRIER_TYPE_TRANSITION {
            return false;
        }
        // SAFETY: the type selects this union member.
        let t = unsafe { &b.Anonymous.Transition };
        redundant_transition(t.StateBefore.0, t.StateAfter.0, b.Flags == D3D12_RESOURCE_BARRIER_FLAG_NONE)
    }

    /// Legacy resource UAV ordering. The references remain live through recording.
    fn uav(resource: Option<&ID3D12Resource>) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    // SAFETY: the COM pointer is borrowed, just as in host::transition.
                    pResource: ManuallyDrop::new(resource.map(|r| unsafe { std::mem::transmute_copy(r) })),
                }),
            },
        }
    }

    pub(crate) struct ClampScope<'a> {
        pub(crate) inputs: &'a [&'a ID3D12Resource],
        pub(crate) budget: &'a ID3D12Resource,
    }

    impl ClampScope<'_> {
        fn has_writable_input(&self) -> bool {
            self.inputs.iter().any(|r| {
                // SAFETY: each buffer is a live SRV argument to the clamp.
                unsafe { r.GetDesc() }.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
            })
        }
    }

    fn enhanced_clamp(list: &ID3D12GraphicsCommandList) -> Option<ID3D12GraphicsCommandList7> {
        static SUPPORTED: OnceLock<bool> = OnceLock::new();
        let host = crate::host::host()?;
        let supported = *SUPPORTED.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
            // SAFETY: the device fills this feature structure of its size.
            let result = unsafe {
                host.device.CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS12,
                    (&raw mut options).cast(),
                    std::mem::size_of_val(&options) as u32,
                )
            };
            result.is_ok() && options.EnhancedBarriersSupported.as_bool()
        });
        if supported { list.cast().ok() } else { None }
    }

    fn clamp_global(list: &ID3D12GraphicsCommandList7, reason: &str) {
        // The title can produce arguments from graphics or compute UAVs.
        // The clamp consumes them as SRVs and updates the budget as a UAV.
        let accesses = [D3D12_GLOBAL_BARRIER {
            SyncBefore: D3D12_BARRIER_SYNC_ALL_SHADING,
            SyncAfter: D3D12_BARRIER_SYNC_COMPUTE_SHADING,
            AccessBefore: D3D12_BARRIER_ACCESS_UNORDERED_ACCESS,
            AccessAfter: D3D12_BARRIER_ACCESS_SHADER_RESOURCE | D3D12_BARRIER_ACCESS_UNORDERED_ACCESS,
        }];
        let group = D3D12_BARRIER_GROUP {
            Type: D3D12_BARRIER_TYPE_GLOBAL,
            NumBarriers: 1,
            Anonymous: D3D12_BARRIER_GROUP_0 { pGlobalBarriers: accesses.as_ptr() },
        };
        // SAFETY: the list is open and the one group is backed by accesses.
        unsafe { crate::rf_x2::record_global(list, reason, &[group]) };
    }

    /// The optional scope is the complete set of resources touched by the
    /// next Victory pass, excluding ones ordered by its state transitions.
    #[track_caller]
    pub(crate) unsafe fn victory_barriers(
        list: &ID3D12GraphicsCommandList,
        reason: &'static str,
        barriers: &[D3D12_RESOURCE_BARRIER],
        scope: Option<ClampScope<'_>>,
    ) {
        let site = Site::at(reason, Location::caller());
        stats(|s| {
            boundary(s, list, barriers.len() as u64);
            for b in barriers {
                if b.Type == D3D12_RESOURCE_BARRIER_TYPE_TRANSITION {
                    // SAFETY: selected by Type.
                    let t = unsafe { &b.Anonymous.Transition };
                    *s.transitions.entry((site, t.StateBefore.0, t.StateAfter.0)).or_default() += 1;
                    s.redundant += u64::from(redundant(b));
                } else if is_global(b) {
                    *s.globals.entry(site).or_default() += 1;
                }
            }
        });
        if !narrow() || scope.is_none() && !barriers.iter().any(redundant) {
            unsafe { crate::rf_x2::record_barriers(list, reason, barriers) };
            return;
        }
        let mut actual = Vec::with_capacity(barriers.len());
        let (mut removed, mut narrowed, mut scoped_global) = (0, 0, 0);
        for b in barriers {
            if redundant(b) {
                removed += 1;
            } else {
                if let Some(scope) = scope.as_ref().filter(|_| is_global(b)) {
                    if !scope.has_writable_input() {
                        actual.push(uav(Some(scope.budget)));
                        narrowed += 1;
                        continue;
                    }
                    // A legacy resource UAV barrier describes UAV-to-UAV.
                    // The mutable arguments leave that domain to be read as
                    // SRVs. A global Enhanced barrier describes those accesses
                    // without introducing a layout on the promoted buffers.
                    if actual.is_empty() && let Some(enhanced) = enhanced_clamp(list) {
                        clamp_global(&enhanced, reason);
                        scoped_global += 1;
                        continue;
                    }
                }
                // The original array continues to own all its pointers.
                // ResourceBarrier structs have no active automatic COM drops.
                actual.push(unsafe { std::ptr::read(b) });
            }
        }
        stats(|s| {
            s.dropped_transition += removed;
            s.narrowed += narrowed;
            s.scoped_global += scoped_global;
        });
        if !actual.is_empty() {
            unsafe { crate::rf_x2::record_barriers(list, reason, &actual) };
        }
    }

    fn is_global(b: &D3D12_RESOURCE_BARRIER) -> bool {
        // SAFETY: the type selects the union member.
        b.Type == D3D12_RESOURCE_BARRIER_TYPE_UAV && unsafe { b.Anonymous.UAV.pResource.is_none() }
    }

    fn emit_title(list: &ID3D12GraphicsCommandList, barriers: &[D3D12_RESOURCE_BARRIER]) {
        stats(|s| {
            for b in barriers.iter().filter(|b| b.Type == D3D12_RESOURCE_BARRIER_TYPE_UAV) {
                if is_global(b) {
                    s.title_emitted_global += 1;
                } else {
                    s.title_emitted_named += 1;
                }
            }
        });
        // SAFETY: the recording is open and every resource pointer is live.
        unsafe { crate::rf_x2::title_barriers(list, barriers) };
    }

    fn top<K: Ord, V>(data: impl IntoIterator<Item = (K, u64)>, format: impl Fn(K, u64) -> V) -> Vec<V> {
        let mut entries: Vec<_> = data.into_iter().collect();
        entries.sort_unstable_by(|(a, n), (b, m)| m.cmp(n).then_with(|| a.cmp(b)));
        entries.into_iter().take(8).map(|(k, n)| format(k, n)).collect()
    }

    pub(crate) fn frame(frame: u64) {
        let mut totals = None;
        stats(|s| {
            let lists = std::mem::take(&mut s.gaps_by_list);
            totals = Some(std::mem::take(s));
            s.gaps_by_list = lists;
        });
        let Some(s) = totals else { return };
        let transition_count: u64 = s.transitions.values().sum();
        let global_count: u64 = s.globals.values().sum();
        let mut transition_sites = HashMap::new();
        for ((site, _, _), count) in &s.transitions {
            *transition_sites.entry(*site).or_default() += *count;
        }
        let top_transition = top(transition_sites, |site, n| format!("{site:?}={n}"));
        let top_global = top(s.globals.iter().map(|(k, v)| (k, *v)), |site, n| format!("{site:?}={n}"));
        let top_pairs = top(s.transitions.iter().map(|(k, v)| (k, *v)), |(site, from, to), n| {
            format!("{site:?}[{from:#x}->{to:#x}]={n}")
        });
        tracing::info!(
            frame, stats = switches().stats, narrow = narrow(), title_uav = switches().title, verify = switches().verify,
            title_uav_named = s.title_named, title_uav_global = s.title_global,
            title_uav_emitted_named = s.title_emitted_named, title_uav_emitted_global = s.title_emitted_global,
            title_sites = ?s.title_requests, victory_global = global_count, victory_transition = transition_count,
            redundant_transition = s.redundant, dropped_transition = s.dropped_transition,
            narrowed_global = s.narrowed, scoped_global = s.scoped_global,
            consecutive = s.consecutive, gaps = s.gaps, draws_between = s.draws, dispatches_between = s.dispatches,
            max_draws_between = s.max_draws, max_dispatches_between = s.max_dispatches,
            would_omit = s.would_omit, omitted = s.omitted, delayed_past_work = s.delayed_past_work,
            tracked_work = s.tracked_work, conservative_hazards = s.conservative_hazards,
            conservative_omissions = s.conservative_omissions,
            top_global = ?top_global, top_transition = ?top_transition, top_pairs = ?top_pairs,
            globals_by_site = ?s.globals, transitions_by_site_and_state = ?s.transitions,
            "rf-x7:"
        );
    }

    /// Just the narrow set of commands with immutable, bounded bindings
    /// participates in the prediction. Any other command seals the window.
    #[derive(Default)]
    pub(crate) struct Title {
        frontier: Frontier,
        current: Option<Footprint>,
        ordered_named: HashSet<u64>,
        pending_conservative_hits: u64,
    }

    impl Title {
        fn flush(&mut self, list: &ID3D12GraphicsCommandList) {
            if self.frontier.flush() && !switches().verify {
                emit_title(list, &[uav(None)]);
            }
            self.pending_conservative_hits = 0;
        }

        pub(crate) fn opaque(&mut self, list: &ID3D12GraphicsCommandList) {
            self.flush(list);
            self.frontier.all.unknown = true;
            self.current = None;
            self.ordered_named.clear();
        }

        pub(crate) fn begin(&mut self, list: &ID3D12GraphicsCommandList, known: bool) {
            if known {
                self.current = Some(Footprint::default());
            } else {
                self.opaque(list);
            }
        }

        pub(crate) fn root(&mut self, shadow: &crate::shadow::Shadow, writes: bool, uav: bool) {
            if let Some(current) = &mut self.current {
                let (start, len) = shadow.span();
                current.add(Access {
                    resource: shadow.buffer.as_raw() as u64,
                    start,
                    end: start.saturating_add(len),
                    writes,
                    uav,
                });
            }
        }

        pub(crate) fn shader_work(&mut self, list: &ID3D12GraphicsCommandList) {
            let Some(next) = self.current.take() else { return };
            stats(|s| s.tracked_work += u64::from(!next.unknown));
            if next.unknown {
                self.ordered_named.clear();
            } else {
                for access in &next.accesses {
                    self.ordered_named.remove(&access.resource);
                }
            }
            if let Some(before) = &self.frontier.pending {
                if before.conflicts(&next) {
                    self.flush(list);
                } else {
                    let conservative = u64::from(switches().verify && before.conservative_conflict(&next));
                    self.pending_conservative_hits += conservative;
                    stats(|s| {
                        s.delayed_past_work += 1;
                        s.conservative_hazards += conservative;
                    });
                }
            }
            self.frontier.record(next);
        }

        fn barriers(&mut self, host: &crate::host::HostList, barriers: &[D3D12_RESOURCE_BARRIER]) {
            let list = &host.list;
            if barriers.len() == 1 && is_global(&barriers[0]) && plain_uav(&barriers[0]) {
                self.ordered_named.clear();
                let had_pending = self.frontier.pending.is_some();
                let wait = self.frontier.wait();
                if matches!(wait, Wait::Merge | Wait::Covered) || had_pending && wait == Wait::Record {
                    omitted();
                    let hits = std::mem::take(&mut self.pending_conservative_hits);
                    stats(|s| s.conservative_omissions += hits);
                }
                if switches().verify || wait == Wait::Record {
                    emit_title(list, barriers);
                }
            } else if barriers.iter().all(plain_uav) && barriers.iter().all(|b| !is_global(b)) {
                if self.frontier.pending.is_some() {
                    self.flush(list);
                    self.ordered_named.clear();
                }
                let mut actual = Vec::with_capacity(barriers.len());
                for b in barriers {
                    // SAFETY: plain_uav checked the union member. Keep it
                    // alive until reset, including across adjacent calls.
                    let Some(resource) = (unsafe { b.Anonymous.UAV.pResource.as_ref() }) else { continue };
                    if self.ordered_named.insert(resource.as_raw() as u64) {
                        host.keep(resource);
                        actual.push(uav(Some(resource)));
                    } else {
                        omitted();
                    }
                }
                if switches().verify {
                    emit_title(list, barriers);
                } else if !actual.is_empty() {
                    emit_title(list, &actual);
                }
            } else {
                self.opaque(list);
                emit_title(list, barriers);
            }
        }
    }

    fn plain_uav(b: &D3D12_RESOURCE_BARRIER) -> bool {
        b.Type == D3D12_RESOURCE_BARRIER_TYPE_UAV && b.Flags == D3D12_RESOURCE_BARRIER_FLAG_NONE
    }

    fn omitted() {
        stats(|s| {
            s.would_omit += 1;
            s.omitted += u64::from(!switches().verify);
        });
    }

    pub(crate) fn title_barriers(host: &crate::host::HostList, barriers: &[D3D12_RESOURCE_BARRIER]) {
        stats(|s| boundary(s, &host.list, barriers.len() as u64));
        if let Some(mut title) = host.rf_x7_title() {
            title.barriers(host, barriers);
        } else {
            emit_title(&host.list, barriers);
        }
    }
}

#[cfg(windows)]
pub(crate) use windows_impl::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn access(resource: u64, start: u64, end: u64, writes: bool) -> Access {
        Access { resource, start, end, writes, uav: writes }
    }

    fn footprint(access: Access) -> Footprint {
        let mut result = Footprint::default();
        result.add(access);
        result
    }

    #[test]
    fn detects_raw_war_waw_and_aliases() {
        let read = access(1, 0, 100, false);
        let write = access(1, 0, 100, true);
        let alias = access(2, 75, 200, true);
        let disjoint = access(3, 100, 300, true);
        assert!(read.hazards(write));
        assert!(write.hazards(read));
        assert!(write.hazards(write));
        assert!(read.hazards(alias));
        assert!(!read.hazards(read));
        assert!(!read.hazards(disjoint));
        assert!(write.hazards(access(1, 900, 950, false)));
    }

    #[test]
    fn retains_first_wait_and_joins_consecutive_waits() {
        let mut f = Frontier::default();
        assert_eq!(f.wait(), Wait::Record);
        assert_eq!(f.wait(), Wait::Covered);
        f.record(footprint(access(1, 0, 100, true)));
        assert_eq!(f.wait(), Wait::Delay);
        assert_eq!(f.wait(), Wait::Merge);
        assert!(f.flush());
        assert!(!f.flush());
    }

    #[test]
    fn delays_through_independent_work_and_carries_all_writers_forward() {
        let mut f = Frontier::default();
        let a = access(1, 0, 100, true);
        let b = access(2, 200, 300, true);
        assert_eq!(f.wait(), Wait::Record);
        f.record(footprint(a));
        assert_eq!(f.wait(), Wait::Delay);
        let next = footprint(b);
        assert!(!f.pending.as_ref().unwrap().conflicts(&next));
        f.record(next);
        assert_eq!(f.wait(), Wait::Merge);
        let pending = f.pending.as_ref().unwrap();
        assert!(pending.conflicts(&footprint(Access { writes: false, ..a })));
        assert!(pending.conflicts(&footprint(Access { writes: false, ..b })));
        assert!(f.flush());
    }

    #[test]
    fn always_conflicts_with_untracked_work() {
        let mut f = Frontier::default();
        assert_eq!(f.wait(), Wait::Record);
        f.record(footprint(access(1, 0, 100, true)));
        assert_eq!(f.wait(), Wait::Delay);
        let unknown = Footprint { unknown: true, ..Footprint::default() };
        assert!(f.pending.as_ref().unwrap().conflicts(&unknown));
        assert!(f.flush());
        f.record(unknown);
        assert_eq!(f.wait(), Wait::Record);
    }

    #[test]
    fn verification_counts_possible_uav_writes_and_boundary_aliases() {
        let before = footprint(Access { uav: true, ..access(1, 0, 100, false) });
        let after = footprint(access(2, 100, 200, false));
        assert!(!before.conflicts(&after));
        assert!(before.conservative_conflict(&after));
    }

    #[test]
    fn drops_only_full_same_state_transitions() {
        assert!(redundant_transition(8, 8, true));
        assert!(!redundant_transition(8, 8, false));
        assert!(!redundant_transition(8, 64, true));
    }
}
