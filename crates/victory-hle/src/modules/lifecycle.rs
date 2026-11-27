//! The title's life cycle. A console title is never told it is closing:
//! quitting it or launching a new game first constrains it (its
//! constrained-change routines run), then suspends it (its suspend routines
//! run, and it has a second to save) and then ends the process. Closing the
//! title's window does the same here; a new launch then starts `victory`
//! again, which waits for this process to be gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
use windows_sys::Win32::UI::WindowsAndMessaging::{self as wm, HMENU};
use windows_sys::core::{PCSTR, PCWSTR, w};

use crate::{HleModule, hle_export};

/// A suspend routine (`quiesced`) or a constrained-change routine
/// (`constrained`): both take a flag and the title's context.
type Routine = extern "system" fn(flag: u8, context: *mut c_void);

/// Registration id, routine and context.
type Registration = (usize, Routine, usize);

/// Each suspend routine.
static ROUTINES: Mutex<Vec<Registration>> = Mutex::new(Vec::new());

/// Each constrained-change routine.
static CONSTRAINED: Mutex<Vec<Registration>> = Mutex::new(Vec::new());

fn routines() -> MutexGuard<'static, Vec<Registration>> {
    ROUTINES.lock().unwrap_or_else(|e| e.into_inner())
}

fn constrained() -> MutexGuard<'static, Vec<Registration>> {
    CONSTRAINED.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long the console lets suspend routines run before it ends the title.
const SUSPEND_DEADLINE: Duration = Duration::from_secs(1);

const ERROR_INVALID_PARAMETER: u32 = 87;

/// Adds `routine` to `list` and writes its registration id.
fn register(
    mut list: MutexGuard<'static, Vec<Registration>>,
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    let Some(routine) = routine else {
        return ERROR_INVALID_PARAMETER;
    };
    if registration.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    list.push((id, routine, context as usize));
    // SAFETY: checked non-null; the title passes room for its registration.
    unsafe { registration.write(id) };
    0
}

pub extern "system" fn RegisterAppStateChangeNotification(
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    register(routines(), routine, context, registration)
}

pub extern "system" fn UnregisterAppStateChangeNotification(registration: usize) {
    routines().retain(|&(id, ..)| id != registration);
}

/// The host's own version never calls the routine (a desktop app is never
/// constrained), so a title waiting for it would never suspend.
pub extern "system" fn RegisterAppConstrainedChangeNotification(
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    register(constrained(), routine, context, registration)
}

pub extern "system" fn UnregisterAppConstrainedChangeNotification(registration: usize) {
    constrained().retain(|&(id, ..)| id != registration);
}

/// The window procedure a title window had before ours.
const TITLE_PROC: PCWSTR = w!("victory.wndproc");

const WM_INPUT: u32 = 0x00FF;
const RID_INPUT: u32 = 0x1000_0003;
const RIM_TYPEMOUSE: u32 = 0;
const MOUSE_MOVE_ABSOLUTE: u16 = 1;

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
    w_param: WPARAM,
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

const _: () = assert!(
    size_of::<RawInputHeader>() == 24 && size_of::<RawMouse>() == 24 && size_of::<RawInputMousePacket>() == 48
);

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

static SAVED_WINDOWED_RECT: Mutex<Option<RECT>> = Mutex::new(None);
static IS_BORDERLESS: AtomicBool = AtomicBool::new(false);

fn borderless_requested() -> bool {
    std::env::var("VICTORY_BORDERLESS")
        .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
}

fn apply_window_mode(parent: HWND, style: u32, x: i32, y: i32, width: i32, height: i32) -> (u32, i32, i32, i32, i32) {
    if parent.is_null() && borderless_requested() {
        // SAFETY: plain Win32 primary display metrics query.
        let (sw, sh) = unsafe {
            (wm::GetSystemMetrics(wm::SM_CXSCREEN), wm::GetSystemMetrics(wm::SM_CYSCREEN))
        };
        if sw > 0 && sh > 0 {
            IS_BORDERLESS.store(true, Ordering::Relaxed);
            return (wm::WS_POPUP | wm::WS_VISIBLE, 0, 0, sw, sh);
        }
    }
    if parent.is_null() {
        IS_BORDERLESS.store(false, Ordering::Relaxed);
    }
    let (fx, fy, fw, fh) = fit(parent, x, y, width, height);
    (style, fx, fy, fw, fh)
}

fn apply_live_window_mode(hwnd: HWND, borderless: bool) {
    if hwnd.is_null() {
        return;
    }
    if borderless {
        if !IS_BORDERLESS.swap(true, Ordering::Relaxed) {
            let mut rc = RECT::default();
            // SAFETY: valid RECT and live HWND.
            if unsafe { wm::GetWindowRect(hwnd, &mut rc) } != 0
                && (rc.right - rc.left) >= 320
                && (rc.bottom - rc.top) >= 200
            {
                *SAVED_WINDOWED_RECT.lock().unwrap_or_else(|e| e.into_inner()) = Some(rc);
            }
        }
        // SAFETY: plain Win32 screen metrics and style/position update on our window's UI thread.
        unsafe {
            let sw = wm::GetSystemMetrics(wm::SM_CXSCREEN);
            let sh = wm::GetSystemMetrics(wm::SM_CYSCREEN);
            if sw > 0 && sh > 0 {
                wm::SetWindowLongPtrW(hwnd, wm::GWL_STYLE, (wm::WS_POPUP | wm::WS_VISIBLE) as isize);
                wm::SetWindowPos(
                    hwnd,
                    wm::HWND_TOP,
                    0,
                    0,
                    sw,
                    sh,
                    wm::SWP_FRAMECHANGED | wm::SWP_NOOWNERZORDER | wm::SWP_SHOWWINDOW,
                );
            }
        }
    } else {
        IS_BORDERLESS.store(false, Ordering::Relaxed);
        let saved = *SAVED_WINDOWED_RECT.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: plain Win32 workarea query and style/position update on our window's UI thread.
        unsafe {
            let (left, top, w, h) = if let Some(rc) = saved {
                (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top)
            } else {
                let mut area = RECT::default();
                let ok = wm::SystemParametersInfoW(
                    wm::SPI_GETWORKAREA,
                    0,
                    (&raw mut area).cast(),
                    0,
                );
                let (aw, ah) = if ok != 0 {
                    (area.right - area.left, area.bottom - area.top)
                } else {
                    (1600, 900)
                };
                let (cw, ch) = fit_into(
                    &RECT {
                        left: 0,
                        top: 0,
                        right: (aw * 85) / 100,
                        bottom: (ah * 85) / 100,
                    },
                    1280,
                    720,
                );
                let mut rc = RECT {
                    left: 0,
                    top: 0,
                    right: cw.max(640),
                    bottom: ch.max(360),
                };
                let _ = wm::AdjustWindowRect(&mut rc, wm::WS_OVERLAPPEDWINDOW, 0);
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;
                let left = area.left + (aw - w) / 2;
                let top = area.top + (ah - h) / 2;
                (left, top, w, h)
            };
            wm::SetWindowLongPtrW(
                hwnd,
                wm::GWL_STYLE,
                (wm::WS_OVERLAPPEDWINDOW | wm::WS_VISIBLE) as isize,
            );
            wm::SetWindowPos(
                hwnd,
                wm::HWND_NOTOPMOST,
                left,
                top,
                w,
                h,
                wm::SWP_FRAMECHANGED | wm::SWP_NOOWNERZORDER | wm::SWP_SHOWWINDOW,
            );
        }
    }
}

fn restart_from_menu() {
    end_title(
        "F1 menu restart",
        Box::new(|| relaunch(std::env::args_os().skip(1).collect())),
    );
}

fn quit_from_menu() {
    end_title("F1 menu quit", Box::new(|| {}));
}

/// Formats a clean window title for the host/game window.
pub fn format_window_title(original: &str) -> String {
    if let Ok(custom) = std::env::var("VICTORY_WINDOW_TITLE") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let trimmed = original.trim();
    if trimmed.is_empty()
        || matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "victory" | "game_scarlett_master" | "game_scarlett_master.exe" | "app" | "title" | "window"
        )
    {
        return "Tequesta - Grand Theft Auto VI".to_owned();
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("tequesta") || lower.contains("grand theft auto") {
        return trimmed.to_owned();
    }
    format!("{trimmed} - Tequesta")
}

/// Sets the console process title on Windows (`SetConsoleTitleW`).
pub fn set_process_console_title(title: &str) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleTitleW(lp_console_title: PCWSTR) -> i32;
    }
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated UTF-16 string.
    unsafe {
        let _ = SetConsoleTitleW(wide.as_ptr());
    }
}

fn apply_initial_window_title(hwnd: HWND, original: &str) {
    if hwnd.is_null() {
        return;
    }
    let title = format_window_title(original);
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: valid HWND and NUL-terminated UTF-16 string.
    unsafe {
        let _ = wm::SetWindowTextW(hwnd, wide.as_ptr());
    }
}

pub extern "system" fn CreateWindowExW(
    ex_style: u32,
    class: PCWSTR,
    name: PCWSTR,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    menu: HMENU,
    instance: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let (style, x, y, width, height) = apply_window_mode(parent, style, x, y, width, height);
    // SAFETY: the title's own arguments, forwarded (the size fitted).
    let hwnd = unsafe {
        wm::CreateWindowExW(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    if parent.is_null() {
        let orig = if name.is_null() {
            String::new()
        } else {
            // SAFETY: non-null NUL-terminated UTF-16 string from caller.
            let len = unsafe { (0..512).position(|i| *name.add(i) == 0).unwrap_or(512) };
            // SAFETY: bounded slice of `name`.
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(name, len) })
        };
        apply_initial_window_title(hwnd, &orig);
    }
    hook(hwnd, parent.is_null());
    hwnd
}

/// See [`CreateWindowExW`].
pub extern "system" fn CreateWindowExA(
    ex_style: u32,
    class: PCSTR,
    name: PCSTR,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    menu: HMENU,
    instance: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let (style, x, y, width, height) = apply_window_mode(parent, style, x, y, width, height);
    // SAFETY: the title's own arguments, forwarded (the size fitted).
    let hwnd = unsafe {
        wm::CreateWindowExA(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    if parent.is_null() {
        let orig = if name.is_null() {
            String::new()
        } else {
            // SAFETY: non-null NUL-terminated C string from caller.
            unsafe { std::ffi::CStr::from_ptr(name.cast()) }
                .to_string_lossy()
                .into_owned()
        };
        apply_initial_window_title(hwnd, &orig);
    }
    hook(hwnd, parent.is_null());
    hwnd
}

/// A console title sizes its window to its render target (
/// the title: 3840x2160 on a Series X), which can exceed the host's screen.
/// A top-level window larger than the work area is shrunk to fit, keeping
/// its proportions, and centred; what it shows is scaled to it (the swap
/// chain stretches). `VICTORY_WINDOW_MAX=WIDTHxHEIGHT` applies a smaller
/// limit without changing the title's render targets.
fn fit(parent: HWND, x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32) {
    let mut area = RECT::default();
    // SAFETY: `area` receives the primary monitor's work area.
    let known =
        unsafe { wm::SystemParametersInfoW(wm::SPI_GETWORKAREA, 0, (&raw mut area).cast(), 0) };
    let limit = window_limit();
    let fitted = fit_with_limit(&area, width, height, limit);
    if !parent.is_null() || known == 0 || fitted == (width, height) {
        return (x, y, width, height);
    }
    tracing::info!(width, height, to = ?fitted, ?limit, "title window fitted to the screen");
    let (w, h) = fitted;
    let left = area.left + (area.right - area.left - w) / 2;
    let top = area.top + (area.bottom - area.top - h) / 2;
    (left, top, w, h)
}

fn parse_window_limit(value: &str) -> Option<(i32, i32)> {
    let (width, height) = value.trim().split_once(['x', 'X'])?;
    let (width, height) = (width.trim().parse().ok()?, height.trim().parse().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn window_limit() -> Option<(i32, i32)> {
    static LIMIT: std::sync::OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        let value = std::env::var_os("VICTORY_WINDOW_MAX")?;
        let parsed = value.to_str().and_then(parse_window_limit);
        if parsed.is_none() {
            tracing::warn!(
                ?value,
                "ignoring invalid VICTORY_WINDOW_MAX; expected positive WIDTHxHEIGHT"
            );
        }
        parsed
    })
}

fn fit_with_limit(
    area: &RECT,
    width: i32,
    height: i32,
    limit: Option<(i32, i32)>,
) -> (i32, i32) {
    let Some((max_width, max_height)) = limit else {
        return fit_into(area, width, height);
    };
    let bounded = RECT {
        left: 0,
        top: 0,
        right: (area.right - area.left).min(max_width),
        bottom: (area.bottom - area.top).min(max_height),
    };
    fit_into(&bounded, width, height)
}

/// `width` x `height` scaled down to fit `area`, or unchanged if it fits
/// (or is `CW_USEDEFAULT`).
fn fit_into(area: &RECT, width: i32, height: i32) -> (i32, i32) {
    let (aw, ah) = (area.right - area.left, area.bottom - area.top);
    if width <= 0 || height <= 0 || aw <= 0 || ah <= 0 || (width <= aw && height <= ah) {
        return (width, height);
    }
    let scale = f64::min(
        f64::from(aw) / f64::from(width),
        f64::from(ah) / f64::from(height),
    );
    (
        (f64::from(width) * scale) as i32,
        (f64::from(height) * scale) as i32,
    )
}

/// Puts our procedure in front of the title's and registers F1 menu + Raw Input mouse.
fn hook(hwnd: HWND, top_level: bool) {
    if hwnd.is_null() {
        return;
    }
    static UI_ACTIONS: std::sync::Once = std::sync::Once::new();
    UI_ACTIONS.call_once(|| {
        victory_gpu::tequesta_ui::register_actions(restart_from_menu, quit_from_menu);
    });
    if top_level {
        let rid = RawInputDevice {
            us_usage_page: 0x01,
            us_usage: 0x02,
            dw_flags: 0,
            hwnd_target: hwnd,
        };
        // SAFETY: registers standard HID mouse raw input for this top-level window.
        let ok = unsafe { RegisterRawInputDevices(&rid, 1, size_of::<RawInputDevice>() as u32) };
        if ok == 0 {
            tracing::warn!("RegisterRawInputDevices failed for mouse raw input");
        }
    }
    // SAFETY: a window this thread just created. The title's procedure is
    // stored before ours can run.
    unsafe {
        let title = wm::GetWindowLongPtrW(hwnd, wm::GWLP_WNDPROC);
        wm::SetPropW(hwnd, TITLE_PROC, title as *mut c_void);
        wm::SetWindowLongPtrW(hwnd, wm::GWLP_WNDPROC, window_proc as *const () as isize);
    }
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == victory_gpu::tequesta_ui::WM_APP_DISPLAY_MODE {
        apply_live_window_mode(hwnd, wparam != 0);
        return 0;
    }
    if message == WM_INPUT && !super::gameinput::raw_pump_active() {
        let mut buf = [0u8; 64];
        let mut size = buf.len() as u32;
        // SAFETY: `lparam` is an `HRAWINPUT` handle for this `WM_INPUT` message.
        let read = unsafe {
            GetRawInputData(
                lparam,
                RID_INPUT,
                buf.as_mut_ptr().cast(),
                &mut size,
                size_of::<RawInputHeader>() as u32,
            )
        };
        if read != u32::MAX && (read as usize) >= size_of::<RawInputMousePacket>() {
            // SAFETY: `buf` is at least `size_of::<RawInputMousePacket>()` bytes.
            let pkt = unsafe { buf.as_ptr().cast::<RawInputMousePacket>().read_unaligned() };
            if pkt.header.dw_type == RIM_TYPEMOUSE
                && (pkt.mouse.us_flags & MOUSE_MOVE_ABSOLUTE) == 0
                && (pkt.mouse.l_last_x != 0 || pkt.mouse.l_last_y != 0)
            {
                super::gameinput::push_raw_mouse_delta(pkt.mouse.l_last_x, pkt.mouse.l_last_y);
            }
        }
    }
    // A user close (close button, Alt+F4, window or taskbar menu); a
    // console title never sees one, nor a WM_CLOSE from it.
    if message == wm::WM_SYSCOMMAND && wparam & 0xFFF0 == wm::SC_CLOSE as usize {
        end_title("window closed", Box::new(|| {}));
        return 0;
    }
    // Alt or F10 puts a window into menu mode: the system runs a modal menu
    // loop on the thread that owns the window, which is the title's. The
    // title then stops (no frames, no input) until a key ends the loop, and
    // nothing shows it, since the window has no menu bar;
    // the title aborts itself about half a minute later. A console has no window
    // menu. The mouse still opens it from the title bar.
    if message == wm::WM_SYSCOMMAND && wparam & 0xFFF0 == wm::SC_KEYMENU as usize {
        return 0;
    }
    // Auto-hide the Windows cursor inside the game window's client area while
    // GTA mouse-look lock (`~` / `F2`) is active, and restore it immediately
    // when unlocked (`~` / `F2` / `Esc` / `P`) or when the F1 menu is open.
    if message == wm::WM_SETCURSOR && (lparam as u32 & 0xFFFF) == wm::HTCLIENT {
        static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
        if victory_gpu::tequesta_ui::menu_open() {
            CURSOR_HIDDEN.store(false, Ordering::Relaxed);
            // SAFETY: shows standard arrow cursor inside our window while F1 menu is open.
            unsafe {
                let arrow = wm::LoadCursorW(std::ptr::null_mut(), wm::IDC_ARROW);
                wm::SetCursor(arrow);
            }
            return 1;
        }
        if super::gameinput::cursor_locked() {
            CURSOR_HIDDEN.store(true, Ordering::Relaxed);
            // SAFETY: hides the cursor only inside our client area on the UI thread.
            unsafe { wm::SetCursor(std::ptr::null_mut()) };
            return 1;
        }
        if CURSOR_HIDDEN.swap(false, Ordering::Relaxed) {
            // SAFETY: let the title's procedure restore its cursor first; if it
            // leaves none set, fall back to the standard arrow.
            let res = unsafe {
                let title = wm::GetPropW(hwnd, TITLE_PROC);
                let title = std::mem::transmute::<*mut c_void, wm::WNDPROC>(title);
                wm::CallWindowProcW(title, hwnd, message, wparam, lparam)
            };
            // SAFETY: runs on this window's UI thread; restores the standard
            // arrow cursor if the title's window procedure left none set.
            unsafe {
                if wm::GetCursor().is_null() {
                    let arrow = wm::LoadCursorW(std::ptr::null_mut(), wm::IDC_ARROW);
                    wm::SetCursor(arrow);
                }
            }
            return res;
        }
    }
    // SAFETY: the procedure `hook` stored, called the way the system would.
    unsafe {
        let title = wm::GetPropW(hwnd, TITLE_PROC);
        if message == wm::WM_NCDESTROY {
            wm::RemovePropW(hwnd, TITLE_PROC);
        }
        let title = std::mem::transmute::<*mut c_void, wm::WNDPROC>(title);
        wm::CallWindowProcW(title, hwnd, message, wparam, lparam)
    }
}

/// Suspends the title as the console does when it ends it, runs `then`
/// (once), then ends the process. Returns at once; later calls do nothing.
pub(crate) fn end_title(why: &'static str, then: Box<dyn FnOnce() + Send>) {
    static ENDING: AtomicBool = AtomicBool::new(false);
    if ENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let constrain = constrained().clone();
    let suspend = routines().clone();
    tracing::info!(
        constrained = constrain.len(),
        routines = suspend.len(),
        "{why}: suspending the title"
    );
    // Whoever comes first (the routines or the deadline) runs `then`; the
    // other waits for it, so the process never ends halfway through.
    let then = std::sync::Arc::new(Mutex::new(Some(then)));
    let finish = move |how: &str| {
        let mut then = then.lock().unwrap_or_else(|e| e.into_inner());
        tracing::info!("{how}; ending the title");
        if let Some(then) = then.take() {
            then();
        }
        end();
    };
    let late = finish.clone();
    // Not on the calling thread: titles hand the suspend to their window
    // thread and wait for it inside the routine.
    std::thread::spawn(move || {
        constrain_then_suspend(&constrain, &suspend);
        finish("title suspended");
    });
    std::thread::spawn(move || {
        std::thread::sleep(SUSPEND_DEADLINE);
        late("title did not finish suspending in time");
    });
}

/// Runs the constrained-change routines (constrained), then the suspend
/// routines (quiesced), as the console does when it suspends a title.
/// Grand Theft Auto VI's suspend routine waits for a state that only its
/// constrained routine brings about; called alone it never returns.
fn constrain_then_suspend(constrain: &[Registration], suspend: &[Registration]) {
    for &(_, routine, context) in constrain.iter().chain(suspend) {
        routine(1, context as *mut c_void);
    }
}

/// Set in the environment of a relaunched `victory`: an inherited handle
/// of the process it replaces.
const RELAUNCHED_FROM: &str = "VICTORY_RELAUNCHED_FROM";

/// Starts `victory` again with `args` (after the title has suspended); the
/// new process waits until this one has ended.
pub(crate) fn relaunch(args: Vec<std::ffi::OsString>) {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    let mut me: HANDLE = std::ptr::null_mut();
    // SAFETY: an inheritable duplicate of our own process handle.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut me,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    let spawned = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        command.args(&args);
        if duplicated {
            command.env(RELAUNCHED_FROM, (me as usize).to_string());
        }
        command.spawn()
    });
    match spawned {
        Ok(child) => tracing::info!(pid = child.id(), ?args, "relaunched"),
        Err(e) => tracing::warn!(%e, "relaunch failed; the title just ends"),
    }
}

/// In a relaunched `victory`: waits (30 s at most) for the process it
/// replaces to be gone, so the two never share the title's files.
pub fn wait_for_previous() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let Some(handle) = std::env::var(RELAUNCHED_FROM)
        .ok()
        .and_then(|h| h.parse::<usize>().ok())
    else {
        return;
    };
    // SAFETY: removing our own variable before any other thread reads it.
    unsafe { std::env::remove_var(RELAUNCHED_FROM) };
    let handle = handle as HANDLE;
    // SAFETY: the inherited process handle the previous process gave us.
    unsafe {
        WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
    }
}

fn end() {
    // Compiled host pipelines kept for the next run.
    #[cfg(windows)]
    victory_gpu::pipeline::save_pipeline_cache();
    // SAFETY: ends this process, as the console ends a suspended title.
    unsafe { TerminateProcess(GetCurrentProcess(), 0) };
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "lifecycle",
        dlls: &[
            "api-ms-win-core-psm-appnotify-*",
            "ext-ms-win-rtcore-ntuser-window-*",
        ],
        exports: vec![
            hle_export!(RegisterAppStateChangeNotification),
            hle_export!(UnregisterAppStateChangeNotification),
            hle_export!(RegisterAppConstrainedChangeNotification),
            hle_export!(UnregisterAppConstrainedChangeNotification),
            hle_export!(CreateWindowExW),
            hle_export!(CreateWindowExA),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "system" fn noop(_: u8, _: *mut c_void) {}

    #[test]
    fn registrations_come_and_go() {
        let (mut a, mut b) = (0, 0);
        assert_eq!(
            RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut a),
            0
        );
        assert_eq!(
            RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut b),
            0
        );
        UnregisterAppStateChangeNotification(a);
        let left: Vec<_> = routines().iter().map(|r| r.0).collect();
        assert_eq!(left, [b]);
    }

    #[test]
    fn constrained_registrations_come_and_go() {
        let (mut a, mut b) = (0, 0);
        assert_eq!(
            RegisterAppConstrainedChangeNotification(Some(noop), std::ptr::null_mut(), &mut a),
            0
        );
        assert_eq!(
            RegisterAppConstrainedChangeNotification(Some(noop), std::ptr::null_mut(), &mut b),
            0
        );
        assert_eq!(
            RegisterAppConstrainedChangeNotification(None, std::ptr::null_mut(), &mut a),
            ERROR_INVALID_PARAMETER
        );
        UnregisterAppConstrainedChangeNotification(a);
        let left: Vec<_> = constrained().iter().map(|r| r.0).collect();
        assert_eq!(left, [b]);
    }

    /// What `recording_routine` saw: (context, flag) per call, in order.
    static CALLS: Mutex<Vec<(usize, u8)>> = Mutex::new(Vec::new());

    extern "system" fn recording_routine(flag: u8, context: *mut c_void) {
        CALLS.lock().unwrap().push((context as usize, flag));
    }

    #[test]
    fn constrained_routines_run_before_suspend_routines() {
        let r: Routine = recording_routine;
        constrain_then_suspend(&[(1, r, 10), (2, r, 11)], &[(3, r, 20), (4, r, 21)]);
        assert_eq!(*CALLS.lock().unwrap(), [(10, 1), (11, 1), (20, 1), (21, 1)]);
    }

    #[test]
    fn oversized_windows_shrink_to_the_screen() {
        let area = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(fit_into(&area, 3840, 2160), (1848, 1040));
        assert_eq!(fit_into(&area, 1280, 720), (1280, 720));
        assert_eq!(
            fit_into(&area, wm::CW_USEDEFAULT, 0),
            (wm::CW_USEDEFAULT, 0)
        );
    }

    #[test]
    fn configured_window_limit_fits_4k_into_1080p() {
        let area = RECT {
            left: 100,
            top: 50,
            right: 2660,
            bottom: 1490,
        };
        assert_eq!(fit_with_limit(&area, 3840, 2160, Some((1920, 1080))), (1920, 1080));
        assert_eq!(fit_with_limit(&area, 640, 480, Some((1920, 1080))), (640, 480));
        assert_eq!(fit_with_limit(&area, 3840, 2160, None), (2560, 1440));
    }

    #[test]
    fn configured_window_limit_still_respects_the_work_area() {
        let area = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(fit_with_limit(&area, 3840, 2160, Some((2560, 1440))), (1848, 1040));
        assert_eq!(
            fit_with_limit(&area, wm::CW_USEDEFAULT, 0, Some((1920, 1080))),
            (wm::CW_USEDEFAULT, 0)
        );
    }

    #[test]
    fn configured_window_limit_preserves_non_widescreen_proportions() {
        let area = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        assert_eq!(fit_with_limit(&area, 1600, 1200, Some((1920, 1080))), (1440, 1080));
    }

    #[test]
    fn window_limit_requires_two_positive_dimensions() {
        assert_eq!(parse_window_limit("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_window_limit(" 1920 X 1080 "), Some((1920, 1080)));
        for bad in [
            "",
            "1920",
            "0x1080",
            "1920x0",
            "-1x1080",
            "1920x-1",
            "1920x1080x2",
            "2147483648x1080",
        ] {
            assert_eq!(parse_window_limit(bad), None, "{bad}");
        }
    }

    #[test]
    fn hooked_windows_still_reach_the_title_procedure() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("before"),
            0,
            0,
            0,
            10,
            10,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        let mut text = [0u16; 16];
        // SAFETY: our window; the buffer holds 16 characters.
        unsafe {
            wm::SetWindowTextW(hwnd, w!("after"));
            let n = wm::GetWindowTextW(hwnd, text.as_mut_ptr(), 16);
            assert_eq!(String::from_utf16_lossy(&text[..n as usize]), "after");
            assert!(!wm::GetPropW(hwnd, TITLE_PROC).is_null());
            wm::DestroyWindow(hwnd);
        }
    }

    thread_local! {
        /// The system commands `recording` received, low four bits cleared.
        static COMMANDS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Stands in for a title's procedure: notes system commands and acts
    /// on none of them (no menu loop, no minimising).
    extern "system" fn recording(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == wm::WM_SYSCOMMAND {
            COMMANDS.with(|c| c.borrow_mut().push(wparam & 0xFFF0));
            return 0;
        }
        // SAFETY: the system's default procedure, called as the system would.
        unsafe { wm::DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    #[test]
    fn the_keyboard_menu_never_reaches_the_title() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("menu"),
            0,
            0,
            0,
            10,
            10,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        // SAFETY: our window, owned by this thread, so each message is
        // handled before SendMessageW returns. `recording` stands in for the
        // title's procedure and the original is put back before the window
        // is destroyed.
        unsafe {
            let title = wm::GetPropW(hwnd, TITLE_PROC);
            wm::SetPropW(hwnd, TITLE_PROC, recording as *const () as *mut c_void);
            let command = |c: u32, key: u8| {
                wm::SendMessageW(hwnd, wm::WM_SYSCOMMAND, c as usize, LPARAM::from(key))
            };
            command(wm::SC_KEYMENU, 0); // Alt or F10
            command(wm::SC_KEYMENU, b' '); // Alt+Space
            command(wm::SC_KEYMENU | 1, 0); // the low bits are the system's
            command(wm::SC_MINIMIZE, 0);
            wm::SetPropW(hwnd, TITLE_PROC, title);
            wm::DestroyWindow(hwnd);
        }
        let seen = COMMANDS.with(|c| c.borrow().clone());
        assert_eq!(seen, [wm::SC_MINIMIZE as usize]);
    }

    #[test]
    fn formats_window_titles_cleanly() {
        assert_eq!(format_window_title(""), "Tequesta - Grand Theft Auto VI");
        assert_eq!(format_window_title("VIctory"), "Tequesta - Grand Theft Auto VI");
        assert_eq!(
            format_window_title("game_scarlett_master.exe"),
            "Tequesta - Grand Theft Auto VI"
        );
        assert_eq!(
            format_window_title("Grand Theft Auto VI"),
            "Grand Theft Auto VI"
        );
        assert_eq!(format_window_title("Custom Title"), "Custom Title - Tequesta");
    }
}
