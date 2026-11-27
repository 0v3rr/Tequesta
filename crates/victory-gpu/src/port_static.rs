//! Static command-list recompile / bypass (`VICTORY_PORT_STATIC=1`, enabled by
//! default unless `VICTORY_PORT_STATIC=0`, toggleable live from the F1 menu).
//!
//! Tracks fast-path hits for:
//! - Full draw/dispatch bind bypass when PSO, root signature, heaps, epoch,
//!   depth state, root bindings, and descriptor table headers are unchanged.
//! - Constants-only rebind when only root 32-bit constants changed.
//! - Direct-table entry cache hits (`State::table_entry`).
//! - Contiguous `ExecuteIndirect` fusion in `list::thin`.
//! - HLE fence active-polling short-circuit (`fence::completed_called`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

static ENABLED: OnceLock<AtomicBool> = OnceLock::new();

fn flag() -> &'static AtomicBool {
    ENABLED.get_or_init(|| {
        let on = std::env::var_os("VICTORY_PORT_STATIC").is_none_or(|v| v != "0");
        AtomicBool::new(on)
    })
}

/// Whether `VICTORY_PORT_STATIC` is currently active.
#[inline]
pub fn on() -> bool {
    flag().load(Relaxed)
}

/// Toggles `VICTORY_PORT_STATIC` at runtime (used by the F1 overlay menu).
pub fn set_enabled(enabled: bool) {
    flag().store(enabled, Relaxed);
}

/// `[bind_full, bind_const, direct_hit, indirect_fused, fence_yield]`
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

#[inline]
pub(crate) fn note_bind_full() {
    STATS[0].fetch_add(1, Relaxed);
}

#[inline]
pub(crate) fn note_bind_const() {
    STATS[1].fetch_add(1, Relaxed);
}

#[inline]
pub(crate) fn note_direct_hit() {
    STATS[2].fetch_add(1, Relaxed);
}

#[inline]
pub(crate) fn note_indirect_fused(extra: u64) {
    STATS[3].fetch_add(extra, Relaxed);
}

#[inline]
pub(crate) fn note_fence_yield() {
    STATS[4].fetch_add(1, Relaxed);
}

/// Non-destructive snapshot of `[bind_full, bind_const, direct_hit, indirect_fused, fence_yield]`.
#[inline]
pub(crate) fn snapshot() -> [u64; 5] {
    [
        STATS[0].load(Relaxed),
        STATS[1].load(Relaxed),
        STATS[2].load(Relaxed),
        STATS[3].load(Relaxed),
        STATS[4].load(Relaxed),
    ]
}

/// Per-frame telemetry string when `perf_trace_on()` is active.
pub(crate) fn breakdown(frames: f64) -> String {
    let f = |i: usize| STATS[i].swap(0, Relaxed) as f64 / frames;
    format!(
        "port_static(on={} bind_full={:.0} bind_const={:.0} dir_hit={:.0} ind_fused={:.0} fence_yield={:.0})",
        on(),
        f(0),
        f(1),
        f(2),
        f(3),
        f(4),
    )
}
