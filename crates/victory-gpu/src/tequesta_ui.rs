//! Native D3D12 `F1` overlay menu and compact top-right FPS/frametime HUD.
//!
//! - Toggle menu with `F1` (`VK_F1`).
//! - Hot settings (apply immediately): `VICTORY_PORT_STATIC`, FPS cap
//!   (`Unlimited` / `30` / `60`), mouse sensitivity (`0.25x..3.00x`), and
//!   compact FPS/ms corner counter.
//! - Restart-required settings (persisted to `tequesta_config.toml`):
//!   resolution scale (`50%` / `75%` / `100%`), windowed vs borderless
//!   fullscreen, `VICTORY_RAW_VIEWS`, and `VICTORY_KBM_NATIVE`.
//! - Action buttons: `APPLY & RESTART` and `QUIT GAME`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use victory_core::TequestaConfig;
use windows::Win32::Foundation::{HWND, POINT as WIN_POINT};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_DOWN, VK_ESCAPE, VK_F1, VK_LBUTTON, VK_LEFT, VK_RETURN, VK_RIGHT,
    VK_SPACE, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, HTCLIENT, PostMessageW,
    WM_MOUSEMOVE, WM_SETCURSOR,
};

use crate::host::{Host, HostList, transition};

const MENU_W: u32 = 520;
const MENU_H: u32 = 444;
const HUD_W: u32 = 176;
const HUD_H: u32 = 30;
const UPLOADS: usize = 4;

const BG: [u8; 3] = [16, 18, 24];
const HEADER_BG: [u8; 3] = [24, 28, 38];
const BORDER: [u8; 3] = [66, 76, 98];
const ROW_HL: [u8; 3] = [36, 48, 72];
const BTN_RESTART: [u8; 3] = [36, 92, 168];
const BTN_RESTART_HL: [u8; 3] = [54, 122, 214];
const BTN_QUIT: [u8; 3] = [138, 38, 46];
const BTN_QUIT_HL: [u8; 3] = [184, 52, 62];
const WHITE: [u8; 3] = [240, 244, 250];
const MUTED: [u8; 3] = [164, 174, 192];
const ACCENT: [u8; 3] = [104, 180, 255];
const GREEN: [u8; 3] = [124, 232, 92];
const ORANGE: [u8; 3] = [245, 168, 66];
const RED: [u8; 3] = [240, 82, 76];

static MENU_OPEN: AtomicBool = AtomicBool::new(false);
static FPS_CAP: OnceLock<AtomicU32> = OnceLock::new();
static MOUSE_SENS_X100: OnceLock<AtomicU32> = OnceLock::new();
static SHOW_HUD: OnceLock<AtomicBool> = OnceLock::new();
static KBM_NATIVE: OnceLock<AtomicBool> = OnceLock::new();
static CONFIG: OnceLock<Mutex<TequestaConfig>> = OnceLock::new();
static ACTIONS: OnceLock<(fn(), fn())> = OnceLock::new();

fn config_lock() -> &'static Mutex<TequestaConfig> {
    CONFIG.get_or_init(|| {
        let cfg = TequestaConfig::load();
        let _ = FPS_CAP.set(AtomicU32::new(cfg.fps_cap));
        let _ = MOUSE_SENS_X100.set(AtomicU32::new((cfg.mouse_sensitivity * 100.0).round() as u32));
        let _ = SHOW_HUD.set(AtomicBool::new(cfg.show_fps_hud));
        let _ = KBM_NATIVE.set(AtomicBool::new(cfg.kbm_native));
        crate::port_static::set_enabled(cfg.port_static);
        Mutex::new(cfg)
    })
}

fn ensure_init() {
    let _ = config_lock();
}

/// Registers the restart and quit callbacks from `victory-hle::modules::lifecycle`.
pub fn register_actions(on_restart: fn(), on_quit: fn()) {
    let _ = ACTIONS.set((on_restart, on_quit));
}

/// Whether the `F1` overlay menu is currently open.
#[inline]
pub fn menu_open() -> bool {
    MENU_OPEN.load(Relaxed)
}

/// Toggles the `F1` overlay menu.
pub fn toggle_menu() {
    ensure_init();
    MENU_OPEN.fetch_xor(true, Relaxed);
}

/// Current live FPS cap (`0` = unlimited, `30`, `60`).
#[inline]
pub fn fps_cap() -> u32 {
    ensure_init();
    FPS_CAP.get().map_or(0, |a| a.load(Relaxed))
}

/// Current live mouse sensitivity multiplier.
#[inline]
pub fn mouse_sensitivity() -> f32 {
    ensure_init();
    MOUSE_SENS_X100.get().map_or(100, |a| a.load(Relaxed)) as f32 / 100.0
}

/// Whether the compact top-right FPS/ms counter is enabled.
#[inline]
pub fn show_fps_hud() -> bool {
    ensure_init();
    SHOW_HUD.get().is_none_or(|a| a.load(Relaxed))
}

/// Whether `VICTORY_KBM_NATIVE` is enabled in config.
#[inline]
pub fn kbm_native_enabled() -> bool {
    ensure_init();
    KBM_NATIVE.get().is_none_or(|a| a.load(Relaxed))
}

fn foreground_ours() -> bool {
    let mut pid = 0u32;
    // SAFETY: plain Win32 foreground window PID query.
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    pid == std::process::id()
}

#[inline]
fn key_down(vk: u16) -> bool {
    // SAFETY: plain Win32 async key state query.
    unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
}

fn notify_cursor(hwnd: HWND) {
    if !hwnd.0.is_null() {
        let lparam = (HTCLIENT as isize) | ((WM_MOUSEMOVE as isize) << 16);
        // SAFETY: asynchronous post to our own process's window.
        unsafe {
            let _ = PostMessageW(hwnd.0.cast(), WM_SETCURSOR, hwnd.0 as usize, lparam);
        }
    }
}

#[rustfmt::skip]
const fn glyph(c: u8) -> [u8; 7] {
    match c {
        b'0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        b'1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'2' => [0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111],
        b'3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        b'4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        b'5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        b'6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        b'7' => [0b11111, 0b00001, 0b00010, 0b00010, 0b00100, 0b00100, 0b00100],
        b'8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        b'9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        b'A' => [0b00100, 0b01010, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001],
        b'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        b'C' => [0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111],
        b'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        b'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        b'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        b'G' => [0b01111, 0b10000, 0b10000, 0b10011, 0b10001, 0b10001, 0b01111],
        b'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        b'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        b'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        b'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        b'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        b'N' => [0b10001, 0b11001, 0b11001, 0b10101, 0b10011, 0b10011, 0b10001],
        b'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        b'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        b'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        b'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        b'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        b'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        b'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        b'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        b'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        b'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        b'.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        b'/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        b'-' => [0, 0, 0, 0b11111, 0, 0, 0],
        b'_' => [0, 0, 0, 0, 0, 0, 0b11111],
        b'|' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        b'%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        b':' => [0, 0b00100, 0b00100, 0, 0b00100, 0b00100, 0],
        b'<' => [0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010],
        b'>' => [0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000],
        b'&' => [0b01100, 0b10010, 0b10100, 0b01000, 0b10101, 0b10010, 0b01101],
        b' ' => [0; 7],
        _ => [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111],
    }
}

fn bpp(format: DXGI_FORMAT) -> Option<usize> {
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_R10G10B10A2_UNORM => Some(4),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(8),
        _ => None,
    }
}

fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let exp = ((bits >> 23) & 0xff) as i32 - 127;
    if exp < -14 {
        return 0;
    }
    let h_exp = ((exp + 15).clamp(0, 31) as u16) << 10;
    let h_mant = ((bits & 0x7f_ffff) >> 13) as u16;
    h_exp | h_mant
}

fn encode(format: DXGI_FORMAT, [r, g, b]: [u8; 3], dst: &mut [u8]) {
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => dst[..4].copy_from_slice(&[r, g, b, 0xFF]),
        DXGI_FORMAT_B8G8R8A8_UNORM => dst[..4].copy_from_slice(&[b, g, r, 0xFF]),
        DXGI_FORMAT_R10G10B10A2_UNORM => {
            let r10 = (u32::from(r) * 1023 / 255) & 0x3FF;
            let g10 = (u32::from(g) * 1023 / 255) & 0x3FF;
            let b10 = (u32::from(b) * 1023 / 255) & 0x3FF;
            let word = r10 | (g10 << 10) | (b10 << 20) | (0x3 << 30);
            dst[..4].copy_from_slice(&word.to_le_bytes());
        }
        DXGI_FORMAT_R16G16B16A16_FLOAT => {
            let to_h = |c: u8| f32_to_f16((f32::from(c) / 255.0).powi(2));
            dst[0..2].copy_from_slice(&to_h(r).to_le_bytes());
            dst[2..4].copy_from_slice(&to_h(g).to_le_bytes());
            dst[4..6].copy_from_slice(&to_h(b).to_le_bytes());
            dst[6..8].copy_from_slice(&0x3C00u16.to_le_bytes());
        }
        _ => {}
    }
}

struct Canvas {
    w: usize,
    h: usize,
    rgb: Vec<[u8; 3]>,
}

impl Canvas {
    fn new(w: u32, h: u32, fill: [u8; 3]) -> Self {
        Self { w: w as usize, h: h as usize, rgb: vec![fill; (w * h) as usize] }
    }

    fn rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
        let x1 = (x + w).min(self.w as u32) as usize;
        let y1 = (y + h).min(self.h as u32) as usize;
        for yy in (y as usize).min(y1)..y1 {
            self.rgb[yy * self.w + (x as usize).min(x1)..yy * self.w + x1].fill(color);
        }
    }

    fn border(&mut self, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
        if w == 0 || h == 0 {
            return;
        }
        self.rect(x, y, w, 1, color);
        self.rect(x, y + h - 1, w, 1, color);
        self.rect(x, y, 1, h, color);
        self.rect(x + w - 1, y, 1, h, color);
    }

    fn text(&mut self, mut x: u32, y: u32, px: u32, color: [u8; 3], text: &str) {
        let step = 6 * px;
        for ch in text.bytes() {
            let bits = glyph(ch.to_ascii_uppercase());
            for (row, &mask) in bits.iter().enumerate() {
                if mask == 0 {
                    continue;
                }
                for col in 0..5u32 {
                    if (mask >> (4 - col)) & 1 != 0 {
                        self.rect(x + col * px, y + row as u32 * px, px, px, color);
                    }
                }
            }
            x += step;
        }
    }

    fn pack(&self, format: DXGI_FORMAT, pitch: usize) -> Vec<u8> {
        let bytes = bpp(format).unwrap_or(4);
        let mut out = vec![0u8; pitch * self.h];
        let mut enc = [0u8; 8];
        for y in 0..self.h {
            let top = y * pitch;
            for x in 0..self.w {
                encode(format, self.rgb[y * self.w + x], &mut enc[..bytes]);
                let at = top + x * bytes;
                out[at..at + bytes].copy_from_slice(&enc[..bytes]);
            }
        }
        out
    }
}

struct Surface {
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    pitch: u32,
    uploads: [(ID3D12Resource, *mut u8); UPLOADS],
    card: ID3D12Resource,
    turn: usize,
    fresh: bool,
    uploaded: bool,
}

// SAFETY: upload pointers stay mapped for the lifetime of their committed D3D12 upload buffers.
unsafe impl Send for Surface {}

impl Surface {
    fn new(host: &Host, w: u32, h: u32, format: DXGI_FORMAT) -> Option<Self> {
        let bytes_per_pixel = bpp(format)?;
        let pitch = (w as usize * bytes_per_pixel).next_multiple_of(256) as u32;
        let bytes = u64::from(pitch) * u64::from(h);
        let mut uploads = Vec::with_capacity(UPLOADS);
        for _ in 0..UPLOADS {
            uploads.push(host.upload_buffer(bytes)?);
        }
        let uploads: [(ID3D12Resource, *mut u8); UPLOADS] = uploads.try_into().ok()?;
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut card: Option<ID3D12Resource> = None;
        // SAFETY: plain D3D12 default buffer creation.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut card)
                .ok()?;
        }
        Some(Self {
            w,
            h,
            format,
            pitch,
            uploads,
            card: card?,
            turn: 0,
            fresh: false,
            uploaded: false,
        })
    }

    fn upload(&mut self, canvas: &Canvas) {
        let packed = canvas.pack(self.format, self.pitch as usize);
        self.turn = (self.turn + 1) % UPLOADS;
        // SAFETY: `uploads[turn]` is persistently mapped for `pitch * h` bytes.
        unsafe { std::ptr::copy_nonoverlapping(packed.as_ptr(), self.uploads[self.turn].1, packed.len()) };
        self.fresh = true;
    }

    fn blit(&mut self, list: &HostList, back: &ID3D12Resource, dst_x: u32, dst_y: u32, max_w: u32, max_h: u32) {
        let l = &list.list;
        let copy_w = self.w.min(max_w.saturating_sub(dst_x));
        let copy_h = self.h.min(max_h.saturating_sub(dst_y));
        if copy_w == 0 || copy_h == 0 {
            return;
        }
        // SAFETY: live D3D12 resources and open copy-dest backbuffer command list.
        unsafe {
            if std::mem::take(&mut self.fresh) {
                if self.uploaded {
                    l.ResourceBarrier(&[transition(
                        &self.card,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    )]);
                }
                l.CopyResource(&self.card, &self.uploads[self.turn].0);
                l.ResourceBarrier(&[transition(
                    &self.card,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                self.uploaded = true;
            }
            if !self.uploaded {
                return;
            }
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.card),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: self.format,
                            Width: self.w,
                            Height: self.h,
                            Depth: 1,
                            RowPitch: self.pitch,
                        },
                    },
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(back),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let part = D3D12_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: copy_w,
                bottom: copy_h,
                back: 1,
            };
            l.CopyTextureRegion(&to, dst_x, dst_y, 0, &from, Some(&part));
        }
    }
}

const ROW_COUNT: usize = 10;

/// Row y-coordinates inside the `MENU_W x MENU_H` canvas.
const fn row_y(idx: usize) -> u32 {
    match idx {
        0 => 76,
        1 => 106,
        2 => 136,
        3 => 166,
        4 => 222,
        5 => 252,
        6 => 282,
        7 => 312,
        8 | 9 => 378,
        _ => 0,
    }
}

fn draw_menu_row(c: &mut Canvas, selected: usize, idx: usize, label: &str, val: &str, val_col: [u8; 3]) {
    let y = row_y(idx);
    if selected == idx {
        c.rect(10, y - 3, MENU_W - 20, 24, ROW_HL);
        c.text(16, y + 2, 2, ACCENT, ">");
    }
    c.text(34, y + 2, 2, WHITE, label);
    let vx = MENU_W - 20 - (val.len() as u32 * 12);
    c.text(vx, y + 2, 2, val_col, val);
}

fn paint_menu(cfg: &TequestaConfig, selected: usize, pending_restart: bool) -> Canvas {
    let mut c = Canvas::new(MENU_W, MENU_H, BG);
    c.border(0, 0, MENU_W, MENU_H, BORDER);
    c.rect(1, 1, MENU_W - 2, 42, HEADER_BG);
    c.text(16, 14, 2, ACCENT, "TEQUESTA MENU F1");
    let tag = if pending_restart { "RESTART REQUIRED" } else { "READY" };
    let tag_col = if pending_restart { ORANGE } else { GREEN };
    c.text(MENU_W - 14 - (tag.len() as u32 * 12), 16, 2, tag_col, tag);

    c.text(16, 54, 2, ACCENT, "LIVE SETTINGS");
    draw_menu_row(&mut c, selected, 0, "VICTORY_PORT_STATIC", if cfg.port_static { "ON" } else { "OFF" }, if cfg.port_static { GREEN } else { RED });
    let fps_str = match cfg.fps_cap {
        30 => "30 FPS",
        60 => "60 FPS",
        _ => "UNLIMITED",
    };
    draw_menu_row(&mut c, selected, 1, "FPS CAP", fps_str, ACCENT);
    let sens_str = format!("{:.2}X", cfg.mouse_sensitivity);
    draw_menu_row(&mut c, selected, 2, "MOUSE SENSITIVITY", &sens_str, WHITE);
    draw_menu_row(&mut c, selected, 3, "FPS/MS COUNTER", if cfg.show_fps_hud { "ON" } else { "OFF" }, if cfg.show_fps_hud { GREEN } else { MUTED });

    c.rect(14, 194, MENU_W - 28, 1, BORDER);
    c.text(16, 200, 2, ORANGE, "RESTART REQUIRED");

    let res_str = format!("{}%", cfg.resolution_scale);
    draw_menu_row(&mut c, selected, 4, "RESOLUTION SCALE", &res_str, WHITE);
    draw_menu_row(
        &mut c,
        selected,
        5,
        "DISPLAY MODE",
        if cfg.borderless_fullscreen { "BORDERLESS" } else { "WINDOWED" },
        WHITE,
    );
    draw_menu_row(&mut c, selected, 6, "VICTORY_RAW_VIEWS", if cfg.raw_views { "ON" } else { "OFF" }, if cfg.raw_views { GREEN } else { MUTED });
    draw_menu_row(&mut c, selected, 7, "NATIVE KEYBOARD/MOUSE", if cfg.kbm_native { "ON" } else { "OFF" }, if cfg.kbm_native { GREEN } else { RED });

    // Bottom action buttons.
    let y_btn = row_y(8);
    let (bx1, bw1) = (18u32, 286u32);
    c.rect(bx1, y_btn, bw1, 32, if selected == 8 { BTN_RESTART_HL } else { BTN_RESTART });
    c.border(bx1, y_btn, bw1, 32, BORDER);
    c.text(bx1 + 53, y_btn + 9, 2, WHITE, "APPLY & RESTART");

    let (bx2, bw2) = (318u32, 184u32);
    c.rect(bx2, y_btn, bw2, 32, if selected == 9 { BTN_QUIT_HL } else { BTN_QUIT });
    c.border(bx2, y_btn, bw2, 32, BORDER);
    c.text(bx2 + 38, y_btn + 9, 2, WHITE, "QUIT GAME");

    c.text(16, MENU_H - 22, 1, MUTED, "F1:CLOSE  UP/DOWN:SELECT  LEFT/RIGHT/ENTER:CHANGE");
    c
}

fn paint_hud(fps: f32, frame_ms: f32) -> Canvas {
    let mut c = Canvas::new(HUD_W, HUD_H, BG);
    c.border(0, 0, HUD_W, HUD_H, BORDER);
    let col = if fps >= 25.0 {
        GREEN
    } else if fps >= 12.0 {
        ORANGE
    } else {
        RED
    };
    let text = format!("{fps:4.1} FPS {frame_ms:4.0}MS");
    c.text(10, 8, 2, col, &text);
    c
}

/// Swapchain overlay state owned by `Presenter`.
pub(crate) struct TequestaOverlay {
    selected: usize,
    pending_restart: bool,
    f1_was_down: bool,
    up_was: bool,
    down_was: bool,
    left_was: bool,
    right_was: bool,
    enter_was: bool,
    esc_was: bool,
    lmb_was: bool,
    dirty_menu: bool,
    last_hud: Instant,
    menu_surf: Option<Surface>,
    hud_surf: Option<Surface>,
}

impl TequestaOverlay {
    pub(crate) fn new() -> Self {
        ensure_init();
        Self {
            selected: 0,
            pending_restart: false,
            f1_was_down: false,
            up_was: false,
            down_was: false,
            left_was: false,
            right_was: false,
            enter_was: false,
            esc_was: false,
            lmb_was: false,
            dirty_menu: true,
            last_hud: Instant::now() - Duration::from_secs(1),
            menu_surf: None,
            hud_surf: None,
        }
    }

    fn apply_row(&mut self, row: usize, dir: i32) {
        let mut restart_now = false;
        let mut quit_now = false;
        {
            let mut cfg = config_lock().lock().unwrap_or_else(|e| e.into_inner());
            let mut hot_changed = false;
            match row {
                0 => {
                    cfg.port_static = !cfg.port_static;
                    crate::port_static::set_enabled(cfg.port_static);
                    hot_changed = true;
                }
                1 => {
                    let order = [0u32, 30, 60];
                    let pos = order.iter().position(|&v| v == cfg.fps_cap).unwrap_or(0);
                    let next = if dir < 0 { (pos + 2) % 3 } else { (pos + 1) % 3 };
                    cfg.fps_cap = order[next];
                    if let Some(a) = FPS_CAP.get() {
                        a.store(cfg.fps_cap, Relaxed);
                    }
                    hot_changed = true;
                }
                2 => {
                    let delta = if dir < 0 { -0.25 } else { 0.25 };
                    cfg.mouse_sensitivity = (cfg.mouse_sensitivity + delta).clamp(0.25, 3.0);
                    if let Some(a) = MOUSE_SENS_X100.get() {
                        a.store((cfg.mouse_sensitivity * 100.0).round() as u32, Relaxed);
                    }
                    hot_changed = true;
                }
                3 => {
                    cfg.show_fps_hud = !cfg.show_fps_hud;
                    if let Some(a) = SHOW_HUD.get() {
                        a.store(cfg.show_fps_hud, Relaxed);
                    }
                    hot_changed = true;
                }
                4 => {
                    let order = [50u32, 75, 100];
                    let pos = order.iter().position(|&v| v == cfg.resolution_scale).unwrap_or(2);
                    let next = if dir < 0 { (pos + 2) % 3 } else { (pos + 1) % 3 };
                    cfg.resolution_scale = order[next];
                    self.pending_restart = true;
                }
                5 => {
                    cfg.borderless_fullscreen = !cfg.borderless_fullscreen;
                    self.pending_restart = true;
                }
                6 => {
                    cfg.raw_views = !cfg.raw_views;
                    self.pending_restart = true;
                }
                7 => {
                    cfg.kbm_native = !cfg.kbm_native;
                    if let Some(a) = KBM_NATIVE.get() {
                        a.store(cfg.kbm_native, Relaxed);
                    }
                    self.pending_restart = true;
                }
                8 => {
                    let _ = cfg.save();
                    restart_now = true;
                }
                9 => {
                    quit_now = true;
                }
                _ => {}
            }
            if hot_changed {
                let _ = cfg.save();
            }
        }
        self.dirty_menu = true;
        if let Some((on_restart, on_quit)) = ACTIONS.get() {
            if restart_now {
                on_restart();
            } else if quit_now {
                on_quit();
            }
        }
    }

    fn poll_input(&mut self, hwnd: HWND, width: u32, height: u32) {
        let ours = foreground_ours();
        let edge = |vk: u16, prev: &mut bool| -> bool {
            let down = ours && key_down(vk as u16);
            let fired = down && !*prev;
            *prev = down;
            fired
        };

        if edge(VK_F1, &mut self.f1_was_down) {
            toggle_menu();
            self.dirty_menu = true;
            notify_cursor(hwnd);
        }

        if !menu_open() {
            self.up_was = false;
            self.down_was = false;
            self.left_was = false;
            self.right_was = false;
            self.enter_was = false;
            self.esc_was = false;
            self.lmb_was = false;
            return;
        }

        if edge(VK_ESCAPE, &mut self.esc_was) {
            MENU_OPEN.store(false, Relaxed);
            notify_cursor(hwnd);
            return;
        }
        if edge(VK_UP, &mut self.up_was) {
            self.selected = (self.selected + ROW_COUNT - 1) % ROW_COUNT;
            self.dirty_menu = true;
        }
        if edge(VK_DOWN, &mut self.down_was) {
            self.selected = (self.selected + 1) % ROW_COUNT;
            self.dirty_menu = true;
        }
        if edge(VK_LEFT, &mut self.left_was) {
            self.apply_row(self.selected, -1);
        }
        if edge(VK_RIGHT, &mut self.right_was) {
            self.apply_row(self.selected, 1);
        }
        if edge(VK_RETURN, &mut self.enter_was) || (ours && key_down(VK_SPACE)) {
            self.apply_row(self.selected, 1);
        }

        // Mouse hover & click inside centered menu box.
        let lmb = ours && key_down(VK_LBUTTON);
        let clicked = lmb && !self.lmb_was;
        self.lmb_was = lmb;
        if ours && !hwnd.0.is_null() {
            let mut pt = POINT::default();
            // SAFETY: valid POINT and HWND belonging to our process.
            if unsafe { GetCursorPos(&mut pt) } != 0 {
                let mut wpt = WIN_POINT { x: pt.x, y: pt.y };
                // SAFETY: valid POINT and HWND belonging to our process.
                if unsafe { ScreenToClient(hwnd, &mut wpt) }.as_bool() {
                    let mx = width.saturating_sub(MENU_W) / 2;
                    let my = height.saturating_sub(MENU_H) / 2;
                    let lx = wpt.x - mx as i32;
                    let ly = wpt.y - my as i32;
                    if (0..MENU_W as i32).contains(&lx) && (0..MENU_H as i32).contains(&ly) {
                        let mut hit: Option<usize> = None;
                        for r in 0..8usize {
                            let y0 = row_y(r) as i32 - 3;
                            if (y0..y0 + 24).contains(&ly) {
                                hit = Some(r);
                                break;
                            }
                        }
                        let yb = row_y(8) as i32;
                        if (yb..yb + 32).contains(&ly) {
                            if (18..18 + 286).contains(&lx) {
                                hit = Some(8);
                            } else if (318..318 + 184).contains(&lx) {
                                hit = Some(9);
                            }
                        }
                        if let Some(r) = hit {
                            if self.selected != r {
                                self.selected = r;
                                self.dirty_menu = true;
                            }
                            if clicked {
                                let dir = if lx < (MENU_W as i32) / 2 && r == 2 { -1 } else { 1 };
                                self.apply_row(r, dir);
                            }
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn update_and_draw(
        &mut self,
        host: &Host,
        list: &HostList,
        back: &ID3D12Resource,
        (width, height): (u32, u32),
        format: DXGI_FORMAT,
        hwnd: HWND,
        fps: f32,
        frame_ms: f32,
    ) {
        if bpp(format).is_none() || width < 200 || height < 100 {
            return;
        }
        self.poll_input(hwnd, width, height);

        if show_fps_hud() {
            if self.hud_surf.as_ref().is_none_or(|s| s.format != format) {
                self.hud_surf = Surface::new(host, HUD_W, HUD_H, format);
                self.last_hud = Instant::now() - Duration::from_secs(1);
            }
            if let Some(surf) = &mut self.hud_surf {
                if self.last_hud.elapsed() >= Duration::from_millis(200) {
                    self.last_hud = Instant::now();
                    let canvas = paint_hud(fps, frame_ms);
                    surf.upload(&canvas);
                }
                let hx = width.saturating_sub(HUD_W + 14);
                surf.blit(list, back, hx, 14, width, height);
            }
        }

        if menu_open() {
            if self.menu_surf.as_ref().is_none_or(|s| s.format != format) {
                self.menu_surf = Surface::new(host, MENU_W, MENU_H, format);
                self.dirty_menu = true;
            }
            if let Some(surf) = &mut self.menu_surf {
                if std::mem::take(&mut self.dirty_menu) {
                    let cfg = config_lock().lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let canvas = paint_menu(&cfg, self.selected, self.pending_restart);
                    surf.upload(&canvas);
                }
                let mx = width.saturating_sub(MENU_W) / 2;
                let my = height.saturating_sub(MENU_H) / 2;
                surf.blit(list, back, mx, my, width, height);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_and_hud_paint_cleanly() {
        let mut cfg = TequestaConfig::default();
        let menu = paint_menu(&cfg, 0, false);
        assert_eq!(menu.rgb.len(), (MENU_W * MENU_H) as usize);
        let pitch = (MENU_W as usize * 4).next_multiple_of(256);
        assert_eq!(menu.pack(DXGI_FORMAT_R8G8B8A8_UNORM, pitch).len(), pitch * MENU_H as usize);

        cfg.borderless_fullscreen = true;
        let menu_restart = paint_menu(&cfg, 8, true);
        assert_eq!(menu_restart.rgb.len(), (MENU_W * MENU_H) as usize);

        let hud = paint_hud(24.5, 40.8);
        assert_eq!(hud.rgb.len(), (HUD_W * HUD_H) as usize);
    }
}
