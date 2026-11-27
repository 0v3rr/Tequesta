//! `gameinput.dll`: input devices from the host. Gamepads come from XInput
//! (host slots 0-3); while none is connected, the keyboard drives one
//! stand-in gamepad (`FALLBACK_BUTTONS`). The keyboard is a device of its own
//! too. A poll thread turns host state into readings (a new one per change)
//! and runs device callbacks on connect and disconnect.
#![allow(non_snake_case)]
// Title code calls these through the objects, passing pointers the real API
// would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleExport, HleModule};

/// The documented errors.
pub const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
pub const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
pub const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
pub const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;

/// Input kinds (bits) our devices have.
const KEYBOARD_KIND: u32 = 0x10;
const GAMEPAD_KIND: u32 = 0x4_0000;
/// Device status of a connected device: connected, input enabled.
const STATUS_CONNECTED: u32 = 0x3;
const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad buttons; the title maps each bit to one of its pad buttons.
const MENU: u32 = 0x1;
const VIEW: u32 = 0x2;
const A: u32 = 0x4;
const B: u32 = 0x8;
const X: u32 = 0x10;
const Y: u32 = 0x20;
const UP: u32 = 0x40;
const DOWN: u32 = 0x80;
const LEFT: u32 = 0x100;
const RIGHT: u32 = 0x200;
const LEFT_SHOULDER: u32 = 0x400;
const RIGHT_SHOULDER: u32 = 0x800;
const LEFT_STICK: u32 = 0x1000;
const RIGHT_STICK: u32 = 0x2000;

/// Devices: host pad slots, the keyboard's stand-in pad, the keyboard.
const PADS: usize = 4;
const FALLBACK: usize = PADS;
const KEYBOARD: usize = PADS + 1;
const DEVICES: usize = PADS + 2;

/// Readings kept per device for next/previous lookups.
const HISTORY: usize = 256;
const POLL: Duration = Duration::from_millis(8);

/// The keyboard's stand-in gamepad (virtual key, button); README lists it.
/// Sticks: W/A/S/D left, I/J/K/L right; triggers Z and C.
const FALLBACK_BUTTONS: [(u8, u32); 14] = [
    (0x1B, MENU), // Esc
    (0x09, VIEW), // Tab
    (0x0D, A),    // Enter
    (0x08, B),    // Backspace
    (b'X', X),
    (b'Y', Y),
    (0x26, UP), // arrows
    (0x28, DOWN),
    (0x25, LEFT),
    (0x27, RIGHT),
    (b'Q', LEFT_SHOULDER),
    (b'E', RIGHT_SHOULDER),
    (b'F', LEFT_STICK),
    (b'H', RIGHT_STICK),
];

/// Opt-in GTA V PC-style keyboard bindings for the stand-in gamepad
/// (`VICTORY_KBM_GTA=1`). Left stick stays W/A/S/D; right stick is driven by
/// mouse look (toggled with F2 or `` ` `` / `~`) plus I/J/K/L fallback;
/// triggers come from LMB (RT) and RMB (LT). `Host::keys()` reports specific
/// left/right modifiers (`0xA0..=0xA5`) rather than generic `0x10..=0x12`.
const GTA_BUTTONS: [(u8, u32); 25] = [
    (0x1B, MENU),           // Esc (also unlocks mouse look)
    (b'P', MENU),           // Pause / Map
    (b'V', VIEW),           // Change camera perspective
    (0xA0, A),              // Left Shift: Sprint
    (0xA1, A),              // Right Shift: Sprint
    (0x0D, A),              // Enter: Confirm (menus)
    (0x20, X),              // Space: Jump / Climb / Vault
    (b'X', X),              // X fallback
    (b'R', B),              // Reload / Melee
    (0x08, B),              // Backspace: Back / Cancel (menus)
    (b'F', Y),              // Enter / Exit vehicle
    (0x09, LEFT_SHOULDER),  // Tab: Weapon wheel (LB)
    // Cover/handbrake (RB) uses scancode 0x10 (key left of Z/W) to avoid AZERTY Q strafe-left conflict
    (0xA2, LEFT_STICK),     // Left Ctrl: Crouch / Stealth / Horn (L3)
    (0xA3, LEFT_STICK),     // Right Ctrl: Crouch / Stealth / Horn (L3)
    (b'C', RIGHT_STICK),    // C: Look behind / Special (R3)
    (0x14, RIGHT_STICK),    // Caps Lock: Special (R3)
    (b'H', RIGHT_STICK),    // H fallback (R3)
    (b'E', RIGHT),          // E: Interact / Talk (D-pad Right)
    (0x27, RIGHT),          // Right arrow (D-pad Right)
    (0x26, UP),             // Up arrow: Phone (D-pad Up)
    (b'M', UP),             // M: Phone / Menu (D-pad Up)
    (0x28, DOWN),           // Down arrow: Character wheel / Minimap (D-pad Down)
    (b'Z', DOWN),           // Z: Expand minimap / Character wheel (D-pad Down)
    (0x25, LEFT),           // Left arrow (D-pad Left)
    (b'G', LEFT),           // G: Throw / Context left (D-pad Left)
];

/// Whether GTA mouse-look lock (`~` / `F2`) is currently active; read by
/// `lifecycle` on `WM_SETCURSOR` to auto-hide the Windows cursor inside the
/// game window's client area while locked.
#[cfg(windows)]
static CURSOR_LOCKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
static RAW_MOUSE_DX: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
#[cfg(windows)]
static RAW_MOUSE_DY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
#[cfg(windows)]
static RAW_MOUSE_LAST_US: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
static RAW_PUMP_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Returns true when the input poll thread's dedicated `RIDEV_INPUTSINK` raw
/// mouse pump is active so `lifecycle::window_proc` does not double-count.
#[cfg(windows)]
pub(crate) fn raw_pump_active() -> bool {
    RAW_PUMP_ACTIVE.load(Ordering::Relaxed)
}

/// Accumulates relative mouse deltas from `WM_INPUT` (Raw Input) while mouse-look
/// is locked and the `F1` overlay menu is closed.
#[cfg(windows)]
pub(crate) fn push_raw_mouse_delta(dx: i32, dy: i32) {
    RAW_MOUSE_LAST_US.store(now(), Ordering::Relaxed);
    if CURSOR_LOCKED.load(Ordering::Relaxed) && !victory_gpu::tequesta_ui::menu_open() {
        RAW_MOUSE_DX.fetch_add(dx, Ordering::Relaxed);
        RAW_MOUSE_DY.fetch_add(dy, Ordering::Relaxed);
    }
}

#[cfg(windows)]
fn raw_mouse_recent(now_us: u64) -> bool {
    let last = RAW_MOUSE_LAST_US.load(Ordering::Relaxed);
    last != 0 && now_us.saturating_sub(last) < 250_000
}

/// Returns true while GTA mouse-look lock (`~` / `F2`) is active and the
/// cursor should be hidden inside the client area.
#[cfg(windows)]
pub(crate) fn cursor_locked() -> bool {
    CURSOR_LOCKED.load(Ordering::Relaxed) && !victory_gpu::tequesta_ui::menu_open()
}

/// Overlay hotkeys (`F1` menu, `F3` HUD mode, `F4` static cache, `F5` resolution scale)
/// are consumed by `tequesta_ui` without leaking to the game, while `F2` (`0x71`) and
/// `` ` `` / `~` (`0xC0`) remain untouched for mouse-look capture.
#[inline]
pub(crate) fn is_overlay_hotkey_vk(vk: u8) -> bool {
    matches!(vk, 0x70 | 0x72 | 0x73 | 0x74)
}

/// Parses an opt-in boolean environment variable (`1`, `true`, `yes`, `on`,
/// ignoring surrounding whitespace and ASCII case).
fn parse_env_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Native Raw Input keyboard/mouse (`VICTORY_KBM_NATIVE=1`).
fn native_kbm() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        if let Ok(v) = std::env::var("VICTORY_KBM_NATIVE") {
            return parse_env_flag(&v);
        }
        #[cfg(all(windows, not(test)))]
        {
            victory_gpu::tequesta_ui::kbm_native_enabled()
        }
        #[cfg(any(not(windows), test))]
        {
            false
        }
    });
    *ENABLED
}

/// GTA V PC-style keyboard + mouse stand-in pad (`VICTORY_KBM_GTA=1` or `VICTORY_KBM_NATIVE=1`).
/// Off by default in unit tests unless enabled so existing keyboard table tests stay unchanged.
fn gta_kbm() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        if let Ok(v) = std::env::var("VICTORY_KBM_GTA") {
            return parse_env_flag(&v);
        }
        if let Ok(v) = std::env::var("VICTORY_KBM_NATIVE") {
            return parse_env_flag(&v);
        }
        #[cfg(all(windows, not(test)))]
        {
            victory_gpu::tequesta_ui::kbm_native_enabled()
        }
        #[cfg(any(not(windows), test))]
        {
            false
        }
    });
    *ENABLED
}

/// Mouse-look sensitivity multiplier for `VICTORY_KBM_GTA=1` / `VICTORY_KBM_NATIVE=1`
/// (live-editable from the F1 menu, seeded from `VICTORY_MOUSE_SENS`).
#[cfg(windows)]
fn mouse_sens() -> f32 {
    victory_gpu::tequesta_ui::mouse_sensitivity()
}

/// Invert mouse Y axis for `VICTORY_KBM_GTA=1` (`VICTORY_MOUSE_INVERT_Y=1`).
#[cfg(windows)]
fn mouse_invert_y() -> bool {
    static INVERT: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_MOUSE_INVERT_Y").is_ok_and(|v| parse_env_flag(&v)));
    *INVERT
}

/// Continuous rate-normalized mouse-delta to right-stick velocity converter.
///
/// Converts per-poll mouse deltas into a velocity rate (`counts / sec`),
/// applies an asymmetric EMA (`ATTACK_TAU` rise, `RELEASE_TAU` release) so
/// slow/continuous pans never stutter step-by-step and flicks bridge 15–27 FPS
/// frames smoothly, then maps magnitude through a $C^1$ smoothstep
/// anti-deadzone ramp.
#[cfg(any(windows, test))]
#[derive(Default)]
struct MouseLook {
    vx: f32,
    vy: f32,
}

#[cfg(any(windows, test))]
impl MouseLook {
    const COUNTS_FOR_FULL: f32 = 950.0;
    const ATTACK_TAU: f32 = 0.036;
    const RELEASE_TAU: f32 = 0.055;
    const REST_EPS: f32 = 0.008;
    const ANTI_DEADZONE: f32 = 0.14;

    fn reset(&mut self) {
        self.vx = 0.0;
        self.vy = 0.0;
    }

    fn step(&mut self, dx: i32, dy: i32, dt: Duration, sens: f32, invert_y: bool) -> (f32, f32) {
        let dt_secs = dt.as_secs_f32().clamp(0.001, 0.25);
        let y_sign = if invert_y { 1.0 } else { -1.0 };
        let gain = sens.max(0.05) / (Self::COUNTS_FOR_FULL * dt_secs);
        let mut target_x = dx as f32 * gain;
        let mut target_y = dy as f32 * gain * y_sign;
        let target_mag = (target_x * target_x + target_y * target_y).sqrt();
        if target_mag > 1.0 {
            target_x /= target_mag;
            target_y /= target_mag;
        }
        let tau_x = if target_x.abs() > self.vx.abs() {
            Self::ATTACK_TAU
        } else {
            Self::RELEASE_TAU
        };
        let tau_y = if target_y.abs() > self.vy.abs() {
            Self::ATTACK_TAU
        } else {
            Self::RELEASE_TAU
        };
        let alpha_x = 1.0 - (-dt_secs / tau_x).exp();
        let alpha_y = 1.0 - (-dt_secs / tau_y).exp();
        self.vx = (self.vx + (target_x - self.vx) * alpha_x).clamp(-1.0, 1.0);
        self.vy = (self.vy + (target_y - self.vy) * alpha_y).clamp(-1.0, 1.0);

        if dx == 0 && self.vx.abs() < Self::REST_EPS {
            self.vx = 0.0;
        }
        if dy == 0 && self.vy.abs() < Self::REST_EPS {
            self.vy = 0.0;
        }
        let mag = (self.vx * self.vx + self.vy * self.vy).sqrt().min(1.0);
        if mag <= 1e-4 {
            return (0.0, 0.0);
        }
        let u = (mag / 0.22).min(1.0);
        let ramp = u * u * (3.0 - 2.0 * u);
        let boosted = (mag + Self::ANTI_DEADZONE * ramp * (1.0 - mag)).min(1.0);
        (
            (self.vx / mag * boosted).clamp(-1.0, 1.0),
            (self.vy / mag * boosted).clamp(-1.0, 1.0),
        )
    }
}

/// Edge-captured state for `[LMB, RMB, MMB]` so holding a button on window
/// chrome or in another app and dragging into the client area never fires
/// triggers or buttons.
#[cfg(any(windows, test))]
#[derive(Default)]
struct MouseButtons {
    seeded: bool,
    prev: [bool; 3],
    captured: [bool; 3],
}

#[cfg(any(windows, test))]
impl MouseButtons {
    fn reset(&mut self) {
        self.seeded = false;
        self.prev = [false; 3];
        self.captured = [false; 3];
    }

    fn update(&mut self, down: [bool; 3], eligible: bool) -> [bool; 3] {
        if !self.seeded {
            self.seeded = true;
            self.prev = down;
            self.captured = [false; 3];
            return self.captured;
        }
        for ((&is_down, prev), captured) in down.iter().zip(&mut self.prev).zip(&mut self.captured) {
            if !is_down {
                *captured = false;
            } else if !*prev && eligible {
                *captured = true;
            }
            *prev = is_down;
        }
        self.captured
    }
}

/// Mouse-look capture state machine: auto-engages when focused and the `F1`
/// menu is closed, yields on focus loss / `F1` open without permanently pausing,
/// and respects explicit `F2` / `` ` `` / `~` toggles and `Esc` / `P` unlocks.
#[cfg(any(windows, test))]
#[derive(Default)]
struct MouseCapture {
    locked: bool,
    user_paused: bool,
    just_locked: bool,
    toggle_was_down: bool,
    unlock_was_down: bool,
}

#[cfg(any(windows, test))]
impl MouseCapture {
    fn step(
        &mut self,
        active_focus: bool,
        menu_open: bool,
        toggle_down: bool,
        unlock_down: bool,
    ) -> (bool, bool) {
        if !active_focus || menu_open {
            self.locked = false;
            self.just_locked = false;
            self.toggle_was_down = toggle_down;
            self.unlock_was_down = unlock_down;
            return (false, false);
        }

        let toggle_edge = toggle_down && !self.toggle_was_down;
        let unlock_edge = unlock_down && !self.unlock_was_down;
        self.toggle_was_down = toggle_down;
        self.unlock_was_down = unlock_down;

        if toggle_edge {
            if self.locked {
                self.user_paused = true;
                self.locked = false;
                self.just_locked = false;
            } else {
                self.user_paused = false;
                self.locked = true;
                self.just_locked = true;
            }
        } else if unlock_edge && self.locked {
            self.user_paused = true;
            self.locked = false;
            self.just_locked = false;
        } else if !self.locked && !self.user_paused {
            self.locked = true;
            self.just_locked = true;
        }

        let jl = std::mem::take(&mut self.just_locked);
        (self.locked, jl)
    }
}

/// A gamepad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PadState {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

/// One key held down: scan code (0xE0xx when extended), character, key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct KeyState {
    scan_code: u32,
    code_point: u32,
    virtual_key: u8,
    dead_key: bool,
}

const _: () = assert!(size_of::<KeyState>() == 12 && size_of::<PadState>() == 28);

/// Host input, or a fake in tests.
trait Source: Send {
    /// The pad in host slot `slot`, `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down; none while the title's window lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    /// Motor speeds: low frequency (left), high frequency (right).
    fn rumble(&mut self, slot: usize, low: u16, high: u16);
    /// Extra stand-in pad input from the mouse when `VICTORY_KBM_GTA=1`.
    /// When `active` is false (a physical controller owns input), any active
    /// mouse-look capture must be released without warping the cursor.
    fn mouse_pad(&mut self, _active: bool) -> PadState {
        PadState::default()
    }
}

fn is_hw_scancode(sc: u32) -> bool {
    (1..0x100).contains(&sc)
}

fn fallback(keys: &[KeyState]) -> PadState {
    fallback_for(keys, gta_kbm())
}

fn fallback_for(keys: &[KeyState], gta: bool) -> PadState {
    fallback_for_mode(keys, gta, native_kbm())
}

fn fallback_for_mode(keys: &[KeyState], gta: bool, native: bool) -> PadState {
    let on = |vk: u8| {
        if keys.iter().any(|k| k.virtual_key == vk) {
            1.0
        } else {
            0.0
        }
    };
    let on_phys = |sc: u32, vk_fallback: u8| {
        if keys
            .iter()
            .any(|k| k.scan_code == sc || (!is_hw_scancode(k.scan_code) && k.virtual_key == vk_fallback))
        {
            1.0
        } else {
            0.0
        }
    };
    let mut buttons = 0u32;
    let table: &[(u8, u32)] = if gta { &GTA_BUTTONS } else { &FALLBACK_BUTTONS };
    for &(vk, bit) in table {
        let pressed = if gta && native && vk == b'Z' {
            // On AZERTY, physical scancode 0x11 is `Z` (forward), whereas
            // physical scancode 0x2C is bottom-left (`W` on AZERTY / `Z` on QWERTY).
            keys.iter()
                .any(|k| k.scan_code == 0x2C || (k.virtual_key == b'Z' && k.scan_code != 0x11))
        } else {
            on(vk) != 0.0
        };
        if pressed {
            buttons |= bit;
        }
    }
    if gta {
        // Cover/handbrake (RB): scancode 0x10 (Q on QWERTY, A on AZERTY).
        let cover_down = if native {
            keys.iter()
                .any(|k| k.scan_code == 0x10 || (!is_hw_scancode(k.scan_code) && k.virtual_key == b'Q'))
        } else {
            keys.iter()
                .any(|k| k.scan_code == 0x10 || k.virtual_key == b'Q')
        };
        if cover_down {
            buttons |= RIGHT_SHOULDER;
        }
    }
    if gta && native {
        // Layout-independent AZERTY (ZQSD) + QWERTY (WASD) scancode movement.
        let left_y = on_phys(0x11, b'W') - on_phys(0x1F, b'S');
        let left_x = on_phys(0x20, b'D') - on_phys(0x1E, b'A');
        return PadState {
            buttons,
            left_trigger: 0.0,
            right_trigger: 0.0,
            left_x,
            left_y,
            right_x: on(b'L') - on(b'J'),
            right_y: on(b'I') - on(b'K'),
        };
    }
    PadState {
        buttons,
        left_trigger: if gta { 0.0 } else { on(b'Z') },
        right_trigger: if gta { 0.0 } else { on(b'C') },
        left_x: on(b'D') - on(b'A'),
        left_y: on(b'W') - on(b'S'),
        right_x: on(b'L') - on(b'J'),
        right_y: on(b'I') - on(b'K'),
    }
}

/// Two pads as one: buttons of both, and of each axis and trigger the one
/// pushed further.
fn merged(a: PadState, b: PadState) -> PadState {
    let further = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: further(a.left_trigger, b.left_trigger),
        right_trigger: further(a.right_trigger, b.right_trigger),
        left_x: further(a.left_x, b.left_x),
        left_y: further(a.left_y, b.left_y),
        right_x: further(a.right_x, b.right_x),
        right_y: further(a.right_y, b.right_y),
    }
}

/// Whether the title sees one pad that is always there, fed by the keyboard
/// and by every host pad (on by default; `VICTORY_ONE_PAD=0` shows each host
/// pad as a device of its own, and the stand-in only while there is none).
/// The title keeps to the pad it started with: with the stand-in taken away
/// when a pad was switched on, it showed its controller-disconnected screen
/// until the pad was off again. It also keeps a pad the host lists twice (its
/// own slot and a remapper's) from being two players.
fn one_pad() -> bool {
    std::env::var_os("VICTORY_ONE_PAD").is_none_or(|v| v != "0")
}

#[repr(C)]
struct Table<const N: usize>([*const (); N]);
// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Table<N> {}

/// The input object; one per hub.
#[repr(C)]
pub struct Object {
    vtable: &'static Table<22>,
    hub: *const Hub,
}

#[repr(C)]
pub struct Device {
    vtable: &'static Table<21>,
    hub: *const Hub,
    index: usize,
    info: DeviceInfo,
}

/// What a device says about itself. The title reads `id` (+0x20); the rest
/// follows the public layout and is zero where we have nothing to say.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by title code
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers.
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

fn kind(index: usize) -> u32 {
    if index == KEYBOARD {
        KEYBOARD_KIND
    } else {
        GAMEPAD_KIND
    }
}

impl Device {
    fn new(index: usize) -> Device {
        let mut id = [0; 32];
        id[..7].copy_from_slice(b"victory");
        id[31] = index as u8 + 1;
        let info = DeviceInfo {
            size: size_of::<DeviceInfo>() as u32,
            id,
            root_id: id,
            supported_input: kind(index),
            // Low and high frequency.
            rumble_motors: if index <= FALLBACK { 0x3 } else { 0 },
            ..Default::default()
        };
        Device {
            vtable: &DEVICE,
            hub: std::ptr::null(),
            index,
            info,
        }
    }

    fn hub(&self) -> &'static Hub {
        // SAFETY: set by `Hub::new`; hubs are never freed.
        unsafe { &*self.hub }
    }
}

#[derive(PartialEq)]
enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
}

#[repr(C)]
pub struct Reading {
    vtable: &'static Table<25>,
    refs: AtomicU32,
    device: *const Device,
    sequence: u64,
    timestamp: u64,
    value: Value,
}

impl Reading {
    fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => GAMEPAD_KIND,
            Value::Keys(_) => KEYBOARD_KIND,
        }
    }
}

/// A counted reference to a reading.
struct Ref(NonNull<Reading>);

impl Ref {
    fn new(reading: Reading) -> Ref {
        Ref(NonNull::from(Box::leak(Box::new(reading))))
    }

    /// Another reference, for the title.
    fn share(&self) -> *const Reading {
        reading_add_ref(self.0.as_ptr());
        self.0.as_ptr()
    }
}

impl Deref for Ref {
    type Target = Reading;
    fn deref(&self) -> &Reading {
        // SAFETY: we hold a reference, so it is alive.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0.as_ptr());
    }
}

type DeviceCallback = extern "system" fn(u64, *mut c_void, *const Device, u64, u32, u32);

#[derive(Clone, Copy)]
struct Registration {
    token: u64,
    /// One device's address, or 0 for any.
    device: usize,
    kind: u32,
    status: u32,
    context: usize,
    callback: DeviceCallback,
}

impl Registration {
    fn matches(&self, devices: &[Device; DEVICES], index: usize) -> bool {
        kind(index) & self.kind != 0
            && (self.device == 0 || self.device == &devices[index] as *const Device as usize)
    }

    fn event(&self, device: usize, timestamp: u64, current: u32, previous: u32) -> Event {
        Event {
            registration: *self,
            device,
            timestamp,
            current,
            previous,
        }
    }
}

struct Event {
    registration: Registration,
    device: usize,
    timestamp: u64,
    current: u32,
    previous: u32,
}

#[derive(Default)]
struct Slot {
    connected: bool,
    sequence: u64,
    history: VecDeque<Ref>,
}

struct State {
    slots: [Slot; DEVICES],
    callbacks: Vec<Registration>,
    /// Asynchronous enumerations, delivered by the next poll.
    pending: Vec<Event>,
    /// Device callbacks running now: token and thread.
    running: Vec<(u64, ThreadId)>,
}

pub struct Hub {
    object: Object,
    devices: [Device; DEVICES],
    /// Apart from the state: XInput can be slow, and the title reads
    /// readings every frame.
    source: Mutex<Box<dyn Source>>,
    /// The title sees one pad ([`one_pad`]).
    one_pad: bool,
    state: Mutex<State>,
    /// A device callback returned.
    returned: Condvar,
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// SAFETY: the back pointers are set before the hub is shared and never
// change; what changes sits behind the mutex or is atomic.
unsafe impl Send for Hub {}
// SAFETY: as above.
unsafe impl Sync for Hub {}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Microseconds, the clock of every timestamp we hand out.
fn now() -> u64 {
    START.elapsed().as_micros() as u64
}

impl Hub {
    fn new(source: Box<dyn Source>, one_pad: bool) -> &'static Hub {
        let hub = Box::into_raw(Box::new(Hub {
            object: Object {
                vtable: &INPUT,
                hub: std::ptr::null(),
            },
            devices: std::array::from_fn(Device::new),
            source: Mutex::new(source),
            one_pad,
            state: Mutex::new(State {
                slots: Default::default(),
                callbacks: Vec::new(),
                pending: Vec::new(),
                running: Vec::new(),
            }),
            returned: Condvar::new(),
        }));
        // SAFETY: just allocated and never freed; nothing else sees it yet.
        unsafe {
            (*hub).object.hub = hub;
            for device in &mut (*hub).devices {
                device.hub = hub;
            }
            &*hub
        }
    }

    /// A hub over the host's input, polled from now on.
    fn start(source: Box<dyn Source>) -> &'static Hub {
        let hub = Hub::new(source, one_pad());
        hub.poll();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                hub.poll();
            }
        });
        hub
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Reads the host once; device callbacks run on the calling thread.
    fn poll(&self) {
        let (pads, keys, mouse) = {
            let mut source = lock(&self.source);
            let pads: [_; PADS] = std::array::from_fn(|slot| source.pad(slot));
            let keys = source.keys();
            let stand_in_active = self.one_pad || pads.iter().all(Option::is_none);
            let mouse = source.mouse_pad(stand_in_active);
            (pads, keys, mouse)
        };
        let events = self
            .state()
            .poll(&self.devices, self.one_pad, pads, keys, mouse);
        self.deliver(events);
    }

    /// Runs callbacks, never under our lock: they call back into us. One
    /// unregistered since the event was taken does not run, and
    /// `UnregisterCallback` waits for one that runs.
    fn deliver(&self, events: Vec<Event>) {
        let me = std::thread::current().id();
        for e in events {
            let r = e.registration;
            {
                let mut state = self.state();
                if !state.callbacks.iter().any(|c| c.token == r.token) {
                    continue;
                }
                state.running.push((r.token, me));
            }
            (r.callback)(
                r.token,
                r.context as *mut c_void,
                &self.devices[e.device],
                e.timestamp,
                e.current,
                e.previous,
            );
            let mut state = self.state();
            if let Some(i) = state.running.iter().position(|&x| x == (r.token, me)) {
                state.running.swap_remove(i);
            }
            self.returned.notify_all();
        }
    }

    fn index(&self, device: *const Device) -> Option<usize> {
        self.devices.iter().position(|d| std::ptr::eq(d, device))
    }
}

impl State {
    fn poll(
        &mut self,
        devices: &[Device; DEVICES],
        one_pad: bool,
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        mouse: PadState,
    ) -> Vec<Event> {
        let now = now();
        let mut events = std::mem::take(&mut self.pending);
        let stand_in = merged(fallback(&keys), mouse);
        if one_pad {
            let pad = pads.into_iter().flatten().fold(stand_in, merged);
            self.update(devices, FALLBACK, Some(Value::Pad(pad)), now, &mut events);
            self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
            return events;
        }
        let any_pad = pads.iter().any(Option::is_some);
        for (slot, pad) in pads.into_iter().enumerate() {
            self.update(devices, slot, pad.map(Value::Pad), now, &mut events);
        }
        let fallback = (!any_pad).then_some(Value::Pad(stand_in));
        self.update(devices, FALLBACK, fallback, now, &mut events);
        self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
        events
    }

    /// Records a device's state: a reading when it changed, callbacks when
    /// the device came or went.
    fn update(
        &mut self,
        devices: &[Device; DEVICES],
        index: usize,
        value: Option<Value>,
        now: u64,
        events: &mut Vec<Event>,
    ) {
        let slot = &mut self.slots[index];
        let was = slot.connected;
        slot.connected = value.is_some();
        if let Some(value) = value
            && (!was || slot.history.back().is_none_or(|r| r.value != value))
        {
            slot.sequence += 1;
            if slot.history.len() == HISTORY {
                slot.history.pop_front();
            }
            slot.history.push_back(Ref::new(Reading {
                vtable: &READING,
                refs: AtomicU32::new(1),
                device: &devices[index],
                sequence: slot.sequence,
                timestamp: now,
                value,
            }));
        }
        if was != slot.connected {
            tracing::info!(device = index, connected = !was, "input device");
            let (current, previous) = if was {
                (0, STATUS_CONNECTED)
            } else {
                (STATUS_CONNECTED, 0)
            };
            events.extend(
                self.callbacks
                    .iter()
                    .filter(|r| r.matches(devices, index) && r.status & STATUS_CONNECTED != 0)
                    .map(|r| r.event(index, now, current, previous)),
            );
        }
    }
}

#[cfg(windows)]
fn host() -> Box<dyn Source> {
    Box::new(host::Host::default())
}

#[cfg(not(windows))]
fn host() -> Box<dyn Source> {
    struct Nothing;
    impl Source for Nothing {
        fn pad(&mut self, _: usize) -> Option<PadState> {
            None
        }
        fn keys(&mut self) -> Vec<KeyState> {
            Vec::new()
        }
        fn rumble(&mut self, _: usize, _: u16, _: u16) {}
    }
    Box::new(Nothing)
}

static HUB: LazyLock<&'static Hub> = LazyLock::new(|| Hub::start(host()));

type This = *const Object;
type Out = *mut *const c_void;

/// The DLL's only export (ordinal 1): the input object, static and shared.
pub extern "system" fn GameInputCreate(out: *mut This) -> HRESULT {
    tracing::info!("GameInputCreate");
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a pointer.
    unsafe { out.write(&HUB.object) };
    S_OK
}

fn hub(this: This) -> &'static Hub {
    // SAFETY: `this` is a hub's object; hubs are never freed.
    unsafe { &*(*this).hub }
}

/// Clears a COM out pointer and returns `hr`.
fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

/// Gives the title a reference to `reading`, or fails as with none.
fn hand(out: Out, reading: Option<&Ref>) -> HRESULT {
    match reading {
        _ if out.is_null() => E_INVALIDARG,
        Some(reading) => {
            // SAFETY: checked non-null; the title passes room for a pointer.
            unsafe { out.write(reading.share().cast()) };
            S_OK
        }
        None => fail(out, READING_NOT_FOUND),
    }
}

extern "system" fn no_interface(_: *const c_void, _iid: *const c_void, out: Out) -> HRESULT {
    tracing::warn!("input QueryInterface not implemented");
    fail(out, E_NOINTERFACE)
}
/// The input object and devices live as long as the process.
extern "system" fn add_ref(_: *const c_void) -> u32 {
    1
}

extern "system" fn timestamp(_: This) -> u64 {
    now()
}

extern "system" fn current_reading(
    this: This,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let state = hub.state();
    let newest = |i: usize| {
        state.slots[i]
            .history
            .back()
            .filter(|r| r.kind() & kind != 0)
    };
    let found = if device.is_null() {
        (0..DEVICES)
            .filter(|&i| state.slots[i].connected)
            .filter_map(newest)
            .max_by_key(|r| r.timestamp)
    } else {
        match hub.index(device) {
            None => return fail(out, E_INVALIDARG),
            Some(i) if !state.slots[i].connected => return fail(out, DEVICE_DISCONNECTED),
            Some(i) => newest(i),
        }
    };
    tracing::trace!(
        kind = format_args!("{kind:#x}"),
        device = ?hub.index(device),
        sequence = found.map(|r| r.sequence),
        "current reading"
    );
    hand(out, found)
}

/// The reading after (`forward`) or before `reference` on its device.
fn relative(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
    forward: bool,
) -> HRESULT {
    if reference.is_null() {
        return fail(out, E_INVALIDARG);
    }
    let hub = hub(this);
    // SAFETY: a reading we handed out and the title still holds.
    let reference = unsafe { &*reference };
    // shortcut: with no device given only the reference's device is walked,
    // not every device's readings in time order; the title walks
    // only keyboard readings (one device) and the current pad's previous one.
    let Some(index) = hub.index(reference.device) else {
        return fail(out, E_INVALIDARG);
    };
    if !device.is_null() && !std::ptr::eq(device, reference.device) {
        return fail(out, E_INVALIDARG);
    }
    let state = hub.state();
    let history = &state.slots[index].history;
    if history
        .front()
        .is_none_or(|oldest| reference.sequence < oldest.sequence)
    {
        return fail(out, REFERENCE_TOO_OLD);
    }
    let wanted = |r: &&Ref| r.kind() & kind != 0;
    let found = if forward {
        history
            .iter()
            .filter(|r| r.sequence > reference.sequence)
            .find(wanted)
    } else {
        history
            .iter()
            .rev()
            .filter(|r| r.sequence < reference.sequence)
            .find(wanted)
    };
    hand(out, found)
}
extern "system" fn next_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, true)
}
extern "system" fn previous_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, false)
}
/// The device's reading at `time`, among those still kept.
extern "system" fn temporal_reading(
    this: This,
    time: u64,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let Some(index) = hub.index(device) else {
        return fail(out, E_INVALIDARG);
    };
    let state = hub.state();
    let history = &state.slots[index].history;
    hand(out, history.iter().rev().find(|r| r.timestamp <= time))
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn token(out: *mut u64) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    S_OK
}

// shortcut: reading callbacks never run; the title registers none
// (it polls).
extern "system" fn register_reading(
    _: This,
    _device: *const c_void,
    _kind: u32,
    _threshold: f32,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    tracing::warn!("input reading callbacks are never called");
    token(out)
}

/// Device callbacks run on our poll thread when a device comes or goes.
/// Enumeration reports the devices already there: blocking on the caller's
/// thread before we return (the title registers holding a lock its callback
/// takes again), asynchronous on the next poll.
extern "system" fn register_device(
    this: This,
    device: *const Device,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let hub = hub(this);
    let registration = Registration {
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        device: device as usize,
        kind,
        status,
        context: context as usize,
        callback,
    };
    let mut found = Vec::new();
    {
        let mut state = hub.state();
        if enumeration != 0 && status & STATUS_CONNECTED != 0 {
            let now = now();
            found = (0..DEVICES)
                .filter(|&i| state.slots[i].connected && registration.matches(&hub.devices, i))
                .map(|i| registration.event(i, now, STATUS_CONNECTED, 0))
                .collect();
        }
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            enumeration,
            devices = found.len(),
            "input device callback"
        );
        state.callbacks.push(registration);
        if enumeration == ASYNC_ENUMERATION {
            state.pending.append(&mut found);
        }
    }
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(registration.token) };
    }
    hub.deliver(found);
    S_OK
}
/// Guide button and keyboard layout callbacks: neither ever changes here.
extern "system" fn register_other(
    _: This,
    _device: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    token(out)
}

fn forget(this: This, token: u64) {
    let mut state = hub(this).state();
    state.callbacks.retain(|r| r.token != token);
    state.pending.retain(|e| e.registration.token != token);
}
extern "system" fn stop_callback(this: This, token: u64) {
    forget(this, token);
}
/// Stops the callback, then waits up to `timeout` µs for a call of it that
/// is running; false on time-out, or from inside that callback (it cannot
/// wait for itself), as documented. Either way it is never called again.
extern "system" fn unregister_callback(this: This, token: u64, timeout: u64) -> bool {
    forget(this, token);
    let hub = hub(this);
    let me = std::thread::current().id();
    let deadline = Instant::now().checked_add(Duration::from_micros(timeout));
    let mut state = hub.state();
    loop {
        let running = state.running.iter().filter(|r| r.0 == token);
        if running.clone().any(|r| r.1 == me) {
            return false;
        }
        if running.count() == 0 {
            return true;
        }
        state = match deadline {
            None => hub.returned.wait(state).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let waited = hub.returned.wait_timeout(state, left);
                waited.unwrap_or_else(PoisonError::into_inner).0
            }
        };
    }
}

/// Dispatcher and aggregate device: no title of ours asks for them.
extern "system" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    tracing::warn!("input dispatcher not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn create_aggregate(_: This, _kind: u32, out: Out) -> HRESULT {
    tracing::warn!("aggregate input device not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn find_by_id(this: This, id: *const [u8; 32], out: Out) -> HRESULT {
    if id.is_null() || out.is_null() {
        return fail(out, E_INVALIDARG);
    }
    // SAFETY: checked non-null; the title passes a device id.
    let id = unsafe { &*id };
    match hub(this).devices.iter().find(|d| d.info.id == *id) {
        Some(device) => {
            // SAFETY: checked non-null.
            unsafe { out.write((device as *const Device).cast()) };
            S_OK
        }
        None => fail(out, DEVICE_NOT_FOUND),
    }
}
/// Lookup by object, platform handle or platform string: our devices have
/// none of these.
extern "system" fn find_device(_: This, _value: *const c_void, out: Out) -> HRESULT {
    fail(out, DEVICE_NOT_FOUND)
}
extern "system" fn enable_oem_device(
    _: This,
    _vendor: u16,
    _product: u16,
    _interface: u8,
    _collection: u8,
) -> HRESULT {
    S_OK
}
extern "system" fn set_focus_policy(_: This, _policy: u32) {}

static INPUT: Table<22> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    timestamp as *const (),           // 0x18
    current_reading as *const (),     // 0x20
    next_reading as *const (),        // 0x28
    previous_reading as *const (),    // 0x30
    temporal_reading as *const (),    // 0x38
    register_reading as *const (),    // 0x40
    register_device as *const (),     // 0x48
    register_other as *const (),      // 0x50 guide button
    register_other as *const (),      // 0x58 keyboard layout
    stop_callback as *const (),       // 0x60
    unregister_callback as *const (), // 0x68
    create_dispatcher as *const (),   // 0x70
    create_aggregate as *const (),    // 0x78
    find_by_id as *const (),          // 0x80
    find_device as *const (),         // 0x88 by object
    find_device as *const (),         // 0x90 by platform handle
    find_device as *const (),         // 0x98 by platform string
    enable_oem_device as *const (),   // 0xa0
    set_focus_policy as *const (),    // 0xa8
]);

extern "system" fn device_info(this: *const Device) -> *const DeviceInfo {
    // SAFETY: one of our devices.
    unsafe { &(*this).info }
}
extern "system" fn device_status(this: *const Device) -> u32 {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    if device.hub().state().slots[device.index].connected {
        STATUS_CONNECTED
    } else {
        0
    }
}
// shortcut: trigger motors are dropped (XInput has none). The host API that
// drives them without a redistributable is Windows.Gaming.Input (WinRT
// `Gamepad.Vibration`), which needs WinRT bindings this crate lacks outside
// diagnostics builds; the title sends trigger levels in slot 0x50.
extern "system" fn set_rumble(this: *const Device, params: *const [f32; 4]) {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    let one_pad = device.hub().one_pad;
    if device.index >= PADS && !(one_pad && device.index == FALLBACK) {
        return;
    }
    let [low, high, ..] = if params.is_null() {
        [0.0; 4]
    } else {
        // SAFETY: checked non-null; four motor levels.
        unsafe { *params }
    };
    let speed = |level: f32| (level.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
    let mut source = lock(&device.hub().source);
    if one_pad {
        // The one pad is every host pad.
        for slot in 0..PADS {
            if source.pad(slot).is_some() {
                source.rumble(slot, speed(low), speed(high));
            }
        }
    } else {
        source.rumble(device.index, speed(low), speed(high));
    }
}
extern "system" fn unsupported<const SLOT: usize>(_: *const c_void) -> HRESULT {
    tracing::warn!(
        slot = format_args!("{SLOT:#x}"),
        "input device method not implemented"
    );
    E_NOTIMPL
}

static DEVICE: Table<21> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    device_info as *const (),         // 0x18
    device_status as *const (),       // 0x20
    unsupported::<0x28> as *const (), // battery
    unsupported::<0x30> as *const (), // force feedback
    unsupported::<0x38> as *const (), //
    unsupported::<0x40> as *const (), //
    unsupported::<0x48> as *const (), // haptics
    set_rumble as *const (),          // 0x50
    unsupported::<0x58> as *const (), // input synchronization
    unsupported::<0x60> as *const (), //
    unsupported::<0x68> as *const (), // power off
    unsupported::<0x70> as *const (), // raw device access
    unsupported::<0x78> as *const (), //
    unsupported::<0x80> as *const (), //
    unsupported::<0x88> as *const (), //
    unsupported::<0x90> as *const (), //
    unsupported::<0x98> as *const (), //
    unsupported::<0xa0> as *const (), //
]);

fn reading<'a>(this: *const Reading) -> &'a Reading {
    // SAFETY: a reading we handed out and the title still holds.
    unsafe { &*this }
}
extern "system" fn reading_add_ref(this: *const Reading) -> u32 {
    reading(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
extern "system" fn reading_release(this: *const Reading) -> u32 {
    let left = reading(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference is gone; `Ref::new` boxed it.
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
    left
}
extern "system" fn input_kind(this: *const Reading) -> u32 {
    reading(this).kind()
}
extern "system" fn sequence_number(this: *const Reading, kind: u32) -> u64 {
    let reading = reading(this);
    if reading.kind() & kind != 0 {
        reading.sequence
    } else {
        0
    }
}
extern "system" fn reading_timestamp(this: *const Reading) -> u64 {
    reading(this).timestamp
}
extern "system" fn reading_device(this: *const Reading, out: *mut *const Device) {
    if !out.is_null() {
        // SAFETY: checked non-null; devices need no reference.
        unsafe { out.write(reading(this).device) };
    }
}
extern "system" fn key_count(this: *const Reading) -> u32 {
    match &reading(this).value {
        Value::Keys(keys) => keys.len() as u32,
        Value::Pad(_) => 0,
    }
}
extern "system" fn key_state(this: *const Reading, count: u32, out: *mut KeyState) -> u32 {
    let Value::Keys(keys) = &reading(this).value else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let n = keys.len().min(count as usize);
    // SAFETY: the title passes room for `count` keys.
    unsafe { std::ptr::copy_nonoverlapping(keys.as_ptr(), out, n) };
    n as u32
}
extern "system" fn gamepad_state(this: *const Reading, out: *mut PadState) -> bool {
    match &reading(this).value {
        Value::Pad(state) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(*state) };
            true
        }
        _ => false,
    }
}
/// Kinds our devices do not have: no count, no state.
extern "system" fn nothing(_: *const c_void) -> usize {
    0
}

static READING: Table<25> = Table([
    no_interface as *const (),      // 0x00
    reading_add_ref as *const (),   // 0x08
    reading_release as *const (),   // 0x10
    input_kind as *const (),        // 0x18
    sequence_number as *const (),   // 0x20
    reading_timestamp as *const (), // 0x28
    reading_device as *const (),    // 0x30
    nothing as *const (),           // 0x38 raw report
    nothing as *const (),           // 0x40 controller axes, buttons, switches
    nothing as *const (),           // 0x48
    nothing as *const (),           // 0x50
    nothing as *const (),           // 0x58
    nothing as *const (),           // 0x60
    nothing as *const (),           // 0x68
    key_count as *const (),         // 0x70
    key_state as *const (),         // 0x78
    nothing as *const (),           // 0x80 mouse
    nothing as *const (),           // 0x88 touch
    nothing as *const (),           // 0x90
    nothing as *const (),           // 0x98 motion
    nothing as *const (),           // 0xa0 arcade stick
    nothing as *const (),           // 0xa8 flight stick
    gamepad_state as *const (),     // 0xb0
    nothing as *const (),           // 0xb8 racing wheel
    nothing as *const (),           // 0xc0 UI navigation
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "gameinput",
        dlls: &["gameinput.dll"],
        // Titles import it by ordinal only.
        exports: vec![HleExport {
            name: "#1",
            address: GameInputCreate as *const () as usize,
        }],
    }
}

/// Host input: XInput pads, the keyboard while our process has focus.
#[cfg(windows)]
mod host {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MAPVK_VSC_TO_VK, MapVirtualKeyW,
    };
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
        XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
        XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
        XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
        XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState,
        XInputSetState,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ClipCursor, CreateWindowExW, GetCursorPos, GetForegroundWindow, GetWindowInfo,
        GetWindowThreadProcessId, HTCLIENT, HWND_MESSAGE, MSG, PM_REMOVE, PeekMessageW,
        PostMessageW, SetCursorPos, WINDOWINFO, WM_MOUSEMOVE, WM_SETCURSOR,
    };
    use windows_sys::core::w;

    const WM_INPUT: u32 = 0x00FF;
    const RID_INPUT: u32 = 0x1000_0003;
    const RIM_TYPEMOUSE: u32 = 0;
    const MOUSE_MOVE_ABSOLUTE: u16 = 1;
    const RIDEV_INPUTSINK: u32 = 0x0000_0100;

    #[repr(C)]
    struct RawInputDevice {
        us_usage_page: u16,
        us_usage: u16,
        dw_flags: u32,
        hwnd_target: HWND,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RawInputHeader {
        dw_type: u32,
        dw_size: u32,
        h_device: isize,
        w_param: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RawMouse {
        us_flags: u16,
        _pad: u16,
        ul_buttons: u32,
        ul_raw_buttons: u32,
        l_last_x: i32,
        l_last_y: i32,
        ul_extra_information: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RawInputMousePacket {
        header: RawInputHeader,
        mouse: RawMouse,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn RegisterRawInputDevices(devices: *const RawInputDevice, num_devices: u32, size: u32) -> i32;
        fn GetRawInputData(
            h_raw_input: isize,
            ui_command: u32,
            p_data: *mut c_void,
            pcb_size: *mut u32,
            cb_size_header: u32,
        ) -> u32;
    }

    #[derive(Default)]
    struct RawMousePump {
        init_attempted: bool,
        hwnd: HWND,
    }

    // SAFETY: `RawMousePump` is only touched by `Host` behind `Hub::source` mutex
    // on the poll thread, and `hwnd` is a process-local message-only window handle.
    unsafe impl Send for RawMousePump {}

    impl RawMousePump {
        fn ensure_init(&mut self) {
            if self.init_attempted {
                return;
            }
            self.init_attempted = true;
            // SAFETY: creates a lightweight message-only window on this poll thread
            // and registers `RIDEV_INPUTSINK` mouse raw input so deltas arrive at 125 Hz
            // regardless of the game's render FPS.
            unsafe {
                let hwnd = CreateWindowExW(
                    0,
                    w!("STATIC"),
                    w!(""),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    return;
                }
                let rid = RawInputDevice {
                    us_usage_page: 0x01,
                    us_usage: 0x02,
                    dw_flags: RIDEV_INPUTSINK,
                    hwnd_target: hwnd,
                };
                if RegisterRawInputDevices(&rid, 1, size_of::<RawInputDevice>() as u32) != 0 {
                    self.hwnd = hwnd;
                    RAW_PUMP_ACTIVE.store(true, Ordering::Release);
                    tracing::info!("raw mouse input pump active (125 Hz RIDEV_INPUTSINK)");
                }
            }
        }

        fn drain(&mut self) -> (i32, i32, bool, bool) {
            self.ensure_init();
            if self.hwnd.is_null() {
                return (0, 0, false, false);
            }
            let (mut dx, mut dy) = (0i32, 0i32);
            let (mut saw_rel, mut saw_abs) = (false, false);
            let mut msg = MSG::default();
            // SAFETY: drains queued `WM_INPUT` messages for our message-only window.
            unsafe {
                while PeekMessageW(&mut msg, self.hwnd, WM_INPUT, WM_INPUT, PM_REMOVE) != 0 {
                    let mut buf = [0u8; 64];
                    let mut size = buf.len() as u32;
                    let read = GetRawInputData(
                        msg.lParam,
                        RID_INPUT,
                        buf.as_mut_ptr().cast(),
                        &mut size,
                        size_of::<RawInputHeader>() as u32,
                    );
                    if read != u32::MAX && (read as usize) >= size_of::<RawInputMousePacket>() {
                        let pkt = buf.as_ptr().cast::<RawInputMousePacket>().read_unaligned();
                        if pkt.header.dw_type == RIM_TYPEMOUSE {
                            if (pkt.mouse.us_flags & MOUSE_MOVE_ABSOLUTE) == 0 {
                                saw_rel = true;
                                dx = dx.saturating_add(pkt.mouse.l_last_x);
                                dy = dy.saturating_add(pkt.mouse.l_last_y);
                            } else {
                                saw_abs = true;
                            }
                        }
                    }
                }
            }
            (dx, dy, saw_rel, saw_abs)
        }
    }

    use super::*;

    /// Asking XInput about an empty slot is slow; look again this often.
    const EMPTY_SLOT_RECHECK: Duration = Duration::from_secs(1);

    const BUTTONS: [(u16, u32); 14] = [
        (XINPUT_GAMEPAD_START, MENU),
        (XINPUT_GAMEPAD_BACK, VIEW),
        (XINPUT_GAMEPAD_A, A),
        (XINPUT_GAMEPAD_B, B),
        (XINPUT_GAMEPAD_X, X),
        (XINPUT_GAMEPAD_Y, Y),
        (XINPUT_GAMEPAD_DPAD_UP, UP),
        (XINPUT_GAMEPAD_DPAD_DOWN, DOWN),
        (XINPUT_GAMEPAD_DPAD_LEFT, LEFT),
        (XINPUT_GAMEPAD_DPAD_RIGHT, RIGHT),
        (XINPUT_GAMEPAD_LEFT_SHOULDER, LEFT_SHOULDER),
        (XINPUT_GAMEPAD_RIGHT_SHOULDER, RIGHT_SHOULDER),
        (XINPUT_GAMEPAD_LEFT_THUMB, LEFT_STICK),
        (XINPUT_GAMEPAD_RIGHT_THUMB, RIGHT_STICK),
    ];

    /// Raw values, no dead zone: titles apply their own.
    pub(super) fn convert(pad: &XINPUT_GAMEPAD) -> PadState {
        let stick = |v: i16| (f32::from(v) / 32767.0).max(-1.0);
        PadState {
            buttons: BUTTONS
                .iter()
                .filter(|(host, _)| pad.wButtons & host != 0)
                .fold(0, |all, (_, bit)| all | bit),
            left_trigger: f32::from(pad.bLeftTrigger) / 255.0,
            right_trigger: f32::from(pad.bRightTrigger) / 255.0,
            left_x: stick(pad.sThumbLX),
            left_y: stick(pad.sThumbLY),
            right_x: stick(pad.sThumbRX),
            right_y: stick(pad.sThumbRY),
        }
    }

    #[derive(Default)]
    pub(super) struct Host {
        empty_until: [Option<Instant>; PADS],
        capture: MouseCapture,
        raw_pump: RawMousePump,
        raw_mode: bool,
        cursor_clipped: bool,
        clipped_rect: Option<(i32, i32, i32, i32)>,
        anchor: (i32, i32),
        mouse_last_poll: Option<Instant>,
        mouse_look: MouseLook,
        mouse_buttons: MouseButtons,
    }

    impl Host {
        fn set_cursor_clip(&mut self, rect: Option<(i32, i32, i32, i32)>) {
            match rect {
                Some((left, top, right, bottom)) => {
                    if !self.cursor_clipped || self.clipped_rect != Some((left, top, right, bottom)) {
                        let rc = RECT {
                            left,
                            top,
                            right,
                            bottom,
                        };
                        // SAFETY: confines cursor to our own foreground client rect while locked.
                        if unsafe { ClipCursor(&rc) } != 0 {
                            self.cursor_clipped = true;
                            self.clipped_rect = Some((left, top, right, bottom));
                        }
                    }
                }
                None => {
                    if self.cursor_clipped {
                        // SAFETY: releases cursor confinement.
                        unsafe {
                            let _ = ClipCursor(std::ptr::null());
                        }
                        self.cursor_clipped = false;
                        self.clipped_rect = None;
                    }
                }
            }
        }

        fn set_mouse_locked(&mut self, locked: bool, hwnd: Option<HWND>) {
            if !locked {
                self.set_cursor_clip(None);
            }
            if CURSOR_LOCKED.swap(locked, Ordering::Release) == locked {
                return;
            }
            tracing::info!(locked, "mouse look lock state changed");
            if let Some(hwnd) = hwnd {
                // Prompt the window's UI thread to run WM_SETCURSOR right away
                // so the cursor hides/reappears the instant `~`/`F2`/`Esc`/`P`
                // is pressed, even before moving the mouse.
                let lparam = (HTCLIENT as isize) | ((WM_MOUSEMOVE as isize) << 16);
                // SAFETY: asynchronous post to our own process's foreground window.
                unsafe {
                    let _ = PostMessageW(hwnd, WM_SETCURSOR, hwnd as usize, lparam);
                }
            }
        }

        fn disarm_mouse(&mut self, hwnd: Option<HWND>, reason: &'static str) {
            if CURSOR_LOCKED.load(Ordering::Relaxed) {
                tracing::info!(reason, "mouse look unlocked");
            }
            self.set_mouse_locked(false, hwnd);
            self.mouse_look.reset();
            self.mouse_last_poll = None;
            self.mouse_buttons.reset();
        }
    }

    impl Source for Host {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            if self.empty_until[slot].is_some_and(|until| Instant::now() < until) {
                return None;
            }
            let mut state = XINPUT_STATE::default();
            // SAFETY: room for the state.
            if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
                self.empty_until[slot] = Some(Instant::now() + EMPTY_SLOT_RECHECK);
                return None;
            }
            self.empty_until[slot] = None;
            #[allow(unused_mut)]
            let mut pad = convert(&state.Gamepad);
            // The scripted keys (`VICTORY_KEYS`) also press the first
            // pad's buttons, by the stand-in pad's table: with a real pad
            // connected the stand-in is not there, and a title that listens
            // to the pad (the title's start screen) saw nothing.
            #[cfg(feature = "diagnostics")]
            if slot == 0 {
                for vk in scripted() {
                    if let Some(&(_, button)) = super::FALLBACK_BUTTONS.iter().find(|(key, _)| *key == vk) {
                        pad.buttons |= button;
                    }
                }
            }
            Some(pad)
        }

        fn keys(&mut self) -> Vec<KeyState> {
            #[cfg(feature = "diagnostics")]
            let mut down = scripted();
            #[cfg(not(feature = "diagnostics"))]
            let mut down = Vec::new();
            if victory_gpu::tequesta_ui::menu_open() {
                return down
                    .into_iter()
                    .map(|vk| KeyState {
                        // SAFETY: plain query.
                        scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                        virtual_key: vk,
                        ..Default::default()
                    })
                    .collect();
            }
            if focused() {
                // Left/right modifier keys only, as devices report them; no
                // mouse buttons (below 0x08), and keep F1/F3/F4/F5 overlay hotkeys
                // out of the title's keyboard state (leaving F2/~ mouse toggle intact).
                let held: Vec<u8> = (0x08..=0xFE_u8)
                    .filter(|vk| {
                        !(0x10..=0x12).contains(vk)
                            && !is_overlay_hotkey_vk(*vk)
                            && !down.contains(vk)
                    })
                    // SAFETY: plain queries.
                    .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
                    .collect();
                down.extend(held);
            }
            down.into_iter()
                .map(|vk| KeyState {
                    // SAFETY: plain query.
                    scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                    virtual_key: vk,
                    // shortcut: no characters (code point 0);
                    // the title reads scan codes and virtual keys only.
                    ..Default::default()
                })
                .collect()
        }

        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            let speeds = XINPUT_VIBRATION {
                wLeftMotorSpeed: low,
                wRightMotorSpeed: high,
            };
            // SAFETY: a valid vibration; an empty slot just fails.
            unsafe { XInputSetState(slot as u32, &speeds) };
        }

        fn mouse_pad(&mut self, active: bool) -> PadState {
            // Always drain the raw input pump so stale deltas never queue up while unlocked.
            let (pump_dx, pump_dy, saw_rel, saw_abs) = self.raw_pump.drain();
            if saw_rel {
                self.raw_mode = true;
            } else if saw_abs {
                self.raw_mode = false;
            }

            let key_down = |vk: i32| {
                // SAFETY: plain async key state query.
                unsafe { GetAsyncKeyState(vk) < 0 }
            };
            // Toggle mouse-look lock with F2 (0x71), VK_OEM_3 (0xC0, `` ` `` / `~` on US),
            // or scancode 0x29 (top-left `²` on French AZERTY).
            // SAFETY: plain keyboard layout query.
            let top_left_vk = unsafe { MapVirtualKeyW(0x29, MAPVK_VSC_TO_VK) } as i32;
            let toggle_down =
                key_down(0x71) || key_down(0xC0) || (top_left_vk != 0 && key_down(top_left_vk));
            let unlock_down = key_down(0x1B) || key_down(i32::from(b'P'));

            let menu_open = victory_gpu::tequesta_ui::menu_open();
            let hwnd = foreground_window();
            let active_focus = gta_kbm() && active && hwnd.is_some();
            let (locked, just_locked) =
                self.capture
                    .step(active_focus, menu_open, toggle_down, unlock_down);
            self.set_mouse_locked(locked, hwnd);

            if !active_focus || menu_open {
                self.disarm_mouse(hwnd, "menu, focus lost, or stand-in inactive");
                RAW_MOUSE_DX.store(0, Ordering::Relaxed);
                RAW_MOUSE_DY.store(0, Ordering::Relaxed);
                return PadState::default();
            }
            let hwnd = hwnd.expect("checked active_focus");

            let Some((c_left, c_top, c_right, c_bottom)) = client_rect_screen(hwnd) else {
                self.disarm_mouse(Some(hwnd), "client unavailable");
                return PadState::default();
            };
            let mut pos = POINT::default();
            // SAFETY: valid pointer to our initialized POINT.
            if unsafe { GetCursorPos(&mut pos) } == 0 {
                self.disarm_mouse(Some(hwnd), "cursor query failed");
                return PadState::default();
            }

            let now_inst = Instant::now();
            let dt = self
                .mouse_last_poll
                .replace(now_inst)
                .map_or(POLL, |prev| now_inst.saturating_duration_since(prev));

            let (right_x, right_y) = if locked {
                self.set_cursor_clip(Some((c_left, c_top, c_right, c_bottom)));
                let cx = c_left + (c_right - c_left) / 2;
                let cy = c_top + (c_bottom - c_top) / 2;
                let (fallback_dx, fallback_dy) = (
                    RAW_MOUSE_DX.swap(0, Ordering::Relaxed),
                    RAW_MOUSE_DY.swap(0, Ordering::Relaxed),
                );
                let (rdx, rdy) = if raw_pump_active() {
                    (pump_dx, pump_dy)
                } else {
                    if fallback_dx != 0 || fallback_dy != 0 || raw_mouse_recent(now()) {
                        self.raw_mode = true;
                    }
                    (fallback_dx, fallback_dy)
                };
                let use_raw = native_kbm() && self.raw_mode;
                let (dx, dy) = if just_locked {
                    self.anchor = recenter_cursor(cx, cy).unwrap_or((pos.x, pos.y));
                    self.mouse_look.reset();
                    (0, 0)
                } else if use_raw {
                    // Never feed cursor deltas in raw mode; recenter only near edges.
                    if pos.x <= c_left + 48
                        || pos.x >= c_right - 48
                        || pos.y <= c_top + 48
                        || pos.y >= c_bottom - 48
                    {
                        self.anchor = recenter_cursor(cx, cy).unwrap_or((pos.x, pos.y));
                    } else {
                        self.anchor = (pos.x, pos.y);
                    }
                    (rdx, rdy)
                } else {
                    let dx = pos.x - self.anchor.0;
                    let dy = pos.y - self.anchor.1;
                    let max_step = ((c_right - c_left) / 3).max(160);
                    if dx != 0 || dy != 0 {
                        self.anchor = recenter_cursor(cx, cy).unwrap_or((pos.x, pos.y));
                        if dx.abs() > max_step || dy.abs() > max_step {
                            (0, 0)
                        } else {
                            (dx, dy)
                        }
                    } else {
                        (0, 0)
                    }
                };
                self.mouse_look
                    .step(dx, dy, dt, mouse_sens(), mouse_invert_y())
            } else {
                self.set_cursor_clip(None);
                RAW_MOUSE_DX.store(0, Ordering::Relaxed);
                RAW_MOUSE_DY.store(0, Ordering::Relaxed);
                self.mouse_look.reset();
                (0.0, 0.0)
            };

            let in_client =
                (c_left..c_right).contains(&pos.x) && (c_top..c_bottom).contains(&pos.y);
            let [lmb, rmb, mmb] = self.mouse_buttons.update(
                [key_down(0x01), key_down(0x02), key_down(0x04)],
                locked || in_client,
            );
            let mut pad = PadState {
                right_x,
                right_y,
                ..Default::default()
            };
            if lmb {
                pad.right_trigger = 1.0; // LMB: Shoot / Attack / Gas (RT)
            }
            if rmb {
                pad.left_trigger = 1.0; // RMB: Aim / Brake-Reverse (LT)
            }
            if mmb {
                pad.buttons |= RIGHT_STICK; // MMB: Look behind / Special (R3)
            }
            pad
        }
    }

    /// Debugging aid: `VICTORY_KEYS=<seconds>:<key>,...` holds each key for
    /// 0.2 s that long after input starts (`<from>-<to>:<key>`: all that
    /// time), with or without focus. A key is a
    /// letter, a digit, or `Enter`, `Esc`, `Tab`, `Back`, `Up`, `Down`,
    /// `Left`, `Right` (the stand-in pad's table maps them to buttons).
    #[cfg(feature = "diagnostics")]
    fn scripted() -> Vec<u8> {
        static SCRIPT: LazyLock<(Instant, Vec<(Duration, Duration, u8)>)> = LazyLock::new(|| {
            let list = std::env::var("VICTORY_KEYS").unwrap_or_default();
            let keys = list.split(',').filter_map(|entry| {
                let (at, key) = entry.split_once(':')?;
                let vk = match key.trim() {
                    "Enter" => 0x0D,
                    "Esc" => 0x1B,
                    "Tab" => 0x09,
                    "Back" => 0x08,
                    "Up" => 0x26,
                    "Down" => 0x28,
                    "Left" => 0x25,
                    "Right" => 0x27,
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
                        k.as_bytes()[0].to_ascii_uppercase()
                    }
                    _ => return None,
                };
                let time = |t: &str| Duration::try_from_secs_f64(t.trim().parse().ok()?).ok();
                let (from, to) = match at.split_once('-') {
                    Some((from, to)) => (time(from)?, time(to)?),
                    None => (time(at)?, time(at)? + Duration::from_millis(200)),
                };
                Some((from, to, vk))
            });
            (Instant::now(), keys.collect())
        });
        let (start, keys) = &*SCRIPT;
        let now = start.elapsed();
        keys.iter()
            .filter(|&&(from, to, _)| (from..to).contains(&now))
            .map(|&(_, _, vk)| vk)
            .collect()
    }

    /// The foreground window if it belongs to our process.
    fn foreground_window() -> Option<HWND> {
        let mut process = 0;
        // SAFETY: plain queries; a null window gives process 0.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return None;
            }
            GetWindowThreadProcessId(hwnd, &mut process);
            (process == GetCurrentProcessId()).then_some(hwnd)
        }
    }

    /// The foreground window is one of ours (the title's or our fallback).
    fn focused() -> bool {
        foreground_window().is_some()
    }

    /// Recenters the cursor at `(cx, cy)` and reads back its actual screen
    /// position so fractional DPI scaling never introduces phantom drift.
    /// Returns `None` if either Win32 call fails.
    fn recenter_cursor(cx: i32, cy: i32) -> Option<(i32, i32)> {
        // SAFETY: recenters the cursor inside our own foreground window.
        if unsafe { SetCursorPos(cx, cy) } == 0 {
            return None;
        }
        let mut settled = POINT::default();
        // SAFETY: valid pointer to our initialized POINT.
        if unsafe { GetCursorPos(&mut settled) } == 0 {
            return None;
        }
        Some((settled.x, settled.y))
    }

    /// Client area in screen coordinates directly from Win32 `GetWindowInfo`.
    fn client_rect_screen(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
        let mut info = WINDOWINFO {
            cbSize: size_of::<WINDOWINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: valid initialized WINDOWINFO with cbSize set.
        if unsafe { GetWindowInfo(hwnd, &mut info) } == 0 {
            return None;
        }
        let rc = info.rcClient;
        if rc.right - rc.left < 64 || rc.bottom - rc.top < 64 {
            return None;
        }
        Some((rc.left, rc.top, rc.right, rc.bottom))
    }

    #[test]
    fn host_pads_convert_to_console_pads() {
        let pad = XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_DPAD_LEFT,
            bLeftTrigger: 255,
            sThumbLX: -32768,
            sThumbLY: 32767,
            ..Default::default()
        };
        let state = convert(&pad);
        assert_eq!(state.buttons, A | MENU | LEFT);
        assert_eq!(
            (
                state.left_trigger,
                state.left_x,
                state.left_y,
                state.right_x
            ),
            (1.0, -1.0, 1.0, 0.0)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::HleResolver;
    use crate::HostPolicy;
    use victory_loader::Provenance;
    use victory_pe::ImportSymbol;

    #[derive(Default)]
    struct FakeHost {
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        mouse: PadState,
        mouse_active_log: Vec<bool>,
        rumble: Vec<(usize, u16, u16)>,
    }

    struct Fake(Arc<Mutex<FakeHost>>);

    impl Source for Fake {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            self.0.lock().unwrap().pads[slot]
        }
        fn keys(&mut self) -> Vec<KeyState> {
            self.0.lock().unwrap().keys.clone()
        }
        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            self.0.lock().unwrap().rumble.push((slot, low, high));
        }
        fn mouse_pad(&mut self, active: bool) -> PadState {
            let mut host = self.0.lock().unwrap();
            host.mouse_active_log.push(active);
            if active {
                host.mouse
            } else {
                PadState::default()
            }
        }
    }

    fn fake() -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        fake_with(false)
    }

    fn fake_with(one_pad: bool) -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        let host = Arc::new(Mutex::new(FakeHost::default()));
        let hub = Hub::new(Box::new(Fake(Arc::clone(&host))), one_pad);
        hub.poll();
        (hub, host)
    }

    fn key(virtual_key: u8) -> KeyState {
        KeyState {
            scan_code: u32::from(virtual_key) + 0x100,
            virtual_key,
            ..Default::default()
        }
    }

    fn slot<F: Copy, T>(object: *const T, offset: usize) -> F {
        // SAFETY: our objects start with their table, which has the slot.
        let f = unsafe { *(*(object as *const *const *const ())).add(offset / 8) };
        // SAFETY: the caller names the slot's signature.
        unsafe { std::mem::transmute_copy(&f) }
    }

    /// Device callbacks seen: device index, current and previous status.
    type Log = Mutex<Vec<(usize, u32, u32)>>;

    extern "system" fn record(
        _token: u64,
        context: *mut c_void,
        device: *const Device,
        _timestamp: u64,
        current: u32,
        previous: u32,
    ) {
        // SAFETY: the test passes its log and one of the hub's devices.
        let (log, index) = unsafe { (&*(context as *const Log), (*device).index) };
        log.lock().unwrap().push((index, current, previous));
    }

    fn register(hub: &Hub, kind: u32, enumeration: u32, log: &Log) -> u64 {
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let mut token = 0;
        let context = log as *const Log as *mut c_void;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let hr = register(
            &hub.object,
            std::ptr::null(),
            kind,
            1,
            enumeration,
            context,
            Some(record),
            &mut token,
        );
        assert_eq!(hr, S_OK);
        assert_ne!(token, 0);
        token
    }

    fn result(hr: HRESULT, out: *const c_void) -> Result<*const Reading, HRESULT> {
        if hr == S_OK {
            Ok(out.cast())
        } else {
            assert!(out.is_null());
            Err(hr)
        }
    }

    fn current(hub: &Hub, kind: u32, device: usize) -> Result<*const Reading, HRESULT> {
        type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let hr = slot::<Current, _>(&hub.object, 0x20)(
            &hub.object,
            kind,
            &hub.devices[device],
            &mut out,
        );
        result(hr, out)
    }

    fn walk(hub: &Hub, offset: usize, from: *const Reading) -> Result<*const Reading, HRESULT> {
        type Walk = extern "system" fn(This, *const Reading, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let device = reading(from).device;
        let hr =
            slot::<Walk, _>(&hub.object, offset)(&hub.object, from, GAMEPAD_KIND, device, &mut out);
        result(hr, out)
    }

    fn pad(reading: *const Reading) -> PadState {
        let mut state = PadState::default();
        let get =
            slot::<extern "system" fn(*const Reading, *mut PadState) -> bool, _>(reading, 0xb0);
        assert!(get(reading, &mut state));
        state
    }

    fn stamp(reading: *const Reading) -> (u64, u64) {
        let time = slot::<extern "system" fn(*const Reading) -> u64, _>(reading, 0x28);
        let sequence = slot::<extern "system" fn(*const Reading, u32) -> u64, _>(reading, 0x20);
        (time(reading), sequence(reading, GAMEPAD_KIND))
    }

    fn release(reading: *const Reading) {
        slot::<extern "system" fn(*const Reading) -> u32, _>(reading, 0x10)(reading);
    }

    #[test]
    fn ordinal_one_resolves_to_create() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("GameInput.dll", &ImportSymbol::Ordinal(1)),
            Some(Provenance::Hle)
        );
        assert_eq!(r.classify("GameInput.dll", &ImportSymbol::Ordinal(2)), None);
    }

    #[test]
    fn pads_connect_report_and_disconnect() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        // Blocking enumeration ran before registration returned: with no
        // host pad the keyboard's stand-in is the only one.
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: A,
            left_x: 0.5,
            ..Default::default()
        });
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[1..],
            [(1, STATUS_CONNECTED, 0), (FALLBACK, 0, STATUS_CONNECTED)]
        );

        let first = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(first).buttons, A);
        assert_eq!(pad(first).left_x, 0.5);
        // No change, no new reading.
        hub.poll();
        let again = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(again, first);
        release(again);

        std::thread::sleep(Duration::from_millis(2));
        host.lock().unwrap().pads[1].as_mut().unwrap().buttons = B;
        hub.poll();
        let second = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(second).buttons, B);
        let (t1, s1) = stamp(first);
        let (t2, s2) = stamp(second);
        assert!(t2 > t1 && s2 == s1 + 1);
        assert!(t2 <= now());

        // The title's edge check: previous of current, and back again.
        assert_eq!(walk(hub, 0x30, second), Ok(first));
        assert_eq!(walk(hub, 0x28, first), Ok(second));
        assert_eq!(walk(hub, 0x28, second), Err(READING_NOT_FOUND));
        assert_eq!(walk(hub, 0x30, first), Err(READING_NOT_FOUND));
        for r in [first, first, second, second] {
            release(r);
        }

        host.lock().unwrap().pads[1] = None;
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[3..],
            [(1, 0, STATUS_CONNECTED), (FALLBACK, STATUS_CONNECTED, 0)]
        );
        assert_eq!(current(hub, GAMEPAD_KIND, 1), Err(DEVICE_DISCONNECTED));
        // The stand-in is back, at rest.
        let rest = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(pad(rest), PadState::default());
        release(rest);
    }

    #[test]
    fn keys_feed_the_keyboard_and_the_stand_in_pad() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, KEYBOARD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(KEYBOARD, STATUS_CONNECTED, 0)]);

        let keys = vec![key(b'W'), key(0x0D), key(b'L'), key(b'C')];
        host.lock().unwrap().keys = keys.clone();
        hub.poll();
        let r = current(hub, KEYBOARD_KIND, KEYBOARD).unwrap();
        let count = slot::<extern "system" fn(*const Reading) -> u32, _>(r, 0x70);
        let state =
            slot::<extern "system" fn(*const Reading, u32, *mut KeyState) -> u32, _>(r, 0x78);
        assert_eq!(count(r), 4);
        let mut got = [KeyState::default(); 8];
        assert_eq!(state(r, 8, got.as_mut_ptr()), 4);
        assert_eq!(got[..4], keys[..]);
        release(r);

        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(
            pad(r),
            PadState {
                buttons: A,
                left_y: 1.0,
                right_x: 1.0,
                right_trigger: 1.0,
                ..Default::default()
            }
        );
        release(r);
        // A keyboard reading is no gamepad reading.
        assert_eq!(current(hub, GAMEPAD_KIND, KEYBOARD), Err(READING_NOT_FOUND));
    }

    #[test]
    fn a_host_pad_feeds_the_one_pad_which_never_goes() {
        let (hub, host) = fake_with(true);
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);
        let pad = |hub: &Hub| match &hub.state().slots[FALLBACK].history.back().unwrap().value {
            Value::Pad(p) => *p,
            Value::Keys(_) => unreachable!(),
        };
        host.lock().unwrap().keys = vec![key(b'D')];
        hub.poll();
        assert_eq!(pad(hub).left_x, 1.0);
        // A pad is switched on, in any slot: its input arrives, beside the
        // keyboard's, and no device comes or goes.
        host.lock().unwrap().pads[2] = Some(PadState { buttons: A, left_y: -0.5, ..Default::default() });
        hub.poll();
        let p = pad(hub);
        assert_eq!((p.buttons, p.left_x, p.left_y), (A, 1.0, -0.5));
        let device: *const Device = &hub.devices[FALLBACK];
        set_rumble(device, &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(host.lock().unwrap().rumble, [(2, u16::MAX, 0)]);
        // And off again.
        host.lock().unwrap().pads[2] = None;
        host.lock().unwrap().keys.clear();
        hub.poll();
        assert_eq!(pad(hub), PadState::default());
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(!hub.state().slots[2].connected);
    }

    #[test]
    fn async_enumeration_waits_for_the_poll_and_unregistering_stops_callbacks() {
        let (hub, host) = fake();
        let log = Log::default();
        let token = register(hub, GAMEPAD_KIND, ASYNC_ENUMERATION, &log);
        assert!(log.lock().unwrap().is_empty());
        hub.poll();
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(unregister(&hub.object, token, 5000));
        host.lock().unwrap().pads[0] = Some(PadState::default());
        hub.poll();
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn unregistering_waits_for_a_running_callback() {
        use std::sync::atomic::AtomicBool;
        static INSIDE: AtomicBool = AtomicBool::new(false);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        static FROM_INSIDE: AtomicU32 = AtomicU32::new(0);
        extern "system" fn slow(
            token: u64,
            context: *mut c_void,
            _: *const Device,
            _: u64,
            _: u32,
            _: u32,
        ) {
            // From inside its own callback, unregistering fails at once.
            let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(context, 0x68);
            if !unregister(context.cast(), token, 0) {
                FROM_INSIDE.fetch_add(1, Ordering::SeqCst);
            }
            INSIDE.store(true, Ordering::SeqCst);
            while !LEAVE.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let (hub, host) = fake();
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let mut token = 0;
        let context = (&raw const hub.object).cast_mut().cast();
        assert_eq!(
            register(&hub.object, std::ptr::null(), GAMEPAD_KIND, 1, 0, context, Some(slow), &mut token),
            S_OK
        );
        host.lock().unwrap().pads[1] = Some(PadState::default());
        let poller = std::thread::spawn(|| hub.poll());
        while !INSIDE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(!unregister(&hub.object, token, 5000), "times out while it runs");
        let leave = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            LEAVE.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        assert!(unregister(&hub.object, token, u64::MAX), "waits for it");
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert_eq!(FROM_INSIDE.load(Ordering::SeqCst), 1);
        poller.join().unwrap();
        leave.join().unwrap();
    }

    #[test]
    fn devices_are_found_by_id_and_rumble_reaches_the_host() {
        let (hub, host) = fake();
        host.lock().unwrap().pads[2] = Some(PadState::default());
        hub.poll();
        let device = &hub.devices[2] as *const Device;
        let info = slot::<extern "system" fn(*const Device) -> *const u8, _>(device, 0x18);
        // SAFETY: the info has the id at +0x20.
        let id = unsafe { *(info(device).add(0x20) as *const [u8; 32]) };
        assert_ne!(id, [0; 32]);

        type Find = extern "system" fn(This, *const [u8; 32], Out) -> HRESULT;
        let find = slot::<Find, _>(&hub.object, 0x80);
        let mut out = std::ptr::null();
        assert_eq!(find(&hub.object, &id, &mut out), S_OK);
        assert_eq!(out, device.cast());
        assert_eq!(find(&hub.object, &[1; 32], &mut out), DEVICE_NOT_FOUND);
        assert!(out.is_null());

        type Rumble = extern "system" fn(*const Device, *const [f32; 4]);
        let rumble = slot::<Rumble, _>(device, 0x50);
        rumble(device, &[0.5, 1.0, 1.0, 1.0]);
        rumble(&hub.devices[FALLBACK], &[1.0; 4]);
        rumble(device, std::ptr::null());
        assert_eq!(host.lock().unwrap().rumble, [(2, 32767, 65535), (2, 0, 0)]);
    }

    #[test]
    fn gta_kbm_maps_pc_style_keys_and_smooths_mouse_look() {
        let keys = vec![
            key(b'W'),
            key(0xA0), // Left Shift -> A (Sprint)
            key(0x20), // Space -> X (Jump)
            key(b'R'), // R -> B (Reload)
            key(b'F'), // F -> Y (Enter/Exit vehicle)
            key(0x09), // Tab -> LB (Weapon wheel)
            key(b'Q'), // Q -> RB (Cover)
            key(0xA2), // Left Ctrl -> L3 (Crouch)
            key(b'C'), // C -> R3 (Look behind), not RT
            key(b'E'), // E -> D-pad Right (Interact)
            key(b'Z'), // Z -> D-pad Down (Minimap), not LT
            key(b'V'), // V -> View (Camera)
            key(0x1B), // Esc -> Menu
        ];
        let mapped = fallback_for(&keys, true);
        assert_eq!(
            mapped.buttons,
            A | X
                | B
                | Y
                | LEFT_SHOULDER
                | RIGHT_SHOULDER
                | LEFT_STICK
                | RIGHT_STICK
                | RIGHT
                | DOWN
                | VIEW
                | MENU
        );
        assert_eq!(mapped.left_y, 1.0);
        assert_eq!(mapped.left_trigger, 0.0);
        assert_eq!(mapped.right_trigger, 0.0);

        // Mouse pad merges cleanly into the stand-in pad.
        let (hub, host) = fake_with(true);
        {
            let mut h = host.lock().unwrap();
            h.keys = vec![key(b'W')];
            h.mouse = PadState {
                buttons: RIGHT_STICK,
                left_trigger: 1.0,
                right_trigger: 1.0,
                right_x: 0.75,
                right_y: -0.25,
                ..Default::default()
            };
        }
        hub.poll();
        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        let got = pad(r);
        assert_eq!(got.buttons, RIGHT_STICK);
        assert_eq!(
            (
                got.left_y,
                got.left_trigger,
                got.right_trigger,
                got.right_x,
                got.right_y
            ),
            (1.0, 1.0, 1.0, 0.75, -0.25)
        );
        release(r);

        let mut look = MouseLook::default();
        let (mut rx, mut ry) = (0.0, 0.0);
        for _ in 0..4 {
            (rx, ry) = look.step(12, -6, POLL, 1.0, false);
        }
        assert!(
            (0.3..=1.0).contains(&rx),
            "mouse right should push +right_x, got {rx}"
        );
        assert!(
            (0.15..=1.0).contains(&ry),
            "mouse up (-dy) should push +right_y, got {ry}"
        );
        // Inverted Y flips the vertical axis.
        look.reset();
        let (_, ry_inv) = look.step(0, -10, POLL, 1.0, true);
        assert!(ry_inv < -0.15, "invert_y should flip right_y, got {ry_inv}");
        // Decays back to zero when the mouse stops moving.
        for _ in 0..40 {
            look.step(0, 0, POLL, 1.0, false);
        }
        assert_eq!(look.step(0, 0, POLL, 1.0, false), (0.0, 0.0));
        // Low-sensitivity 1-pixel movement is not swallowed by the rest deadzone.
        let (fine_x, _) = look.step(1, 0, POLL, 0.5, false);
        assert!(
            fine_x > 0.0,
            "1px low-sens motion should stay non-zero, got {fine_x}"
        );
        // A short mouse stroke clears console right-stick deadzone and settles quickly.
        look.reset();
        let (mut stroke_x, _) = (0.0, 0.0);
        for _ in 0..3 {
            (stroke_x, _) = look.step(5, 0, POLL, 1.0, false);
        }
        assert!(
            stroke_x >= 0.25,
            "short 5px stroke should clear deadzone (>=0.25), got {stroke_x}"
        );
        for _ in 0..40 {
            look.step(0, 0, POLL, 1.0, false);
        }
        assert_eq!(look.step(0, 0, POLL, 1.0, false), (0.0, 0.0));

        // Strict boolean env parsing rejects "false", "0", " 0 ", and "".
        assert!(parse_env_flag("1"));
        assert!(parse_env_flag(" true "));
        assert!(parse_env_flag("YES"));
        assert!(!parse_env_flag("0"));
        assert!(!parse_env_flag(" 0 "));
        assert!(!parse_env_flag("false"));
        assert!(!parse_env_flag(""));

        // Dragging a held button from outside (chrome / another app) into the
        // client area must not fire triggers until released and pressed fresh.
        let mut buttons = MouseButtons::default();
        assert_eq!(buttons.update([true, false, false], true), [false; 3]);
        assert_eq!(buttons.update([true, false, false], true), [false; 3]);
        assert_eq!(buttons.update([false, false, false], true), [false; 3]);
        assert_eq!(buttons.update([true, false, false], false), [false; 3]);
        assert_eq!(buttons.update([true, false, false], true), [false; 3]);
        assert_eq!(buttons.update([false, false, false], true), [false; 3]);
        assert_eq!(
            buttons.update([true, false, true], true),
            [true, false, true]
        );
        assert_eq!(
            buttons.update([true, false, true], false),
            [true, false, true]
        );
        assert_eq!(buttons.update([false, false, false], false), [false; 3]);
    }

    #[test]
    fn physical_controller_takeover_disarms_mouse_pad_when_not_one_pad() {
        let (hub, host) = fake_with(false);
        host.lock().unwrap().pads[0] = Some(PadState {
            buttons: A,
            ..Default::default()
        });
        hub.poll();
        assert_eq!(host.lock().unwrap().mouse_active_log.last(), Some(&false));

        host.lock().unwrap().pads[0] = None;
        hub.poll();
        assert_eq!(host.lock().unwrap().mouse_active_log.last(), Some(&true));
    }

    #[test]
    fn azerty_scancodes_map_zqsd_and_cover_without_conflicts() {
        let sc_key = |scan_code: u32, virtual_key: u8| KeyState {
            scan_code,
            virtual_key,
            ..Default::default()
        };
        // AZERTY physical 'A' (scancode 0x10) maps strictly to Cover (RB), not strafe-left.
        let cover = fallback_for_mode(&[sc_key(0x10, b'A')], true, true);
        assert_eq!(cover.buttons, RIGHT_SHOULDER);
        assert_eq!(cover.left_x, 0.0);

        // AZERTY physical 'Q' (scancode 0x1E) maps strictly to strafe-left, not Cover.
        let left = fallback_for_mode(&[sc_key(0x1E, b'Q')], true, true);
        assert_eq!(left.buttons, 0);
        assert_eq!(left.left_x, -1.0);

        // AZERTY physical 'Z' (scancode 0x11) maps strictly to forward, not D-pad Down.
        let fwd = fallback_for_mode(&[sc_key(0x11, b'Z')], true, true);
        assert_eq!(fwd.buttons, 0);
        assert_eq!(fwd.left_y, 1.0);

        // AZERTY physical 'W' (scancode 0x2C) maps to D-pad Down, not forward.
        let down = fallback_for_mode(&[sc_key(0x2C, b'W')], true, true);
        assert_eq!(down.buttons, DOWN);
        assert_eq!(down.left_y, 0.0);

        // AZERTY physical 'S' (0x1F) and 'D' (0x20).
        let back_right = fallback_for_mode(&[sc_key(0x1F, b'S'), sc_key(0x20, b'D')], true, true);
        assert_eq!((back_right.left_x, back_right.left_y), (1.0, -1.0));
    }

    #[test]
    fn mouse_capture_auto_locks_when_focused_and_respects_f1_f2_esc() {
        let mut cap = MouseCapture::default();

        // Auto-locks on first focused poll while F1 menu is closed.
        assert_eq!(cap.step(true, false, false, false), (true, true));
        assert_eq!(cap.step(true, false, false, false), (true, false));

        // Opening F1 menu unlocks, and closing it with Esc held auto-resumes without latching pause.
        assert_eq!(cap.step(true, true, false, true), (false, false));
        assert_eq!(cap.step(true, false, false, true), (true, true));
        assert_eq!(cap.step(true, false, false, false), (true, false));

        // Explicit F2 / `~` toggle pauses mouse look until toggled back on.
        assert_eq!(cap.step(true, false, true, false), (false, false));
        assert_eq!(cap.step(true, false, false, false), (false, false));
        assert_eq!(cap.step(true, false, true, false), (true, true));

        // Esc / P while locked unlocks and stays paused until F2 / `~` is pressed.
        assert_eq!(cap.step(true, false, false, true), (false, false));
        assert_eq!(cap.step(true, false, false, false), (false, false));
        assert_eq!(cap.step(true, false, true, false), (true, true));

        // Focus loss unlocks temporarily and auto-resumes on refocus.
        assert_eq!(cap.step(false, false, false, false), (false, false));
        assert_eq!(cap.step(true, false, false, false), (true, true));
    }

    #[test]
    fn mouse_look_is_smooth_and_continuous_without_step_impulses() {
        let mut look = MouseLook::default();
        let mut prev_x = 0.0;
        for i in 0..20 {
            let (rx, ry) = look.step(2, 0, POLL, 1.5, false);
            assert_eq!(ry, 0.0);
            assert!(
                rx >= prev_x - 1e-5,
                "continuous slow pan should be monotonic at step {i}: {prev_x} -> {rx}"
            );
            assert!(
                (rx - prev_x).abs() < 0.12,
                "adjacent step delta should be smooth (<0.12) at step {i}: {prev_x} -> {rx}"
            );
            prev_x = rx;
        }
        assert!(
            (0.25..=0.80).contains(&prev_x),
            "continuous slow pan should reach smooth steady plateau, got {prev_x}"
        );

        // Single 1px micro-step must not produce an abrupt jump.
        look.reset();
        let (micro_x, _) = look.step(1, 0, POLL, 0.5, false);
        assert!(
            micro_x > 0.0 && micro_x < 0.12,
            "1px micro-step should be gentle, got {micro_x}"
        );

        // Fast continuous sweep reaches high turn rate cleanly.
        look.reset();
        let mut fast_x = 0.0;
        for _ in 0..8 {
            (fast_x, _) = look.step(8, 0, POLL, 2.0, false);
        }
        assert!(
            fast_x >= 0.85,
            "fast continuous sweep should reach high turn rate (>=0.85), got {fast_x}"
        );
    }

    #[test]
    fn overlay_hotkeys_are_filtered_without_affecting_f2_or_game_keys() {
        assert!(super::is_overlay_hotkey_vk(0x70)); // F1
        assert!(!super::is_overlay_hotkey_vk(0x71)); // F2 mouse-look toggle
        assert!(super::is_overlay_hotkey_vk(0x72)); // F3 HUD mode
        assert!(super::is_overlay_hotkey_vk(0x73)); // F4 static cache
        assert!(super::is_overlay_hotkey_vk(0x74)); // F5 resolution scale
        assert!(!super::is_overlay_hotkey_vk(0xC0)); // ` / ~ mouse-look toggle
        assert!(!super::is_overlay_hotkey_vk(b'W'));
        assert!(!super::is_overlay_hotkey_vk(0x1B)); // Esc
    }
}
