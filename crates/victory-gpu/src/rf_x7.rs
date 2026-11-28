//! Resource ordering and barrier attribution for RF X7.

/// A global wait covers the imported work at the head of the list and
/// every subsequent interval that may have touched a UAV.
struct GlobalWait {
    dirty: bool,
}

impl Default for GlobalWait {
    fn default() -> Self {
        Self { dirty: true }
    }
}

impl GlobalWait {
    fn work(&mut self, uav_access: bool) {
        self.dirty |= uav_access;
    }

    fn wait(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
}

/// Returns whether the new wait shares the already pending GPU command.
fn queue_wait(pending: &mut bool) -> bool {
    std::mem::replace(pending, true)
}

fn redundant_transition(before: i32, after: i32, full: bool) -> bool {
    full && before == after
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::collections::HashMap;
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
        switches().title || switches().verify || narrow()
    }

    pub(crate) fn possible_uav_access(shaders: Option<&crate::pipeline::host::HostPipeline>) -> bool {
        shaders.is_none_or(|pipeline| {
            pipeline.stages.is_empty() || pipeline.stages.iter().any(|stage| {
                let shader = stage.translated();
                shader.stores || shader.stores_by_address || shader.rf_x7_bindless
                    || !stage.root_uavs.is_empty()
                    || !shader.host.root_indexed_views.is_empty()
                    || shader.used.iter().any(|view| view.kind == victory_shader::ResourceType::Uav)
            })
        })
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
        consecutive_calls: u64,
        within_call: u64,
        coalesced_calls: u64,
        coalesced_enhanced: u64,
        draws: u64,
        dispatches: u64,
        gaps: u64,
        max_draws: u64,
        max_dispatches: u64,
        would_omit: u64,
        omitted: u64,
        tracked_work: u64,
        possible_uav_work: u64,
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
            let adjacent = u64::from(gap.draws == 0 && gap.dispatches == 0 && !gap.other);
            s.consecutive += adjacent;
            s.consecutive_calls += adjacent;
        }
        s.consecutive += count - 1;
        s.within_call += count - 1;
    }

    fn coalesced(count: u64) {
        stats(|s| s.coalesced_calls += count);
    }

    pub(crate) fn mark_scoped_global() {
        stats(|s| s.scoped_global += 1);
    }

    /// Run two adjacent legacy arrays as one ordered array. The caller
    /// identifies adjacency at the point of recording, before any work.
    #[track_caller]
    pub(crate) fn adjacent_legacy(
        list: &ID3D12GraphicsCommandList,
        reason: &'static str,
        first: &[D3D12_RESOURCE_BARRIER],
        second: &[D3D12_RESOURCE_BARRIER],
    ) {
        // SAFETY: the callers provide an open list and live resources.
        unsafe {
            if narrow() && !first.is_empty() && !second.is_empty() {
                coalesced(1);
                let batch: Vec<_> = first.iter().chain(second).map(copy_barrier).collect();
                victory_barriers(list, reason, &batch, None);
            } else {
                for barriers in [first, second].into_iter().filter(|b| !b.is_empty()) {
                    victory_barriers(list, reason, barriers, None);
                }
            }
        }
    }

    #[track_caller]
    pub(crate) fn adjacent_title(
        host: &crate::host::HostList,
        reason: &'static str,
        victory: &[D3D12_RESOURCE_BARRIER],
    ) {
        let list = &host.list;
        if !victory.is_empty()
            && let Some(mut title) = host.rf_x7_title()
            && title.take_pending()
        {
            count_title(&[uav(None)]);
            coalesced(1);
            let batch: Vec<_> = std::iter::once(uav(None))
                .chain(victory.iter().filter(|b| !redundant(b)).map(copy_barrier))
                .collect();
            // Count the translated input once per origin. Emit the array once.
            account_victory(list, Site::at(reason, Location::caller()), victory);
            stats(|s| s.dropped_transition += (victory.len() + 1 - batch.len()) as u64);
            unsafe { crate::rf_x2::record_mixed(list, reason, &batch) };
            return;
        }
        unsafe { victory_barriers(list, reason, victory, None) };
    }

    fn copy_barrier(barrier: &D3D12_RESOURCE_BARRIER) -> D3D12_RESOURCE_BARRIER {
        // SAFETY: these unions hold borrowed pointers behind ManuallyDrop.
        unsafe { std::ptr::read(barrier) }
    }

    pub(crate) fn title_requests(site: &'static str, named: u64, global: u64) {
        stats(|s| {
            s.title_named += named;
            s.title_global += global;
            *s.title_requests.entry(site).or_default() += named + global;
        });
    }

    #[track_caller]
    pub(crate) fn enhanced_globals(list: &ID3D12GraphicsCommandList, groups: &[D3D12_BARRIER_GROUP]) {
        let site = Site::at("vx:barrier.place (enhanced)", Location::caller());
        stats(|s| {
            let count = groups.iter().map(|group| u64::from(group.NumBarriers)).sum::<u64>();
            boundary(s, list, count);
            *s.globals.entry(site).or_default() += count;
        });
    }

    /// Merge only global groups whose masks can be unioned without crossing
    /// a split barrier or a NO_ACCESS region. COMMON already covers all access
    /// types supported by the combined sync scope.
    ///
    /// # Safety
    /// The group payloads must be live and valid for the Barrier call.
    pub(crate) unsafe fn merge_global_groups(groups: &[D3D12_BARRIER_GROUP]) -> Option<D3D12_GLOBAL_BARRIER> {
        if !narrow() || groups.iter().any(|g| g.Type != D3D12_BARRIER_TYPE_GLOBAL) {
            return None;
        }
        let count = groups.iter().map(|g| u64::from(g.NumBarriers)).sum::<u64>();
        if count < 2 {
            return None;
        }
        let mut merged: Option<D3D12_GLOBAL_BARRIER> = None;
        for group in groups.iter().filter(|g| g.NumBarriers != 0) {
            // SAFETY: the caller's Barrier group and its payload are live.
            let globals = unsafe { std::slice::from_raw_parts(group.Anonymous.pGlobalBarriers, group.NumBarriers as usize) };
            for b in globals {
                if b.SyncBefore.0 & D3D12_BARRIER_SYNC_SPLIT.0 != 0
                    || b.SyncAfter.0 & D3D12_BARRIER_SYNC_SPLIT.0 != 0
                    || b.AccessBefore == D3D12_BARRIER_ACCESS_NO_ACCESS
                    || b.AccessAfter == D3D12_BARRIER_ACCESS_NO_ACCESS
                    || b.SyncBefore == D3D12_BARRIER_SYNC_NONE
                    || b.SyncAfter == D3D12_BARRIER_SYNC_NONE
                {
                    return None;
                }
                merged = Some(match merged {
                    None => *b,
                    Some(a) => D3D12_GLOBAL_BARRIER {
                        SyncBefore: union_sync(a.SyncBefore, b.SyncBefore),
                        SyncAfter: union_sync(a.SyncAfter, b.SyncAfter),
                        AccessBefore: union_access(a.AccessBefore, b.AccessBefore),
                        AccessAfter: union_access(a.AccessAfter, b.AccessAfter),
                    },
                });
            }
        }
        stats(|s| s.coalesced_enhanced += count - 1);
        merged
    }

    fn union_sync(a: D3D12_BARRIER_SYNC, b: D3D12_BARRIER_SYNC) -> D3D12_BARRIER_SYNC {
        if a == D3D12_BARRIER_SYNC_ALL || b == D3D12_BARRIER_SYNC_ALL {
            D3D12_BARRIER_SYNC_ALL
        } else {
            a | b
        }
    }

    fn union_access(a: D3D12_BARRIER_ACCESS, b: D3D12_BARRIER_ACCESS) -> D3D12_BARRIER_ACCESS {
        if a == D3D12_BARRIER_ACCESS_COMMON || b == D3D12_BARRIER_ACCESS_COMMON {
            D3D12_BARRIER_ACCESS_COMMON
        } else {
            a | b
        }
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
        account_victory(list, site, barriers);
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
                actual.push(copy_barrier(b));
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

    fn account_victory(list: &ID3D12GraphicsCommandList, site: Site, barriers: &[D3D12_RESOURCE_BARRIER]) {
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
    }

    fn is_global(b: &D3D12_RESOURCE_BARRIER) -> bool {
        // SAFETY: the type selects the union member.
        b.Type == D3D12_RESOURCE_BARRIER_TYPE_UAV && unsafe { b.Anonymous.UAV.pResource.is_none() }
    }

    fn count_title(barriers: &[D3D12_RESOURCE_BARRIER]) {
        stats(|s| {
            for b in barriers.iter().filter(|b| b.Type == D3D12_RESOURCE_BARRIER_TYPE_UAV) {
                if is_global(b) {
                    s.title_emitted_global += 1;
                } else {
                    s.title_emitted_named += 1;
                }
            }
        });
    }

    fn emit_title(list: &ID3D12GraphicsCommandList, barriers: &[D3D12_RESOURCE_BARRIER]) {
        count_title(barriers);
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
            consecutive = s.consecutive, consecutive_calls = s.consecutive_calls,
            within_call = s.within_call, coalesced_calls = s.coalesced_calls,
            coalesced_enhanced = s.coalesced_enhanced,
            gaps = s.gaps, draws_between = s.draws, dispatches_between = s.dispatches,
            max_draws_between = s.max_draws, max_dispatches_between = s.max_dispatches,
            would_omit = s.would_omit, omitted = s.omitted,
            tracked_work = s.tracked_work, possible_uav_work = s.possible_uav_work,
            top_global = ?top_global, top_transition = ?top_transition, top_pairs = ?top_pairs,
            globals_by_site = ?s.globals, transitions_by_site_and_state = ?s.transitions,
            "rf-x7:"
        );
    }

    /// A legacy global wait may remain pending only until the next call
    /// that could record GPU work. It owns no resource or borrowed data.
    #[derive(Default)]
    pub(crate) struct Title {
        wait: GlobalWait,
        pending: bool,
    }

    impl Title {
        pub(crate) fn flush(&mut self, list: &ID3D12GraphicsCommandList) {
            if std::mem::take(&mut self.pending) {
                emit_title(list, &[uav(None)]);
            }
        }

        pub(crate) fn work(&mut self, list: &ID3D12GraphicsCommandList, uav_access: bool) {
            self.flush(list);
            self.wait.work(uav_access);
        }

        pub(crate) fn shader_work(&mut self, list: &ID3D12GraphicsCommandList, uav_access: bool) {
            stats(|s| {
                s.tracked_work += 1;
                s.possible_uav_work += u64::from(uav_access);
            });
            self.work(list, uav_access);
        }

        /// Return the pending global to a caller that can record it in the
        /// same array as the next barrier, with no work in between.
        pub(crate) fn take_pending(&mut self) -> bool {
            std::mem::take(&mut self.pending)
        }

        fn barriers(&mut self, list: &ID3D12GraphicsCommandList, barriers: &[D3D12_RESOURCE_BARRIER]) {
            if barriers.len() == 1 && is_global(&barriers[0]) && plain_uav(&barriers[0]) {
                if switches().title || switches().verify {
                    let required = self.wait.wait();
                    if !required {
                        omitted();
                    }
                    if switches().verify || required {
                        self.global(list, barriers);
                    }
                } else {
                    self.global(list, barriers);
                }
            } else {
                if barriers.iter().any(|b| is_global(b) && plain_uav(b)) {
                    self.wait.wait();
                }
                if self.take_pending() {
                    let mut actual = Vec::with_capacity(1 + barriers.len());
                    actual.push(uav(None));
                    actual.extend(barriers.iter().map(copy_barrier));
                    coalesced(1);
                    emit_title(list, &actual);
                } else {
                    emit_title(list, barriers);
                }
            }
        }

        fn global(&mut self, list: &ID3D12GraphicsCommandList, barriers: &[D3D12_RESOURCE_BARRIER]) {
            if narrow() && !switches().verify {
                if queue_wait(&mut self.pending) {
                    coalesced(1);
                }
            } else {
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
            title.barriers(&host.list, barriers);
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

    #[test]
    fn keeps_first_wait_and_skips_read_only_shader_intervals() {
        let mut wait = GlobalWait::default();
        assert!(wait.wait());
        wait.work(false);
        wait.work(false);
        assert!(!wait.wait());
    }

    #[test]
    fn keeps_wait_after_possible_uav_use_or_unclassified_work() {
        let mut wait = GlobalWait::default();
        assert!(wait.wait());
        wait.work(true);
        wait.work(false);
        assert!(wait.wait());
        assert!(!wait.wait());
    }

    #[test]
    fn joins_globals_only_while_a_wait_is_pending() {
        let mut pending = false;
        assert!(!queue_wait(&mut pending));
        assert!(queue_wait(&mut pending));
        assert!(std::mem::take(&mut pending));
        assert!(!queue_wait(&mut pending));
    }

    #[test]
    fn drops_only_full_same_state_transitions() {
        assert!(redundant_transition(8, 8, true));
        assert!(!redundant_transition(8, 8, false));
        assert!(!redundant_transition(8, 64, true));
    }
}
