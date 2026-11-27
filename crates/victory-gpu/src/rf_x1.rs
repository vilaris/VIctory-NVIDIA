//! Residency experiment switches, reservation accounting and periodic census.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

const MIB: u64 = 1 << 20;
const BUFFER_ALIGNMENT: u64 = 1 << 16;

pub(crate) fn priority() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RF_X1_PRIORITY").is_some_and(|v| v == "1"))
}

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| priority() || std::env::var_os("VICTORY_RF_X1_STATS").is_some_and(|v| v == "1"))
}

pub(crate) fn reserve_total() -> u64 {
    static TOTAL: OnceLock<u64> = OnceLock::new();
    *TOTAL.get_or_init(|| {
        if !priority() || !crate::shadow::cards() {
            return 0;
        }
        std::env::var("VICTORY_RF_X1_RESERVE_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(3072)
            .saturating_mul(MIB)
    })
}

static CLAIMED: AtomicU64 = AtomicU64::new(0);
static PLACED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn reserve_unused() -> u64 {
    reserve_total().saturating_sub(PLACED.load(Relaxed))
}

/// The reservation follows the shader-facing allocation until the shadow is dropped.
pub(crate) struct Reservation<'a> {
    claimed: &'a AtomicU64,
    bytes: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.claimed.fetch_sub(self.bytes, Relaxed);
    }
}

pub(crate) fn reserve(size: u64, memory: Option<[u64; 4]>) -> Option<Reservation<'static>> {
    let bytes = size.checked_next_multiple_of(BUFFER_ALIGNMENT)?;
    // Use the full reported headroom, without the legacy fractional guard.
    if !fits_device_budget(bytes, memory) {
        return None;
    }
    claim(&CLAIMED, reserve_total(), bytes)
}

fn fits_device_budget(bytes: u64, memory: Option<[u64; 4]>) -> bool {
    memory.is_none_or(|[budget, used, ..]| bytes <= budget.saturating_sub(used))
}

fn claim(claimed: &AtomicU64, total: u64, bytes: u64) -> Option<Reservation<'_>> {
    claimed
        .fetch_update(Relaxed, Relaxed, |used| used.checked_add(bytes).filter(|&sum| sum <= total))
        .ok()?;
    Some(Reservation { claimed, bytes })
}

#[derive(Clone, Copy)]
pub(crate) enum ShadowKind {
    Buffer = 0,
    Window = 1,
}

struct Live {
    count: AtomicU64,
    bytes: AtomicU64,
}

impl Live {
    const fn new() -> Self {
        Self { count: AtomicU64::new(0), bytes: AtomicU64::new(0) }
    }

    fn add(&self, bytes: u64) {
        self.count.fetch_add(1, Relaxed);
        self.bytes.fetch_add(bytes, Relaxed);
    }

    fn remove(&self, bytes: u64) {
        self.count.fetch_sub(1, Relaxed);
        self.bytes.fetch_sub(bytes, Relaxed);
    }

    fn census(&self) -> (u64, u64) {
        (self.count.load(Relaxed), self.bytes.load(Relaxed) / MIB)
    }
}

/// For each kind, the first slot is system memory and the second is device-local.
static LIVE: [[Live; 2]; 2] = [const { [const { Live::new() }; 2] }; 2];
static REFUSED: [[AtomicU64; 2]; 2] = [const { [const { AtomicU64::new(0) }; 2] }; 2];

pub(crate) fn refused(kind: ShadowKind, allocation_failed: bool) {
    if enabled() {
        REFUSED[kind as usize][usize::from(allocation_failed)].fetch_add(1, Relaxed);
    }
}

pub(crate) struct ShadowAllocation {
    live: &'static Live,
    bytes: u64,
    reservation: Option<Reservation<'static>>,
}

impl ShadowAllocation {
    pub(crate) fn new(
        kind: ShadowKind,
        local: bool,
        bytes: u64,
        reservation: Option<Reservation<'static>>,
    ) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let live = &LIVE[kind as usize][usize::from(local)];
        let bytes = bytes.checked_next_multiple_of(BUFFER_ALIGNMENT)?;
        live.add(bytes);
        // Only reduce what is charged to the texture queue after allocation succeeds.
        if let Some(reservation) = &reservation {
            PLACED.fetch_add(reservation.bytes, Relaxed);
        }
        Some(Self { live, bytes, reservation })
    }
}

impl Drop for ShadowAllocation {
    fn drop(&mut self) {
        self.live.remove(self.bytes);
        if let Some(reservation) = &self.reservation {
            PLACED.fetch_sub(reservation.bytes, Relaxed);
        }
    }
}

/// Paired title and host footprints. Tiled texture backing is counted by `tiles`.
static TEXTURES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(crate) struct TextureFootprint {
    guest: Option<u64>,
    host: u64,
}

impl TextureFootprint {
    pub(crate) fn new(guest: Option<u64>, host: u64) -> Self {
        if let Some(guest) = guest {
            TEXTURES[0].fetch_add(guest, Relaxed);
            TEXTURES[1].fetch_add(host, Relaxed);
        } else {
            TEXTURES[2].fetch_add(1, Relaxed);
        }
        Self { guest, host }
    }
}

impl Drop for TextureFootprint {
    fn drop(&mut self) {
        if let Some(guest) = self.guest {
            TEXTURES[0].fetch_sub(guest, Relaxed);
            TEXTURES[1].fetch_sub(self.host, Relaxed);
        } else {
            TEXTURES[2].fetch_sub(1, Relaxed);
        }
    }
}

/// Called at the host's five-second texture queue log.
pub(crate) fn log(memory: Option<[u64; 4]>) {
    if !enabled() {
        return;
    }
    let [[(buffers_ram, buffers_ram_mb), (buffers_vram, buffers_vram_mb)],
         [(windows_ram, windows_ram_mb), (windows_vram, windows_vram_mb)]] =
        LIVE.each_ref().map(|row| row.each_ref().map(Live::census));
    let [[buffer_refused, buffer_failed], [window_refused, window_failed]] =
        REFUSED.each_ref().map(|row| row.each_ref().map(|n| n.load(Relaxed)));
    let [guest, host, unknown] = TEXTURES.each_ref().map(|n| n.load(Relaxed));
    let tiled = crate::tiles::given_total();
    let [texture_vram, sampled_vram] = crate::resource::rf_x1_texture_bytes(tiled);
    let [budget, used, _, shared] = memory.map_or([0; 4], |sample| sample.map(|bytes| bytes / MIB));
    let reserve_used = PLACED.load(Relaxed) / MIB;
    let reserve_total = reserve_total() / MIB;
    let guest_mb = guest / MIB;
    let host_mb = host.saturating_add(tiled) / MIB;
    let texture_vram = texture_vram / MIB;
    let sampled_vram = sampled_vram / MIB;
    let dxgi_available = memory.is_some();
    tracing::info!(
        "rf-x1: windows vram={windows_vram}/{windows_vram_mb} MB ram={windows_ram}/{windows_ram_mb} MB \
         refused={window_refused} alloc_failed={window_failed} retried=0; \
         buffers vram={buffers_vram}/{buffers_vram_mb} MB ram={buffers_ram}/{buffers_ram_mb} MB \
         refused={buffer_refused} alloc_failed={buffer_failed} retried=0; \
         textures vram={texture_vram} MB sampled={sampled_vram} MB guest={guest_mb} MB host={host_mb} MB unknown_guest={unknown}; \
         reserve={reserve_used}/{reserve_total} MB vram_in_use={used} MB vram_budget={budget} MB shared_with_card_in_use={shared} MB dxgi_available={dxgi_available}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_kinds_share_one_reservation_limit() {
        let used = AtomicU64::new(0);
        let _buffer = claim(&used, 3 * MIB, 2 * MIB).unwrap();
        assert!(claim(&used, 3 * MIB, 2 * MIB).is_none());
        let _window = claim(&used, 3 * MIB, MIB).unwrap();
        assert!(claim(&used, 3 * MIB, 1).is_none());
        assert_eq!(used.load(Relaxed), 3 * MIB);
    }

    #[test]
    fn fresh_attempt_can_use_released_reservation() {
        let used = AtomicU64::new(0);
        let first = claim(&used, MIB, MIB).unwrap();
        assert!(claim(&used, MIB, MIB).is_none());
        drop(first);
        assert_eq!(used.load(Relaxed), 0);
        assert!(claim(&used, MIB, MIB).is_some());
        assert_eq!(used.load(Relaxed), 0);
    }

    #[test]
    fn device_budget_is_a_ceiling_without_fractional_headroom() {
        let memory = Some([10 * MIB, 9 * MIB, 0, 0]);
        assert!(fits_device_budget(MIB, memory));
        assert!(!fits_device_budget(MIB + 1, memory));
        assert!(fits_device_budget(MIB, None));
    }

    #[test]
    fn live_accounting_keeps_only_live_bytes() {
        let live = Live::new();
        live.add(2 * MIB);
        live.add(MIB);
        assert_eq!(live.census(), (2, 3));
        live.remove(2 * MIB);
        assert_eq!(live.census(), (1, 1));
    }
}
