//! Calendar widget: the next hour of your Google Calendar, drawn inside the Windows taskbar.
//! A red dot marks "now" at the left edge, a red line runs one hour to the right, and the
//! events in that hour sit above it as colored blocks (Google Calendar's colors).
//!
//! Hover: today's upcoming events with times. Click an event: join its Meet call, or open it.
//! Drag left/right: move (remembered). Right-click: refresh, sign in/out, reset position, exit.
//!
//! The widget is a layered child window of the taskbar
//! (Shell_TrayWnd), recreated after Explorer restarts. Network calls run on worker threads
//! that hand results back through a mutex and a posted message.

#![windows_subsystem = "windows"]
#![allow(unsafe_op_in_unsafe_fn, static_mut_refs)]

mod google;

use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Mutex;

use google::{Event, FetchError, now, wide};
use serde_json::Value;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus as gp;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_FETCHED: u32 = WM_APP + 1;
const WM_APP_SIGNED_IN: u32 = WM_APP + 2;
const TIMER_HOUSEKEEPING: usize = 1; // every 15 s: redraw (time moves), Explorer restart, taskbar size
const TIMER_FETCH: usize = 2; // every 2 min: pull events from Google
const TIMER_RECREATE: usize = 3; // retries creating the widget while the taskbar is missing
const TIMER_FETCH_SOON: usize = 4; // one-shot, after resume from sleep
const CMD_REFRESH: i32 = 1;
const CMD_OPEN_CALENDAR: i32 = 2;
const CMD_SIGN_IN: i32 = 3;
const CMD_SIGN_OUT: i32 = 4;
const CMD_OPEN_FOLDER: i32 = 5;
const CMD_RESET_POSITION: i32 = 6;
const CMD_EXIT: i32 = 7;

const WINDOW_SECS: i64 = 3600; // the bar shows one hour ahead...
const PAST_SECS: i64 = 8 * 60; // ...plus a few faded minutes behind "now"
const BASE_W: f32 = 132.0; // widget size at 100% scaling
const BASE_H: f32 = 40.0;
const CALENDAR_URL: &str = "https://calendar.google.com/calendar/r/day";

struct Theme {
    light: bool, // Windows taskbar/system theme is light
    fg: u32,
    dim: u32,
    grid: u32,
}

#[derive(PartialEq)]
enum Status {
    Loading,
    NoClient,
    SignedOut,
    Ok,
    Error, // never loaded successfully
}

struct App {
    hinst: HINSTANCE,
    owner: HWND,  // hidden top-level window: timers, broadcasts, menu owner
    widget: HWND, // child of the taskbar
    tooltip: HWND,
    tip: String,
    taskbar: HWND,
    dir: PathBuf,
    pos_file: PathBuf,
    events: Vec<Event>,
    status: Status,
    updated: Option<i64>,
    error: Option<String>,
    fetching: bool,
    signing_in: bool,
    offset: Option<i32>, // px from the taskbar's right edge to the widget's right edge
    scale: f32,
    bounds: (i32, i32, i32, i32),
    drag: Option<(i32, i32, bool)>, // (cursor x at start, offset at start, moved)
    hits: Vec<(f32, f32, f32, f32, String)>, // event blocks from the last paint, for clicks
    theme: Theme,
    quitting: bool,
}

static mut APP: Option<App> = None;
static FETCHED: Mutex<Option<Result<Vec<Event>, FetchError>>> = Mutex::new(None);
static SIGNED_IN: Mutex<Option<Result<(), String>>> = Mutex::new(None);

fn app() -> &'static mut App {
    // Single UI thread; worker threads only touch the mutexes above and post messages.
    unsafe { APP.as_mut().unwrap() }
}

fn argb(a: u8, (r, g, b): (u8, u8, u8)) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn load_theme() -> Theme {
    let mut light: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize").as_ptr(),
            wide("SystemUsesLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut light as *mut u32 as *mut _,
            &mut size,
        );
    }
    if light == 1 {
        Theme { light: true, fg: argb(255, (25, 25, 25)), dim: argb(255, (95, 95, 95)), grid: argb(255, (200, 200, 205)) }
    } else {
        Theme { light: false, fg: argb(255, (235, 235, 235)), dim: argb(255, (160, 160, 160)), grid: argb(255, (70, 72, 80)) }
    }
}

/// Matches the hover tooltip to the system theme: Windows' built-in dark tooltip style in dark
/// mode, the normal one in light mode.
unsafe fn style_tooltip(tooltip: HWND, light: bool) {
    if !tooltip.is_null() {
        let style = wide(if light { "Explorer" } else { "DarkMode_Explorer" });
        SetWindowTheme(tooltip, style.as_ptr(), null());
    }
}

// ---- time formatting ----------------------------------------------------------------------

fn local_time(unix: i64) -> SYSTEMTIME {
    unsafe {
        let ticks = (unix as u64 + 11_644_473_600) * 10_000_000;
        let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        let (mut utc, mut local): (SYSTEMTIME, SYSTEMTIME) = (zeroed(), zeroed());
        FileTimeToSystemTime(&ft, &mut utc);
        SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local);
        local
    }
}

fn hm(unix: i64) -> String {
    let t = local_time(unix);
    format!("{:02}:{:02}", t.wHour, t.wMinute)
}

fn same_day(a: i64, b: i64) -> bool {
    let (x, y) = (local_time(a), local_time(b));
    (x.wYear, x.wMonth, x.wDay) == (y.wYear, y.wMonth, y.wDay)
}

fn duration(secs: i64) -> String {
    let m = (secs + 59) / 60;
    if m >= 60 { format!("{}h {:02}m", m / 60, m % 60) } else { format!("{m}m") }
}

// ---- data ---------------------------------------------------------------------------------

unsafe fn start_fetch() {
    let a = app();
    if a.fetching {
        return;
    }
    a.fetching = true;
    let (dir, owner) = (a.dir.clone(), a.owner as isize);
    std::thread::spawn(move || {
        let r = if std::env::var_os("CALENDAR_WIDGET_DEMO").is_some() { Ok(demo_events()) } else { google::fetch(&dir) };
        *FETCHED.lock().unwrap() = Some(r);
        PostMessageW(owner as HWND, WM_APP_FETCHED, 0, 0);
    });
}

/// Fake events for trying out the look (CALENDAR_WIDGET_DEMO=1).
fn demo_events() -> Vec<Event> {
    let t = now() / 60 * 60;
    let ev = |title: &str, s: i64, e: i64, color: (u8, u8, u8), tentative: bool| Event {
        title: title.into(),
        start: t + s * 60,
        end: t + e * 60,
        color,
        tentative,
        link: String::new(),
        meet: None,
    };
    vec![
        ev("Daily standup", -10, 8, (3, 155, 229), false),
        ev("Design review", 20, 50, (51, 182, 121), false),
        ev("Vendor call", 35, 65, (121, 134, 203), true),
        ev("1:1", 90, 120, (3, 155, 229), false),
    ]
}

unsafe fn on_fetched() {
    let a = app();
    a.fetching = false;
    let Some(r) = FETCHED.lock().unwrap().take() else { return };
    match r {
        Ok(events) => {
            a.events = events;
            a.status = Status::Ok;
            a.updated = Some(now());
            a.error = None;
        }
        Err(FetchError::NoClient) => {
            a.events.clear();
            a.status = Status::NoClient;
        }
        Err(FetchError::SignedOut) => {
            a.events.clear();
            a.status = Status::SignedOut;
        }
        Err(FetchError::Failed(msg)) => {
            // keep showing the last good events
            a.error = Some(msg);
            if a.status != Status::Ok {
                a.status = Status::Error;
            }
        }
    }
    redraw();
}

unsafe fn start_sign_in() {
    let a = app();
    if a.signing_in {
        return;
    }
    a.signing_in = true;
    a.error = None;
    let (dir, owner) = (a.dir.clone(), a.owner as isize);
    std::thread::spawn(move || {
        let r = google::sign_in(&dir);
        *SIGNED_IN.lock().unwrap() = Some(r);
        PostMessageW(owner as HWND, WM_APP_SIGNED_IN, 0, 0);
    });
    redraw();
}

unsafe fn on_signed_in() {
    let a = app();
    a.signing_in = false;
    if let Some(Err(e)) = SIGNED_IN.lock().unwrap().take() {
        a.error = Some(e);
    }
    start_fetch();
    redraw();
}

/// Events overlapping the hour shown on the bar.
fn visible(events: &[Event], t: i64) -> impl Iterator<Item = &Event> {
    events.iter().filter(move |e| e.end > t - PAST_SECS && e.start < t + WINDOW_SECS)
}

fn tooltip_text() -> String {
    let a = app();
    let t = now();
    let mut lines: Vec<String> = Vec::new();
    match a.status {
        Status::NoClient => {
            lines.push("Google Calendar is not set up yet.".into());
            lines.push(format!("Put the OAuth client JSON (client_secret_….json) in {}", a.dir.display()));
            lines.push("Click to open that folder.".into());
        }
        Status::SignedOut if a.signing_in => lines.push("Finish signing in in your browser.".into()),
        Status::SignedOut => lines.push("Not signed in. Click to sign in with Google.".into()),
        Status::Loading => lines.push("Loading your calendar…".into()),
        Status::Error => lines.push("Could not load your calendar.".into()),
        Status::Ok => {
            let today: Vec<&Event> = a.events.iter().filter(|e| e.end > t && same_day(e.start.max(t), t)).collect();
            if today.is_empty() {
                lines.push("Nothing else on your calendar today.".into());
            }
            for e in today.iter().take(10) {
                let when = if e.start <= t {
                    format!("now, {} left", duration(e.end - t))
                } else {
                    format!("in {}", duration(e.start - t))
                };
                let maybe = if e.tentative { " · not accepted" } else { "" };
                lines.push(format!("{}–{}  {}  ({when}{maybe})", hm(e.start), hm(e.end), e.title));
            }
            lines.push(String::new());
            lines.push("Click an event to join its Meet call or open it.".into());
        }
    }
    let mut foot = Vec::new();
    if let Some(u) = a.updated {
        foot.push(format!("Updated {}", hm(u)));
    }
    if let Some(e) = &a.error {
        foot.push(format!("Last refresh failed: {e}"));
    }
    if !foot.is_empty() {
        lines.push(foot.join(" · "));
    }
    lines.join("\r\n")
}

unsafe fn redraw() {
    update_tooltip();
    let a = app();
    if !a.widget.is_null() {
        paint(a.widget);
    }
}

// ---- position -----------------------------------------------------------------------------

fn load_offset(path: &PathBuf) -> Option<i32> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(s.trim_start_matches('\u{feff}')).ok()?;
    v.get("offset")?.as_f64().map(|o| o as i32)
}

unsafe fn rect_of(h: HWND) -> RECT {
    let mut r: RECT = zeroed();
    GetWindowRect(h, &mut r);
    r
}

/// Default spot: just left of the tray icons (drag to move; the position is remembered).
unsafe fn default_offset(tb: &RECT, scale: f32) -> i32 {
    let mut off = (420.0 * scale) as i32;
    let tray = FindWindowExW(app().taskbar, null_mut(), wide("TrayNotifyWnd").as_ptr(), null());
    if !tray.is_null() {
        let n = rect_of(tray);
        if n.right > n.left && n.left > tb.left + (tb.right - tb.left) / 2 {
            off = tb.right - n.left + (12.0 * scale) as i32;
        }
    }
    off
}

fn current_offset() -> i32 {
    let a = app();
    unsafe { a.offset.unwrap_or_else(|| default_offset(&rect_of(a.taskbar), a.scale)) }
}

/// Positions the widget inside the taskbar (child coordinates are relative to the taskbar).
unsafe fn place(force: bool) {
    let a = app();
    if a.widget.is_null() {
        return;
    }
    let t = rect_of(a.taskbar);
    let h = t.bottom - t.top;
    if h <= 0 || h > 200 {
        return; // vertical or odd taskbar
    }
    a.scale = h as f32 / 48.0; // the Windows 11 taskbar is 48 px tall at 100% scaling
    let (w, fh) = ((BASE_W * a.scale) as i32, (BASE_H * a.scale) as i32);
    let off = a.offset.unwrap_or_else(|| default_offset(&t, a.scale));
    let b = ((t.right - t.left) - off - w, (h - fh) / 2, w, fh);
    if !force && b == a.bounds {
        return;
    }
    a.bounds = b;
    SetWindowPos(a.widget, HWND_TOP, b.0, b.1, b.2, b.3, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    paint(a.widget);
}

// ---- widget lifetime ----------------------------------------------------------------------

unsafe fn create_widget() -> bool {
    let a = app();
    let tb = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
    if tb.is_null() {
        return false;
    }
    a.taskbar = tb;
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        wide("CalendarTaskbarWidget").as_ptr(),
        null(),
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
        0, 0, 10, 10,
        tb,
        null_mut(),
        a.hinst,
        null(),
    );
    if hwnd.is_null() {
        return false;
    }
    a.widget = hwnd;
    a.bounds = (0, 0, 0, 0);

    a.tooltip = CreateWindowExW(
        WS_EX_TOPMOST,
        TOOLTIPS_CLASSW,
        null(),
        WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
        CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
        a.owner,
        null_mut(),
        a.hinst,
        null(),
    );
    let mut text = wide("");
    let ti = tool_info(&mut text);
    SendMessageW(a.tooltip, TTM_ADDTOOLW, 0, &ti as *const _ as LPARAM);
    SendMessageW(a.tooltip, TTM_SETMAXTIPWIDTH, 0, 800);
    style_tooltip(a.tooltip, a.theme.light);
    SendMessageW(a.tooltip, TTM_SETDELAYTIME, TTDT_AUTOPOP as usize, 30_000);
    a.tip.clear();

    place(true);
    redraw();
    true
}

fn tool_info(text: &mut Vec<u16>) -> TTTOOLINFOW {
    let a = app();
    TTTOOLINFOW {
        cbSize: size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_IDISHWND | TTF_SUBCLASS,
        hwnd: a.widget,
        uId: a.widget as usize,
        lpszText: text.as_mut_ptr(),
        ..unsafe { zeroed() }
    }
}

unsafe fn update_tooltip() {
    let a = app();
    if a.tooltip.is_null() {
        return;
    }
    let s = tooltip_text();
    if s == a.tip {
        return;
    }
    let mut text = wide(&s);
    a.tip = s;
    let ti = tool_info(&mut text);
    SendMessageW(a.tooltip, TTM_UPDATETIPTEXTW, 0, &ti as *const _ as LPARAM);
}

unsafe fn destroy_widget() {
    let a = app();
    if !a.tooltip.is_null() {
        DestroyWindow(a.tooltip);
        a.tooltip = null_mut();
    }
    if !a.widget.is_null() {
        let w = a.widget;
        a.widget = null_mut();
        if IsWindow(w) != 0 {
            DestroyWindow(w);
        }
    }
}

/// Recreates the widget if Explorer restarted or the widget got detached.
unsafe fn ensure_widget() {
    let a = app();
    let ok = !a.widget.is_null()
        && IsWindow(a.widget) != 0
        && IsWindow(a.taskbar) != 0
        && GetParent(a.widget) == a.taskbar;
    if !ok {
        destroy_widget();
        if !create_widget() {
            SetTimer(app().owner, TIMER_RECREATE, 2000, None);
        }
    }
}

// ---- painting -----------------------------------------------------------------------------

/// Rounded rectangle path; the caller deletes it.
unsafe fn round_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> *mut gp::GpPath {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.1);
    let d = r * 2.0;
    let mut path = null_mut();
    gp::GdipCreatePath(gp::FillModeAlternate, &mut path);
    gp::GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    gp::GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
    gp::GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
    gp::GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
    gp::GdipClosePathFigure(path);
    path
}

const PIXEL_FORMAT_32BPP_PARGB: i32 = 0x000E_200B; // GDI+ PixelFormat32bppPARGB

/// Draws the widget into a 32-bit ARGB bitmap and hands it to the layered window.
/// The background is alpha 1: invisible, but it still catches the mouse over the whole widget
/// (fully transparent pixels would let clicks fall through to the taskbar).
unsafe fn paint(hwnd: HWND) {
    let a = app();
    let mut rc: RECT = zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    if w <= 0 || h <= 0 {
        return;
    }
    let screen = GetDC(null_mut());
    let mem = CreateCompatibleDC(screen);
    let mut bmi: BITMAPINFO = zeroed();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;
    let mut bits = null_mut();
    let bmp = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    let old = SelectObject(mem, bmp as HGDIOBJ);
    let mut canvas = null_mut();
    gp::GdipCreateBitmapFromScan0(w, h, w * 4, PIXEL_FORMAT_32BPP_PARGB, bits as *const u8, &mut canvas);
    let mut g = null_mut();
    gp::GdipGetImageGraphicsContext(canvas as *mut gp::GpImage, &mut g);
    gp::GdipGraphicsClear(g, 0x0100_0000);
    gp::GdipSetSmoothingMode(g, gp::SmoothingModeAntiAlias);
    gp::GdipSetTextRenderingHint(g, gp::TextRenderingHintAntiAliasGridFit);
    let sc = a.scale;
    let mut family = null_mut();
    gp::GdipCreateFontFamilyFromName(wide("Segoe UI").as_ptr(), null_mut(), &mut family);
    let mut fmt = null_mut();
    gp::GdipCreateStringFormat(gp::StringFormatFlagsNoWrap, 0, &mut fmt);
    gp::GdipSetStringFormatTrimming(fmt, gp::StringTrimmingEllipsisCharacter);
    gp::GdipSetStringFormatLineAlign(fmt, gp::StringAlignmentCenter);

    let solid = |c: u32| {
        let mut b = null_mut();
        gp::GdipCreateSolidFill(c, &mut b);
        b as *mut gp::GpBrush
    };
    let font = |px: f32| {
        let mut f = null_mut();
        gp::GdipCreateFont(family, px * sc, gp::FontStyleRegular, gp::UnitPixel, &mut f);
        f
    };
    let mut fmt_wrap = null_mut();
    gp::GdipCreateStringFormat(0, 0, &mut fmt_wrap);
    gp::GdipSetStringFormatTrimming(fmt_wrap, gp::StringTrimmingEllipsisWord);
    gp::GdipSetStringFormatLineAlign(fmt_wrap, gp::StringAlignmentCenter);
    let text_fmt = |s: &str, f: *mut gp::GpFont, brush: *mut gp::GpBrush, x: f32, y: f32, w: f32, h: f32, fm: *mut gp::GpStringFormat| {
        let s: Vec<u16> = s.encode_utf16().collect();
        let r = gp::RectF { X: x, Y: y, Width: w, Height: h };
        gp::GdipDrawString(g, s.as_ptr(), s.len() as i32, f, &r, fm, brush);
    };
    let text = |s: &str, f: *mut gp::GpFont, brush: *mut gp::GpBrush, x: f32, y: f32, w: f32, h: f32| {
        text_fmt(s, f, brush, x, y, w, h, fmt)
    };
    let line = |color: u32, width: f32, x1: f32, y1: f32, x2: f32, y2: f32| {
        let mut pen = null_mut();
        gp::GdipCreatePen1(color, width, gp::UnitPixel, &mut pen);
        gp::GdipDrawLine(g, pen, x1, y1, x2, y2);
        gp::GdipDeletePen(pen);
    };

    let t = now();
    let red = argb(255, (234, 67, 53));
    // Time axis: PAST_SECS of fading past on the left, the red "now" marker, then one hour
    let x1 = w as f32 - 1.0 * sc; // now + 1 hour
    let ppm = x1 / ((PAST_SECS + WINDOW_SECS) as f32 / 60.0); // px per minute
    let x_of = |unix: i64| (unix - (t - PAST_SECS)) as f32 / 60.0 * ppm;
    let x_now = x_of(t);
    let fade_r = 10.0 * sc; // events running past the hour fade out over this width
    let hf = h as f32;
    let (top, bottom) = (2.0 * sc, hf - 5.0 * sc); // event area; quarter ticks below it

    // Clock grid: a faint labelled line at each full hour, small ticks at the quarters
    let f_label = font(8.5);
    let label_b = solid(a.theme.dim);
    let mut labels: Vec<(f32, String)> = Vec::new(); // drawn after the events, where uncovered
    let mut q = ((t - PAST_SECS) / 900 + 1) * 900;
    while q < t + WINDOW_SECS {
        let x = x_of(q);
        if local_time(q).wMinute == 0 {
            line(a.theme.grid, 1.0 * sc, x, top, x, hf - 1.0 * sc);
            if x > x_now + 4.0 * sc && x + 24.0 * sc < x1 {
                labels.push((x + 2.0 * sc, hm(q)));
            }
        } else {
            line(a.theme.dim, 1.0 * sc, x, hf - 4.0 * sc, x, hf - 1.0 * sc);
        }
        q += 900;
    }

    a.hits.clear();
    let dim = solid(a.theme.dim);
    let f_main = font(11.0);
    let f_small = font(9.5);
    let gradient = |xa: f32, ca: u32, xb: f32, cb: u32| {
        let (p1, p2) = (gp::PointF { X: xa, Y: 0.0 }, gp::PointF { X: xb, Y: 0.0 });
        let mut b = null_mut();
        gp::GdipCreateLineBrush(&p1, &p2, ca, cb, gp::WrapModeTileFlipX, &mut b);
        b as *mut gp::GpBrush
    };
    let fill_rect = |brush: *mut gp::GpBrush, xa: f32, xb: f32, y: f32, hh: f32| {
        if xb > xa {
            gp::GdipFillRectangle(g, brush, xa, y, xb - xa, hh);
        }
    };
    if a.status == Status::Ok {
        // Overlapping events go into lanes stacked on top of each other
        let mut ev: Vec<&Event> = visible(&a.events, t).collect();
        ev.sort_by_key(|e| (e.start, -e.end));
        let mut lane_end: Vec<i64> = Vec::new();
        let lanes: Vec<usize> = ev
            .iter()
            .map(|e| match lane_end.iter().position(|&end| end <= e.start) {
                Some(i) => {
                    lane_end[i] = e.end;
                    i
                }
                None => {
                    lane_end.push(e.end);
                    lane_end.len() - 1
                }
            })
            .collect();
        let n = lane_end.len().max(1) as f32;
        let gap = 2.0 * sc;
        let lh = (bottom - top - gap * (n - 1.0)) / n;
        let two_lines = lh >= 24.0 * sc; // room for the time under the title
        let f = if n > 1.0 { f_small } else { f_main };
        for (e, &lane) in ev.iter().zip(&lanes) {
            // Ends outside the window are pushed off-canvas, so only real ends are rounded
            let continues = e.end > t + WINDOW_SECS;
            let bx = if e.start < t - PAST_SECS { -20.0 * sc } else { x_of(e.start) };
            let bx2 = if continues { x1 + 20.0 * sc } else { x_of(e.end) - 1.0 * sc };
            let by = top + lane as f32 * (lh + gap);
            if bx2 - bx < 2.0 {
                continue;
            }
            let alpha = if e.tentative { 70 } else { 255 };
            let (c0, c1) = (argb(0, e.color), argb(alpha, e.color));
            let path = round_path(bx, by, bx2 - bx, lh, 4.0 * sc);
            gp::GdipSetClipPath(g, path, gp::CombineModeReplace);
            // the part before now fades out toward the left edge
            let solid_from = bx.max(x_now);
            if bx < x_now {
                let gb = gradient(-1.0, c0, x_now + 0.5, c1);
                fill_rect(gb, bx.max(0.0), x_now + 0.5, by, lh);
                gp::GdipDeleteBrush(gb);
            }
            // the part after the hour fades out toward the right edge
            let solid_to = if continues { x1 - fade_r } else { bx2 };
            let sb = solid(c1);
            fill_rect(sb, solid_from, solid_to + 0.5, by, lh);
            gp::GdipDeleteBrush(sb);
            if continues {
                let gb = gradient(x1 - fade_r, c1, x1 + 0.5, c0);
                fill_rect(gb, x1 - fade_r, x1, by, lh);
                gp::GdipDeleteBrush(gb);
            }
            if e.tentative && solid_to > solid_from {
                // outline only on the solid part
                gp::GdipSetClipRect(g, solid_from, by - 2.0, solid_to - solid_from, lh + 4.0, gp::CombineModeReplace);
                let mut pen = null_mut();
                gp::GdipCreatePen1(c1 | 0xFF00_0000, 1.0 * sc, gp::UnitPixel, &mut pen);
                gp::GdipDrawPath(g, pen, path);
                gp::GdipDeletePen(pen);
            }
            gp::GdipResetClip(g);
            gp::GdipDeletePath(path);

            // Text starts at "now" so an event in progress stays readable
            let tx = solid_from + 4.0 * sc;
            let tw = (if continues { x1 } else { bx2 }) - tx - 2.0 * sc;
            if tw > 10.0 * sc {
                let (r, gg, b) = e.color;
                let light = 0.299 * r as f32 + 0.587 * gg as f32 + 0.114 * b as f32 > 170.0;
                let rgb = if e.tentative {
                    a.theme.fg & 0xFF_FFFF
                } else if light {
                    0x20_2124
                } else {
                    0xFF_FFFF
                };
                let tb = solid(0xFF00_0000 | rgb);
                if two_lines {
                    text(&e.title, f, tb, tx, by + lh * 0.06, tw, lh * 0.5);
                    let tb2 = solid(0xC000_0000 | rgb);
                    text(&format!("{}–{}", hm(e.start), hm(e.end)), f_small, tb2, tx, by + lh * 0.5, tw, lh * 0.44);
                    gp::GdipDeleteBrush(tb2);
                } else {
                    text(&e.title, f, tb, tx, by, tw, lh);
                }
                gp::GdipDeleteBrush(tb);
            }
            let link = e.meet.clone().unwrap_or_else(|| e.link.clone());
            a.hits.push((bx.max(0.0), by, bx2.min(x1), by + lh, link));
        }
        if !ev.iter().any(|e| e.end > t) {
            let next = a.events.iter().find(|e| e.start >= t + WINDOW_SECS && same_day(e.start, t));
            let msg = match next {
                Some(e) => format!("Free · next {} {}", hm(e.start), e.title),
                None => "Free".to_string(),
            };
            // below the hour labels' row
            text(&msg, f_main, dim, x_now + 4.0 * sc, top + 9.0 * sc, x1 - x_now - 4.0 * sc, bottom - top - 9.0 * sc);
        }
    } else {
        let msg = match a.status {
            Status::NoClient => "Calendar: not set up (hover)",
            Status::SignedOut if a.signing_in => "Calendar: sign in in browser…",
            Status::SignedOut => "Calendar: click to sign in",
            Status::Error => "Calendar: offline",
            _ => "Calendar: loading…",
        };
        text(msg, f_main, dim, x_now + 4.0 * sc, top, x1 - x_now - 4.0 * sc, bottom - top);
    }

    let (lw, lh) = (24.0 * sc, 10.0 * sc);
    for (x, s) in &labels {
        let covered = a.hits.iter().any(|h| x + lw > h.0 && *x < h.2 && top + lh > h.1 && top < h.3);
        if !covered {
            text(s, f_label, label_b, *x, top, lw + 6.0 * sc, lh);
        }
    }

    // "Now": red line with a dot on top, over anything in progress
    let r = 3.0 * sc;
    line(red, 2.0 * sc, x_now, top + r, x_now, bottom);
    let red_b = solid(red);
    gp::GdipFillEllipse(g, red_b, x_now - r, top, r * 2.0, r * 2.0);
    gp::GdipDeleteBrush(red_b);

    gp::GdipDeleteBrush(dim);
    gp::GdipDeleteBrush(label_b);
    gp::GdipDeleteFont(f_label);
    gp::GdipDeleteFont(f_main);
    gp::GdipDeleteFont(f_small);
    gp::GdipDeleteStringFormat(fmt);
    gp::GdipDeleteStringFormat(fmt_wrap);
    gp::GdipDeleteFontFamily(family);
    gp::GdipDeleteGraphics(g);
    gp::GdipDisposeImage(canvas as *mut gp::GpImage);

    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
    UpdateLayeredWindow(hwnd, screen, null(), &size, mem, &src, 0, &blend, ULW_ALPHA);
    SelectObject(mem, old);
    DeleteObject(bmp as HGDIOBJ);
    DeleteDC(mem);
    ReleaseDC(null_mut(), screen);
}

// ---- input --------------------------------------------------------------------------------

fn cursor_x() -> i32 {
    let mut p: POINT = unsafe { zeroed() };
    unsafe { GetCursorPos(&mut p) };
    p.x
}

unsafe fn click(x: f32, y: f32) {
    let a = app();
    match a.status {
        Status::NoClient if !google::has_client(&a.dir) => {
            let _ = std::fs::create_dir_all(&a.dir);
            google::open_url(&a.dir.to_string_lossy());
        }
        Status::NoClient | Status::SignedOut => start_sign_in(),
        Status::Ok => {
            let hit = a.hits.iter().find(|h| x >= h.0 && x <= h.2 && y >= h.1 && y <= h.3);
            match hit {
                Some(h) if !h.4.is_empty() => google::open_url(&h.4),
                _ => google::open_url(CALENDAR_URL),
            }
        }
        _ => start_fetch(),
    }
}

unsafe fn show_menu() {
    let a = app();
    let menu = CreatePopupMenu();
    let item = |id: i32, s: &str| AppendMenuW(menu, MF_STRING, id as usize, wide(s).as_ptr());
    item(CMD_REFRESH, "Refresh now");
    item(CMD_OPEN_CALENDAR, "Open Google Calendar");
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    if !google::has_client(&a.dir) {
        item(CMD_OPEN_FOLDER, "Open setup folder");
    } else if google::has_token(&a.dir) {
        item(CMD_SIGN_OUT, "Sign out");
    } else {
        item(CMD_SIGN_IN, "Sign in with Google");
    }
    item(CMD_RESET_POSITION, "Reset position");
    item(CMD_EXIT, "Exit");
    let mut p: POINT = zeroed();
    GetCursorPos(&mut p);
    SetForegroundWindow(a.owner); // so the menu closes when clicking elsewhere
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.x, p.y, 0, a.owner, null());
    PostMessageW(a.owner, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match cmd {
        CMD_REFRESH => start_fetch(),
        CMD_OPEN_CALENDAR => google::open_url(CALENDAR_URL),
        CMD_SIGN_IN => start_sign_in(),
        CMD_SIGN_OUT => {
            google::sign_out(&a.dir);
            a.events.clear();
            a.status = Status::SignedOut;
            redraw();
        }
        CMD_OPEN_FOLDER => {
            let _ = std::fs::create_dir_all(&a.dir);
            google::open_url(&a.dir.to_string_lossy());
        }
        CMD_RESET_POSITION => {
            a.offset = None;
            let _ = std::fs::remove_file(&a.pos_file);
            place(true);
        }
        CMD_EXIT => {
            a.quitting = true;
            destroy_widget();
            PostQuitMessage(0);
        }
        _ => {}
    }
}

// ---- window procedures --------------------------------------------------------------------

fn lparam_xy(lp: LPARAM) -> (f32, f32) {
    ((lp & 0xFFFF) as i16 as f32, ((lp >> 16) & 0xFFFF) as i16 as f32)
}

extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                // Layered windows are redrawn with paint() directly; just validate here
                ValidateRect(hwnd, null());
                0
            }
            WM_ERASEBKGND => 1,
            WM_LBUTTONDOWN => {
                app().drag = Some((cursor_x(), current_offset(), false));
                SetCapture(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                if let Some((x0, off0, moved)) = app().drag {
                    let dx = cursor_x() - x0;
                    // a few pixels of slack so a click doesn't nudge the widget
                    if moved || dx.abs() > 3 {
                        app().drag = Some((x0, off0, true));
                        app().offset = Some(off0 - dx);
                        place(false);
                    }
                }
                0
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED => {
                if let Some((_, off0, moved)) = app().drag.take() {
                    if msg == WM_LBUTTONUP {
                        ReleaseCapture();
                    }
                    let a = app();
                    if moved {
                        if let Some(off) = a.offset.filter(|&o| o != off0) {
                            let _ = std::fs::write(&a.pos_file, format!("{{\"offset\": {off}}}"));
                        }
                    } else if msg == WM_LBUTTONUP {
                        let (x, y) = lparam_xy(lp);
                        click(x, y);
                    }
                }
                0
            }
            WM_RBUTTONUP => {
                show_menu();
                0
            }
            WM_DESTROY => {
                // Destroyed together with the taskbar when Explorer restarts: come back later
                let a = app();
                if a.widget == hwnd {
                    a.widget = null_mut();
                }
                if !a.quitting {
                    SetTimer(a.owner, TIMER_RECREATE, 2000, None);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

static mut TASKBAR_CREATED: u32 = 0;

extern "system" fn owner_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == TASKBAR_CREATED && msg != 0 {
            ensure_widget(); // Explorer restarted: attach to the new taskbar right away
            return 0;
        }
        match msg {
            WM_APP_FETCHED => {
                on_fetched();
                0
            }
            WM_APP_SIGNED_IN => {
                on_signed_in();
                0
            }
            WM_TIMER => {
                match wp {
                    TIMER_FETCH => start_fetch(),
                    TIMER_FETCH_SOON => {
                        KillTimer(hwnd, TIMER_FETCH_SOON);
                        start_fetch();
                    }
                    TIMER_RECREATE => {
                        KillTimer(hwnd, TIMER_RECREATE);
                        if !app().quitting {
                            ensure_widget();
                        }
                    }
                    TIMER_HOUSEKEEPING => {
                        ensure_widget();
                        if app().drag.is_none() {
                            place(false);
                        }
                        redraw(); // the hour window slides with the clock
                    }
                    _ => {}
                }
                0
            }
            WM_POWERBROADCAST => {
                if wp as u32 == PBT_APMRESUMEAUTOMATIC {
                    SetTimer(hwnd, TIMER_FETCH_SOON, 5000, None); // give the network a moment
                    redraw();
                }
                1
            }
            WM_SETTINGCHANGE | WM_DISPLAYCHANGE => {
                let a = app();
                a.theme = load_theme();
                style_tooltip(a.tooltip, a.theme.light);
                place(true);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ---- main ---------------------------------------------------------------------------------

fn main() {
    unsafe {
        CreateMutexW(null(), 1, wide(r"Local\CalendarTaskbarWidget").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        // If it ever crashes (after running 60 s+), Windows restarts it
        windows_sys::Win32::System::Recovery::RegisterApplicationRestart(null(), 0);

        let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
        InitCommonControlsEx(&icc);
        let mut token = 0usize;
        let input = gp::GdiplusStartupInput { GdiplusVersion: 1, ..zeroed() };
        gp::GdiplusStartup(&mut token, &input, null_mut());

        let hinst = GetModuleHandleW(null()) as HINSTANCE;
        let mut wc: WNDCLASSW = zeroed();
        wc.hInstance = hinst;
        wc.lpfnWndProc = Some(owner_proc);
        let owner_class = wide("CalendarTaskbarOwner");
        wc.lpszClassName = owner_class.as_ptr();
        RegisterClassW(&wc);
        wc.lpfnWndProc = Some(widget_proc);
        wc.hCursor = LoadCursorW(null_mut(), IDC_HAND);
        let widget_class = wide("CalendarTaskbarWidget");
        wc.lpszClassName = widget_class.as_ptr();
        RegisterClassW(&wc);

        // Hidden top-level window (not message-only, so it receives the TaskbarCreated broadcast)
        let owner = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            owner_class.as_ptr(),
            wide("Calendar taskbar widget").as_ptr(),
            WS_POPUP,
            0, 0, 0, 0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        TASKBAR_CREATED = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());

        let dir = PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default())
            .join(".claude")
            .join("calendar-widget");
        let _ = std::fs::create_dir_all(&dir);
        let pos_file = dir.join("position.json");
        let status = if !google::has_client(&dir) {
            Status::NoClient
        } else if !google::has_token(&dir) {
            Status::SignedOut
        } else {
            Status::Loading
        };
        APP = Some(App {
            hinst,
            owner,
            widget: null_mut(),
            tooltip: null_mut(),
            tip: String::new(),
            taskbar: null_mut(),
            offset: load_offset(&pos_file),
            dir,
            pos_file,
            events: Vec::new(),
            status,
            updated: None,
            error: None,
            fetching: false,
            signing_in: false,
            scale: 1.0,
            bounds: (0, 0, 0, 0),
            drag: None,
            hits: Vec::new(),
            theme: load_theme(),
            quitting: false,
        });

        if !create_widget() {
            SetTimer(owner, TIMER_RECREATE, 2000, None); // taskbar not up yet (early at login)
        }
        SetTimer(owner, TIMER_HOUSEKEEPING, 15_000, None);
        SetTimer(owner, TIMER_FETCH, 120_000, None);
        start_fetch();

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        gp::GdiplusShutdown(token);
    }
}
