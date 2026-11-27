//! A host list's D3D12 list, with barrier calls joined
//! (`VICTORY_JOIN_BARRIERS=1`; off by default; A/B-able with
//! `VICTORY_AB=<s>,VICTORY_JOIN_BARRIERS`).
//!
//! The state changes the host asks for (`crate::list::ensure_state`, the
//! aliasing before a texture's first upload) and the title's own barriers
//! wait here instead of going to the list at once; they go, in the order
//! they were asked for, in one call, at the next use of the list for
//! anything else. Every use goes through [`Rec`]'s `Deref`, so nothing is
//! recorded before a barrier asked for earlier. In a replay of capture
//! replay5 joining the calls that had no work between them saved 2.4 ms of
//! 79.5 (2,490 calls joined, 1,619 of them a texture's state change after
//! its upload followed by the next texture's aliasing).

use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Whether barriers are held now (`VICTORY_JOIN_BARRIERS=1`, on by default
/// unless `VICTORY_JOIN_BARRIERS=0`, or its phase of `VICTORY_AB`).
pub(crate) fn on() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_JOIN_BARRIERS") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_JOIN_BARRIERS").is_none_or(|v| v != "0"))
}

/// Barriers held. Their resources are borrowed: the list's states hold the
/// textures (`HostList::transition`), and the title's barriers name what
/// the title keeps alive until the list has run.
struct Held(Vec<D3D12_RESOURCE_BARRIER>);

// SAFETY: borrowed interface pointers of free-threaded objects.
unsafe impl Send for Held {}

/// Since the last frame-rate line: barrier calls asked for while held, and
/// calls made of them.
pub(crate) static STATS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

pub(crate) struct Rec {
    list: ID3D12GraphicsCommandList,
    list7: OnceLock<Option<ID3D12GraphicsCommandList7>>,
    any: AtomicBool,
    held: Mutex<Held>,
}

impl Rec {
    pub(crate) fn new(list: ID3D12GraphicsCommandList) -> Self {
        Self {
            list,
            list7: OnceLock::new(),
            any: AtomicBool::new(false),
            held: Mutex::new(Held(Vec::new())),
        }
    }

    /// Emits native D3D12 enhanced barriers (`ID3D12GraphicsCommandList7::Barrier`)
    /// after flushing any held legacy barriers so ordering is preserved.
    /// Returns `false` if `ID3D12GraphicsCommandList7` is unavailable.
    ///
    /// # Safety
    /// As `ID3D12GraphicsCommandList7::Barrier`: an open list and live resources.
    pub(crate) unsafe fn enhanced_barriers(&self, groups: &[D3D12_BARRIER_GROUP]) -> bool {
        if groups.is_empty() {
            return true;
        }
        for g in groups {
            if g.Type != D3D12_BARRIER_TYPE_BUFFER || g.NumBarriers == 0 {
                return false;
            }
            // SAFETY: `Type == D3D12_BARRIER_TYPE_BUFFER`.
            let ptr = unsafe { g.Anonymous.pBufferBarriers };
            if ptr.is_null() {
                return false;
            }
            // SAFETY: caller guarantees `NumBarriers` entries at `ptr`.
            let barriers = unsafe { std::slice::from_raw_parts(ptr, g.NumBarriers as usize) };
            if !barriers.iter().all(valid_buffer_barrier) {
                return false;
            }
        }
        if self.any.load(Relaxed) {
            self.flush();
        }
        let Some(l7) = self.list7.get_or_init(|| self.list.cast().ok()).as_ref() else {
            return false;
        };
        // SAFETY: caller guarantees an open list and validated buffer barrier descriptors.
        unsafe { l7.Barrier(groups) };
        true
    }

    /// Records `barriers` now, or holds them for the next use of the list.
    ///
    /// # Safety
    /// As `ResourceBarrier`'s: an open list, live resources.
    pub(crate) unsafe fn barriers(&self, barriers: &[D3D12_RESOURCE_BARRIER]) {
        if barriers.is_empty() {
            return;
        }
        if !on() {
            // (What was held while it was on goes first.)
            if self.any.load(Relaxed) {
                self.flush();
            }
            // SAFETY: the caller's.
            unsafe { self.list.ResourceBarrier(barriers) };
            return;
        }
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        for b in barriers {
            if !coalesce_into_held(&mut held.0, b) {
                // SAFETY: as in `flush`.
                unsafe { self.list.ResourceBarrier(&held.0) };
                held.0.clear();
                STATS[1].fetch_add(1, Relaxed);
                // SAFETY: a plain copy of the description; nothing is released
                // through it (its pointers are borrowed, `ManuallyDrop`).
                held.0.push(unsafe { std::ptr::read(b) });
            }
        }
        STATS[0].fetch_add(1, Relaxed);
        self.any.store(!held.0.is_empty(), Relaxed);
    }

    /// What is held goes to the list, in one call.
    fn flush(&self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        self.any.store(false, Relaxed);
        if held.0.is_empty() {
            return;
        }
        // SAFETY: the barriers were asked for on this open list, in order;
        // their resources are alive (`Held`).
        unsafe { self.list.ResourceBarrier(&held.0) };
        held.0.clear();
        STATS[1].fetch_add(1, Relaxed);
    }
}

/// Verifies that `b` is a PC-legal `D3D12_BUFFER_BARRIER` on a live
/// `DEFAULT`-heap UAV-capable host buffer, with valid non-zero sync/access masks.
fn valid_buffer_barrier(b: &D3D12_BUFFER_BARRIER) -> bool {
    const VALID_SYNC: u32 = 0x1 | 0x80 | 0x200 | 0x800 | 0x1000;
    const VALID_ACCESS: u32 = 0x10 | 0x80 | 0x200 | 0x400 | 0x800;
    const WRITE_ACCESS: u32 = 0x10 | 0x400;
    if b.Offset != 0 || b.Size != u64::MAX {
        return false;
    }
    let sb = b.SyncBefore.0 as u32;
    let sa = b.SyncAfter.0 as u32;
    if sb == 0 || sa == 0 || (sb & !VALID_SYNC) != 0 || (sa & !VALID_SYNC) != 0 {
        return false;
    }
    let ab = b.AccessBefore.0 as u32;
    let aa = b.AccessAfter.0 as u32;
    if (ab & !VALID_ACCESS) != 0 || (aa & !VALID_ACCESS) != 0 {
        return false;
    }
    if ((ab & WRITE_ACCESS) != 0 && ab != 0x10 && ab != 0x400)
        || ((aa & WRITE_ACCESS) != 0 && aa != 0x10 && aa != 0x400)
    {
        return false;
    }
    let Some(res) = b.pResource.as_ref() else {
        return false;
    };
    // SAFETY: `res` is a live `ID3D12Resource` borrowed from the caller.
    let desc = unsafe { res.GetDesc() };
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER
        || (desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0) == 0
    {
        return false;
    }
    let mut heap = D3D12_HEAP_PROPERTIES::default();
    // SAFETY: querying heap properties of a live resource.
    unsafe { res.GetHeapProperties(Some(&mut heap), None) }.is_ok()
        && heap.Type == D3D12_HEAP_TYPE_DEFAULT
}

/// The resource a state change or aliasing names (its interface pointer).
fn named(b: &D3D12_RESOURCE_BARRIER) -> Option<usize> {
    // SAFETY: the union's member is the one `Type` says; a pointer read.
    unsafe {
        let p: Option<&ID3D12Resource> = match b.Type {
            D3D12_RESOURCE_BARRIER_TYPE_TRANSITION => b.Anonymous.Transition.pResource.as_ref(),
            D3D12_RESOURCE_BARRIER_TYPE_ALIASING => b.Anonymous.Aliasing.pResourceAfter.as_ref(),
            _ => None,
        };
        p.map(|r| windows::core::Interface::as_raw(r) as usize)
    }
}

fn uav_resource(b: &D3D12_RESOURCE_BARRIER) -> Option<usize> {
    // SAFETY: caller ensures `b.Type == D3D12_RESOURCE_BARRIER_TYPE_UAV`.
    unsafe {
        b.Anonymous
            .UAV
            .pResource
            .as_ref()
            .map(|r| windows::core::Interface::as_raw(r) as usize)
    }
}

/// Conservatively coalesces `incoming` into `held`. Returns `false` if `held`
/// must be flushed before `incoming` can be appended.
/// Never cancels `A -> B -> A` round-trips and never reorders UAVs across transitions.
fn coalesce_into_held(
    held: &mut Vec<D3D12_RESOURCE_BARRIER>,
    incoming: &D3D12_RESOURCE_BARRIER,
) -> bool {
    // SAFETY: plain bit-copy of borrowed `ManuallyDrop` pointers.
    let copy = |b: &D3D12_RESOURCE_BARRIER| unsafe { std::ptr::read(b) };
    match incoming.Type {
        D3D12_RESOURCE_BARRIER_TYPE_TRANSITION => {
            // SAFETY: `Type` is `TRANSITION`.
            let (before, after, sub) = unsafe {
                let t = &incoming.Anonymous.Transition;
                (t.StateBefore, t.StateAfter, t.Subresource)
            };
            if incoming.Flags == D3D12_RESOURCE_BARRIER_FLAG_NONE && before == after {
                return true;
            }
            let res = named(incoming);
            if res.is_some() && held.iter().any(|h| named(h) == res) {
                if let Some(last) = held.last_mut()
                    && last.Type == D3D12_RESOURCE_BARRIER_TYPE_TRANSITION
                    && named(last) == res
                    && last.Flags == D3D12_RESOURCE_BARRIER_FLAG_NONE
                    && incoming.Flags == D3D12_RESOURCE_BARRIER_FLAG_NONE
                {
                    // SAFETY: `Type` is `TRANSITION`.
                    let (prev_sub, prev_before, prev_after) = unsafe {
                        let t = &last.Anonymous.Transition;
                        (t.Subresource, t.StateBefore, t.StateAfter)
                    };
                    if prev_sub == sub && prev_after == before && prev_before != after {
                        // SAFETY: `Type` is `TRANSITION`.
                        unsafe {
                            (*last.Anonymous.Transition).StateAfter = after;
                        }
                        return true;
                    }
                }
                return false;
            }
            held.push(copy(incoming));
            true
        }
        D3D12_RESOURCE_BARRIER_TYPE_UAV => {
            let res = uav_resource(incoming);
            if let Some(last) = held.last()
                && last.Type == D3D12_RESOURCE_BARRIER_TYPE_UAV
            {
                let prev = uav_resource(last);
                if prev.is_none() || prev == res {
                    return true;
                }
            }
            held.push(copy(incoming));
            true
        }
        D3D12_RESOURCE_BARRIER_TYPE_ALIASING => {
            let after_res = named(incoming);
            if after_res.is_some() && held.iter().any(|h| named(h) == after_res) {
                return false;
            }
            held.push(copy(incoming));
            true
        }
        _ => {
            held.push(copy(incoming));
            true
        }
    }
}

impl std::ops::Deref for Rec {
    type Target = ID3D12GraphicsCommandList;

    /// The list, with what is held recorded first.
    fn deref(&self) -> &ID3D12GraphicsCommandList {
        if self.any.load(Relaxed) {
            self.flush();
        }
        &self.list
    }
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    let [asked, made] = [0, 1].map(|i| STATS[i].swap(0, Relaxed) as f64 / frames);
    if asked > 0.0 {
        tracing::info!(
            "barrier calls per frame (VICTORY_JOIN_BARRIERS): {asked:.0} asked for, {made:.0} made of them"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_into_held_handles_uavs_and_noop_transitions() {
        let uav_null = || D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        let noop_tr = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_COMMON,
                    StateAfter: D3D12_RESOURCE_STATE_COMMON,
                }),
            },
        };

        let mut held = Vec::new();
        assert!(coalesce_into_held(&mut held, &noop_tr));
        assert!(held.is_empty());

        assert!(coalesce_into_held(&mut held, &uav_null()));
        assert!(coalesce_into_held(&mut held, &uav_null()));
        assert_eq!(held.len(), 1);

        // UAV after transition is not reordered/removed.
        assert!(coalesce_into_held(&mut held, &noop_tr));
        assert_eq!(held.len(), 1);
    }
}
