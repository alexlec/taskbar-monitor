//! Detail panel shown when the bar is clicked. Two columns: CPU, RAM and system on the left; GPUs on the right.
//! Closes on focus loss or Esc, unless pinned (then Esc, the close button or a click on the bar close it).
//! Can be dragged to one of five slots: above the bar (default) or one of the work area's corners.

use std::cell::Cell;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DWMWCP_ROUND;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gdi::{blend, px, rgb, round_corners, Canvas, Font};
use crate::metrics::{Monitor, Ring, ENGINE_LABELS};
use crate::overlay::{read_dword, write_setting, SETTINGS_KEY};
use crate::{with_monitor, OVERLAY, POPUP};

const CLASS: PCWSTR = w!("TaskbarMonitor.Popup");
const GUIDE_CLASS: PCWSTR = w!("TaskbarMonitor.Guide");
/// From Win32_UI_Controls, which would be a whole feature for one constant.
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Settings: keep the panel open (0/1) and where it sits (see `slot_pos`).
const PINNED_VALUE: PCWSTR = w!("PopupPinned");
const SLOT_VALUE: PCWSTR = w!("PopupSlot");

const BG: COLORREF = rgb(0x20, 0x20, 0x20);
const PANEL: COLORREF = rgb(0x2a, 0x2a, 0x2a);
const TRACK: COLORREF = rgb(0x3a, 0x3a, 0x3a);
const DIV: COLORREF = rgb(0x34, 0x34, 0x34);
const TEXT: COLORREF = rgb(0xf2, 0xf2, 0xf2);
const DIM: COLORREF = rgb(0x9a, 0x9a, 0x9a);
const C_CPU: COLORREF = rgb(0x4f, 0xa3, 0xff);
const C_RAM: COLORREF = rgb(0xb0, 0x7c, 0xff);
const C_GPU: COLORREF = rgb(0x3d, 0xd6, 0x8c);
const C_VRAM: COLORREF = rgb(0xf5, 0xa5, 0x24);

/// Transparent colour of the slot guide window.
const KEY: COLORREF = rgb(0xff, 0x00, 0xff);
const GUIDE_DIM: COLORREF = rgb(0xb0, 0xb0, 0xb0);
const GUIDE_NEAREST: COLORREF = rgb(0x6f, 0xb6, 0xff);

const COL_W: f32 = 300.0;
const GUTTER: f32 = 32.0;
const PAD: f32 = 16.0;
/// Content starts below a top row that holds the pin and close buttons.
const TOP: f32 = 30.0;
const BTN_W: f32 = 28.0;
const BTN_H: f32 = 24.0;
const BTN_Y: f32 = 4.0;
const BTN_MARGIN: f32 = 8.0;

/// Slot 0 is above the bar; 1..=4 are the work area's corners: top-left, top-right, bottom-left, bottom-right.
const SLOTS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    Pin,
    Close,
}

/// A press on the panel, from button-down to button-up. It becomes a drag once the mouse has moved
/// past the system drag threshold, so plain clicks on the buttons still work.
#[derive(Clone, Copy)]
struct Gesture {
    /// What was under the mouse at button-down.
    hit: Hit,
    start: POINT,
    /// Top-left of the panel at button-down.
    origin: POINT,
    active: bool,
    /// Work area of the bar's monitor: the panel is kept inside it while dragged.
    work: RECT,
    /// Click-through window that outlines the slots while dragging.
    guide: HWND,
    /// Where the panel would land in each slot.
    rects: [RECT; SLOTS],
    /// Slot nearest to the panel right now.
    nearest: u32,
}

thread_local! {
    static HIDDEN_AT: Cell<u64> = const { Cell::new(0) };
    static PINNED: Cell<bool> = Cell::new(read_dword(SETTINGS_KEY, PINNED_VALUE) == Some(1));
    static SLOT: Cell<u32> = Cell::new(match read_dword(SETTINGS_KEY, SLOT_VALUE) {
        Some(v) if (v as usize) < SLOTS => v,
        _ => 0,
    });
    /// A pinned panel that was open is hidden while the bar is, and comes back with it.
    static SUSPENDED: Cell<bool> = const { Cell::new(false) };
    static HOVER: Cell<Hit> = const { Cell::new(Hit::None) };
    static GESTURE: Cell<Option<Gesture>> = const { Cell::new(None) };
}

pub unsafe fn create(hinst: HINSTANCE) -> Result<HWND> {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: CLASS,
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let guide = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(guide_proc),
        hInstance: hinst,
        lpszClassName: GUIDE_CLASS,
        ..Default::default()
    };
    RegisterClassExW(&guide);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        CLASS,
        w!("Taskbar Monitor"),
        WS_POPUP,
        0,
        0,
        10,
        10,
        None,
        None,
        Some(hinst),
        None,
    )?;
    round_corners(hwnd, DWMWCP_ROUND);
    Ok(hwnd)
}

pub fn toggle() {
    let p = POPUP.get();
    unsafe {
        if IsWindowVisible(p).as_bool() {
            hide(p);
            return;
        }
        // If this click just closed the popup by stealing focus, don't reopen it
        if GetTickCount64().saturating_sub(HIDDEN_AT.get()) < 250 {
            return;
        }
        SUSPENDED.set(false);
        place(p);
        let _ = ShowWindow(p, SW_SHOW);
        let _ = SetForegroundWindow(p);
    }
}

/// Close if open (the menu hangs off this window, so it must not stay on top of it).
/// Returns whether it was open, so a pinned panel can be brought back after the menu.
pub fn close() -> bool {
    let p = POPUP.get();
    unsafe {
        let open = IsWindowVisible(p).as_bool();
        if open {
            hide(p);
        }
        open
    }
}

/// Show again after `close()`, without taking focus from whatever the user returns to.
pub fn reopen() {
    let p = POPUP.get();
    unsafe {
        if !IsWindowVisible(p).as_bool() {
            SUSPENDED.set(false);
            place(p);
            let _ = ShowWindow(p, SW_SHOWNOACTIVATE);
        }
    }
}

pub fn pinned() -> bool {
    PINNED.get()
}

pub fn set_pinned(on: bool) {
    PINNED.set(on);
    write_setting(PINNED_VALUE, on as u32);
    unsafe {
        let _ = InvalidateRect(Some(POPUP.get()), None, false);
    }
}

/// The bar was hidden (fullscreen app, auto-hidden taskbar): a pinned panel goes with it, since it
/// would otherwise float alone over the fullscreen app.
pub fn on_bar_hidden() {
    let p = POPUP.get();
    unsafe {
        if PINNED.get() && IsWindowVisible(p).as_bool() {
            end_gesture(p);
            let _ = ShowWindow(p, SW_HIDE);
            SUSPENDED.set(true);
        }
    }
}

/// The bar is back: bring back the pinned panel that went away with it.
pub fn on_bar_shown() {
    let p = POPUP.get();
    unsafe {
        if SUSPENDED.take() && PINNED.get() {
            place(p);
            let _ = ShowWindow(p, SW_SHOWNOACTIVATE);
        }
    }
}

/// Called every second: refresh if open.
pub fn refresh() {
    let p = POPUP.get();
    unsafe {
        if IsWindowVisible(p).as_bool() {
            // While dragging, the panel is under the mouse, not in its slot
            if !dragging() {
                place(p);
            }
            let _ = InvalidateRect(Some(p), None, false);
        }
    }
}

unsafe fn hide(p: HWND) {
    end_gesture(p);
    let _ = ShowWindow(p, SW_HIDE);
    HIDDEN_AT.set(GetTickCount64());
}

fn dragging() -> bool {
    GESTURE.get().is_some_and(|g| g.active)
}

fn scale() -> f32 {
    unsafe { GetDpiForWindow(OVERLAY.get()).max(96) as f32 / 96.0 }
}

fn width(s: f32) -> i32 {
    px(2.0 * PAD + 2.0 * COL_W + GUTTER, s)
}

/// Panel size for the current content.
fn size(s: f32) -> (i32, i32) {
    let w = width(s);
    let c = Canvas::new(w, 1);
    (w, with_monitor(|m| draw(&c, s, m)).unwrap_or(px(200.0, s)))
}

/// What the slots are placed against: the bar and the monitor it is on.
#[derive(Clone, Copy)]
struct Area {
    bar: RECT,
    mon: RECT,
    work: RECT,
    gap: i32,
    w: i32,
    h: i32,
}

unsafe fn area(s: f32, w: i32, h: i32) -> Area {
    let mut bar = RECT::default();
    let _ = GetWindowRect(OVERLAY.get(), &mut bar);
    let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(MonitorFromRect(&bar, MONITOR_DEFAULTTONEAREST), &mut mi);
    Area { bar, mon: mi.rcMonitor, work: mi.rcWork, gap: px(8.0, s), w, h }
}

/// Top-left of the panel in a slot. The bottom slots anchor the bottom edge, since the height changes
/// with the number of GPUs. Slot 0 sits just above the bar, aligned with its outer edge.
fn slot_pos(a: &Area, slot: u32) -> (i32, i32) {
    let Area { bar, mon, work, gap, w, h } = *a;
    let left = work.left + gap;
    let right = (work.right - w - gap).max(left);
    let top = work.top + gap;
    let bottom = (work.bottom - h - gap).max(top);
    match slot {
        1 => (left, top),
        2 => (right, top),
        3 => (left, bottom),
        4 => (right, bottom),
        _ => {
            let x = if bar.left - mon.left < mon.right - bar.right { bar.left } else { bar.right - w };
            (x.min(right).max(left), (bar.top.min(work.bottom) - h - gap).max(top))
        }
    }
}

/// Put the panel in its slot, above everything else in the ordinary topmost band.
unsafe fn place(p: HWND) {
    let s = scale();
    let (w, h) = size(s);
    let (x, y) = slot_pos(&area(s, w, h), SLOT.get());
    let _ = SetWindowPos(p, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
}

/// Pin and close buttons in the top row, pin at the corner. Close only exists while pinned.
fn button(s: f32, w: i32, which: Hit) -> RECT {
    let bw = px(BTN_W, s);
    let right = w - px(BTN_MARGIN, s) - if which == Hit::Close { bw } else { 0 };
    let top = px(BTN_Y, s);
    RECT { left: right - bw, top, right, bottom: top + px(BTN_H, s) }
}

fn hit_test(s: f32, w: i32, x: i32, y: i32) -> Hit {
    let inside = |r: RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
    if inside(button(s, w, Hit::Pin)) {
        Hit::Pin
    } else if PINNED.get() && inside(button(s, w, Hit::Close)) {
        Hit::Close
    } else {
        Hit::None
    }
}

unsafe fn hit_at(hwnd: HWND, x: i32, y: i32) -> Hit {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    hit_test(scale(), rc.right, x, y)
}

/// Client coordinates of a mouse message.
fn mouse_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32)
}

/// Slot whose landing rectangle is closest to `r`, by distance between centres.
fn nearest_slot(rects: &[RECT; SLOTS], r: &RECT) -> u32 {
    let centre = |r: &RECT| ((r.left + r.right) as i64 / 2, (r.top + r.bottom) as i64 / 2);
    let (cx, cy) = centre(r);
    (0..SLOTS)
        .min_by_key(|&i| {
            let (x, y) = centre(&rects[i]);
            (x - cx).pow(2) + (y - cy).pow(2)
        })
        .unwrap_or(0) as u32
}

/// Button-down: remember what was hit and capture the mouse so the drag keeps working off the panel.
unsafe fn press(hwnd: HWND, x: i32, y: i32) {
    let _ = SetCapture(hwnd);
    let mut start = POINT::default();
    let _ = GetCursorPos(&mut start);
    let mut r = RECT::default();
    let _ = GetWindowRect(hwnd, &mut r);
    GESTURE.set(Some(Gesture {
        hit: hit_at(hwnd, x, y),
        start,
        origin: POINT { x: r.left, y: r.top },
        active: false,
        work: RECT::default(),
        guide: HWND::default(),
        rects: [RECT::default(); SLOTS],
        nearest: SLOT.get(),
    }));
}

unsafe fn mouse_move(hwnd: HWND, x: i32, y: i32) {
    let Some(mut g) = GESTURE.get() else {
        // Hover feedback on the buttons
        let hit = hit_at(hwnd, x, y);
        if hit != HOVER.get() {
            HOVER.set(hit);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
        let _ = TrackMouseEvent(&mut t);
        return;
    };
    if g.hit != Hit::None {
        return;
    }
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    if !g.active {
        let (dx, dy) = (GetSystemMetrics(SM_CXDRAG).max(1), GetSystemMetrics(SM_CYDRAG).max(1));
        if (pt.x - g.start.x).abs() < dx && (pt.y - g.start.y).abs() < dy {
            return;
        }
        begin_drag(hwnd, &mut g);
    }
    let mut r = RECT::default();
    let _ = GetWindowRect(hwnd, &mut r);
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let x = (g.origin.x + pt.x - g.start.x).min(g.work.right - w).max(g.work.left);
    let y = (g.origin.y + pt.y - g.start.y).min(g.work.bottom - h).max(g.work.top);
    let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    let now = nearest_slot(&g.rects, &RECT { left: x, top: y, right: x + w, bottom: y + h });
    if now != g.nearest {
        g.nearest = now;
        if !g.guide.is_invalid() {
            let _ = InvalidateRect(Some(g.guide), None, false);
        }
    }
    GESTURE.set(Some(g));
}

/// The mouse moved far enough: work out where the panel would land in each slot and outline them.
unsafe fn begin_drag(hwnd: HWND, g: &mut Gesture) {
    let mut r = RECT::default();
    let _ = GetWindowRect(hwnd, &mut r);
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let a = area(scale(), w, h);
    for (i, slot) in g.rects.iter_mut().enumerate() {
        let (x, y) = slot_pos(&a, i as u32);
        *slot = RECT { left: x, top: y, right: x + w, bottom: y + h };
    }
    g.work = a.work;
    g.nearest = nearest_slot(&g.rects, &r);
    g.active = true;
    // The guide paints from GESTURE, possibly as soon as it is shown, so publish it first
    GESTURE.set(Some(*g));

    let Ok(m) = GetModuleHandleW(None) else { return };
    let guide = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
        GUIDE_CLASS,
        w!(""),
        WS_POPUP,
        a.work.left,
        a.work.top,
        a.work.right - a.work.left,
        a.work.bottom - a.work.top,
        None,
        None,
        Some(m.into()),
        None,
    );
    if let Ok(guide) = guide {
        let _ = SetLayeredWindowAttributes(guide, KEY, 0, LWA_COLORKEY);
        g.guide = guide;
        GESTURE.set(Some(*g));
        let _ = ShowWindow(guide, SW_SHOWNOACTIVATE);
        // The guide is topmost too and was created last: put the panel back above it
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// Drop the current press or drag: destroy the guide and release the mouse. Returns whether a drag
/// was in progress, in which case the panel is not where it belongs.
unsafe fn end_gesture(p: HWND) -> bool {
    let Some(g) = GESTURE.take() else { return false };
    if !g.guide.is_invalid() {
        let _ = DestroyWindow(g.guide);
    }
    if GetCapture() == p {
        let _ = ReleaseCapture();
    }
    g.active
}

unsafe fn release(hwnd: HWND, x: i32, y: i32) {
    let Some(g) = GESTURE.get() else { return };
    end_gesture(hwnd);
    if g.active {
        SLOT.set(g.nearest);
        write_setting(SLOT_VALUE, g.nearest);
        place(hwnd);
    } else if g.hit != Hit::None && g.hit == hit_at(hwnd, x, y) {
        match g.hit {
            Hit::Pin => set_pinned(!PINNED.get()),
            _ => hide(hwnd),
        }
    }
}

/// Outlines of the slots over the whole work area. Colour-keyed so only the dots show, and
/// click-through so the drag is never disturbed.
unsafe extern "system" fn guide_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let mut win = RECT::default();
            let _ = GetWindowRect(hwnd, &mut win);
            let c = Canvas::new(rc.right, rc.bottom);
            c.fill(0, 0, c.w, c.h, KEY);
            if let Some(g) = GESTURE.get().filter(|g| g.active) {
                let s = scale();
                let local = |r: &RECT| RECT {
                    left: r.left - win.left,
                    top: r.top - win.top,
                    right: r.right - win.left,
                    bottom: r.bottom - win.top,
                };
                for r in g.rects.iter().enumerate().filter(|&(i, _)| i as u32 != g.nearest).map(|(_, r)| r) {
                    c.dotted_rect(&local(r), px(2.0, s), GUIDE_DIM);
                }
                c.dotted_rect(&local(&g.rects[g.nearest as usize]), px(4.0, s), GUIDE_NEAREST);
            }
            c.blit(hdc);
            let _ = EndPaint(hwnd, &ps);
        }
        WM_ERASEBKGND => return LRESULT(1),
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    LRESULT(0)
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let c = Canvas::new(rc.right, rc.bottom);
    with_monitor(|m| draw(&c, scale(), m));
    c.blit(hdc);
    let _ = EndPaint(hwnd, &ps);
}

/// Draw all content and return the height needed. Call with a 1px-tall canvas to measure.
fn draw(c: &Canvas, s: f32, m: &Monitor) -> i32 {
    let snap = &m.snap;
    c.fill(0, 0, c.w, c.h, BG);
    let pad = px(PAD, s);
    let col_w = px(COL_W, s);
    let top = px(TOP, s);
    let mut d = Doc::new(c, s, pad, top, col_w);

    // Left column
    d.header("CPU", &pct(snap.cpu), C_CPU);
    d.line(&m.cpu_name, "");
    let ghz = if snap.cpu_mhz > 0.0 { format!("{:.2} GHz", snap.cpu_mhz / 1000.0) } else { String::new() };
    let n = snap.cores.len();
    d.line(&tr!(format!("{n} threads"), format!("{n} luồng")), &ghz);
    d.spark(&m.cpu_hist, C_CPU, None);
    d.cores(&snap.cores);
    d.divider();
    d.header("RAM", &pct(snap.ram_pct()), C_RAM);
    d.line(tr!("In use", "Đang dùng"), &format!("{} / {} GB", gb(snap.ram_used), gb(snap.ram_total)));
    d.line(tr!("Available", "Còn trống"), &format!("{} GB", gb(snap.ram_total.saturating_sub(snap.ram_used))));
    d.line(tr!("Committed", "Bộ nhớ cam kết"), &format!("{} / {} GB", gb(snap.commit_used), gb(snap.commit_limit)));
    d.spark(&m.ram_hist, C_RAM, None);
    d.divider();
    d.header(tr!("System", "Hệ thống"), "", TEXT);
    d.line(tr!("Processes", "Tiến trình"), &thousands(snap.processes as u64));
    d.line(tr!("Threads", "Luồng"), &thousands(snap.threads as u64));
    d.line(tr!("Handles", "Handle"), &thousands(snap.handles as u64));
    d.line(tr!("Up time", "Thời gian chạy"), &uptime(snap.uptime_s));
    let left_h = d.y;

    // Right column
    d.x = pad + col_w + px(GUTTER, s);
    d.y = top;
    if snap.gpus.is_empty() {
        d.header("GPU", "–", DIM);
        d.line(tr!("GPU information unavailable", "Không đọc được thông tin GPU"), "");
    }
    for (i, g) in snap.gpus.iter().enumerate() {
        if i > 0 {
            d.divider();
        }
        let title = if snap.gpus.len() > 1 { format!("GPU {i}") } else { "GPU".into() };
        d.header(&title, &pct(g.util), C_GPU);
        d.line(&g.name, "");
        for &(k, v) in &g.engines {
            d.meter(ENGINE_LABELS[k], v, C_GPU);
        }
        d.line("VRAM", &format!("{} / {} GB", gb(g.vram_used), gb(g.vram_total)));
        d.thin_bar(g.vram_pct(), C_VRAM);
        d.line(tr!("Shared memory", "Bộ nhớ dùng chung"), &format!("{} GB", gb(g.shared_used)));
        d.spark(&m.gpu_hist[i], C_GPU, Some((&m.vram_hist[i], C_VRAM)));
    }
    let right_h = d.y;

    let h = left_h.max(right_h) + pad;
    // Column divider
    c.fill(pad + col_w + px(GUTTER / 2.0, s), top, px(1.0, s).max(1), h - top - pad, DIV);
    buttons(c, s);
    h
}

/// Pin (filled and blue while pinned) and, only while pinned, close, in the top row.
fn buttons(c: &Canvas, s: f32) {
    let (pinned, hover) = (PINNED.get(), HOVER.get());
    let icons = Font::face(px(12.0, s), 400, "Segoe Fluent Icons");
    c.font(&icons);
    let btn = |which: Hit, glyph: &str, color: COLORREF| {
        let r = button(s, c.w, which);
        let on = hover == which;
        if on {
            c.fill(r.left, r.top, r.right - r.left, r.bottom - r.top, TRACK);
        }
        let (tw, th) = c.measure(glyph);
        c.text((r.left + r.right - tw) / 2, (r.top + r.bottom - th) / 2, glyph, if on { TEXT } else { color });
    };
    btn(Hit::Pin, if pinned { "\u{E840}" } else { "\u{E718}" }, if pinned { C_CPU } else { DIM });
    if pinned {
        btn(Hit::Close, "\u{E711}", DIM);
    }
    // Deselect before `icons` is deleted
    c.release_font();
}

/// Line-by-line drawing cursor within a column.
struct Doc<'a> {
    c: &'a Canvas,
    s: f32,
    x: i32,
    y: i32,
    w: i32,
    title: Font,
    big: Font,
    body: Font,
}

impl<'a> Doc<'a> {
    fn new(c: &'a Canvas, s: f32, pad: i32, top: i32, w: i32) -> Self {
        Self {
            c,
            s,
            x: pad,
            y: top,
            w,
            title: Font::new(px(14.0, s), 700),
            big: Font::new(px(20.0, s), 700),
            body: Font::new(px(12.5, s), 400),
        }
    }

    fn p(&self, v: f32) -> i32 {
        px(v, self.s)
    }

    fn header(&mut self, label: &str, value: &str, color: COLORREF) {
        self.c.font(&self.title);
        self.c.text(self.x, self.y + self.p(5.0), label, TEXT);
        self.c.font(&self.big);
        self.c.text_right(self.x + self.w, self.y, value, color);
        self.y += self.p(30.0);
    }

    /// Dim label on the left, bright value on the right.
    fn line(&mut self, left: &str, right: &str) {
        self.c.font(&self.body);
        let rw = if right.is_empty() { 0 } else { self.c.measure(right).0 + self.p(8.0) };
        self.c.text_fit(self.x, self.y, self.w - rw, self.p(18.0), left, DIM);
        if !right.is_empty() {
            self.c.text_right(self.x + self.w, self.y, right, TEXT);
        }
        self.y += self.p(19.0);
    }

    fn meter(&mut self, label: &str, v: f32, color: COLORREF) {
        self.c.font(&self.body);
        self.c.text(self.x, self.y, label, DIM);
        let (bx, bw, bh) = (self.x + self.p(96.0), self.w - self.p(96.0 + 44.0), self.p(6.0));
        self.c.bar(bx, self.y + self.p(7.0), bw, bh, v, color, TRACK);
        self.c.text_right(self.x + self.w, self.y, &pct(v), TEXT);
        self.y += self.p(20.0);
    }

    fn thin_bar(&mut self, v: f32, color: COLORREF) {
        self.c.bar(self.x, self.y + self.p(2.0), self.w, self.p(6.0), v, color, TRACK);
        self.y += self.p(14.0);
    }

    fn spark(&mut self, data: &Ring, color: COLORREF, second: Option<(&Ring, COLORREF)>) {
        let (y, h) = (self.y + self.p(6.0), self.p(44.0));
        self.c.fill(self.x, y, self.w, h, PANEL);
        let thick = self.p(1.5);
        self.c.spark(self.x, y, self.w, h, data, color, Some(blend(color, PANEL, 0.22)), thick);
        if let Some((d2, c2)) = second {
            self.c.spark(self.x, y, self.w, h, d2, c2, None, thick);
        }
        self.y = y + h + self.p(8.0);
    }

    /// One vertical bar per CPU thread, wrapping to more rows when there are many.
    fn cores(&mut self, cores: &[f32]) {
        let n = cores.len();
        if n == 0 {
            return;
        }
        let gap = self.p(2.0).max(1);
        let per_row = (((self.w + gap) / (self.p(6.0) + gap)).max(1) as usize).min(n);
        let rows = n.div_ceil(per_row);
        let bw = (self.w - gap * (per_row as i32 - 1)) / per_row as i32;
        let bh = if rows == 1 { self.p(28.0) } else { self.p(18.0) };
        for (i, &v) in cores.iter().enumerate() {
            let bx = self.x + (i % per_row) as i32 * (bw + gap);
            let by = self.y + (i / per_row) as i32 * (bh + gap);
            self.c.fill(bx, by, bw, bh, TRACK);
            let fh = (bh as f32 * v / 100.0).round() as i32;
            self.c.fill(bx, by + bh - fh, bw, fh, C_CPU);
        }
        self.y += rows as i32 * (bh + gap) + self.p(2.0);
    }

    fn divider(&mut self) {
        self.y += self.p(10.0);
        self.c.fill(self.x, self.y, self.w, self.p(1.0).max(1), DIV);
        self.y += self.p(12.0);
    }
}

impl Drop for Doc<'_> {
    // Deselect Doc's fonts before they are deleted (DeleteObject fails on a selected font)
    fn drop(&mut self) {
        self.c.release_font();
    }
}

fn pct(v: f32) -> String {
    format!("{v:.0}%")
}

fn gb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 30) as f64)
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let sep = tr!(',', '.');
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(sep);
        }
        out.push(ch);
    }
    out
}

fn uptime(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        let days = tr!(if d == 1 { "day" } else { "days" }, "ngày");
        format!("{d} {days} {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => paint(hwnd),
        WM_ERASEBKGND => return LRESULT(1),
        WM_ACTIVATE if (wp.0 & 0xffff) as u32 == WA_INACTIVE => {
            if !PINNED.get() {
                hide(hwnd);
            }
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => {
            // Esc cancels a drag in progress first, then closes
            if GESTURE.get().is_some() {
                if end_gesture(hwnd) {
                    place(hwnd);
                }
            } else {
                hide(hwnd);
            }
        }
        WM_LBUTTONDOWN => {
            let (x, y) = mouse_xy(lp);
            press(hwnd, x, y);
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_xy(lp);
            mouse_move(hwnd, x, y);
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_xy(lp);
            release(hwnd, x, y);
            HOVER.set(hit_at(hwnd, x, y));
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        // Capture taken away (another window, a dialog): give up the drag and snap back
        WM_CAPTURECHANGED => {
            if end_gesture(hwnd) {
                place(hwnd);
            }
        }
        WM_MOUSELEAVE => {
            HOVER.set(Hit::None);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(hwnd, &mut pt);
            let cursor = if dragging() {
                IDC_SIZEALL
            } else if hit_at(hwnd, pt.x, pt.y) != Hit::None {
                IDC_HAND
            } else {
                return DefWindowProcW(hwnd, msg, wp, lp);
            };
            SetCursor(LoadCursorW(None, cursor).ok());
            return LRESULT(1);
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    LRESULT(0)
}
