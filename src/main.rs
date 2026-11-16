//! Always-on-top, click-through overlay that shows CPU / GPU / RAM / SSD temperatures.
//!
//! Hotkeys:
//!   Ctrl+Alt+T  show / hide
//!   Ctrl+Alt+P  move to next screen corner
//!   Ctrl+Alt+Q  quit
//!
//! Options:
//!   --port <n>  LibreHardwareMonitor web server port (default 8085)
//!   --once      print one reading to stdout and exit (diagnostics)

#![windows_subsystem = "windows"]

mod sensors;

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::time::Duration;

use sensors::{Readings, Sensors};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---- appearance -----------------------------------------------------------

const FONT_PT: i32 = 13; // font height in px at 100% scale
const OPACITY: u8 = 215; // 0..255 for the whole overlay
const MARGIN: i32 = 12; // gap from the screen edge (px at 100%)

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}
const BG: COLORREF = rgb(18, 18, 22);
const LABEL: COLORREF = rgb(170, 170, 180);
const DIM: COLORREF = rgb(110, 110, 120);
const OK: COLORREF = rgb(90, 220, 120);
const WARM: COLORREF = rgb(255, 200, 70);
const HOT: COLORREF = rgb(255, 85, 85);

/// (label, warm threshold, hot threshold)
const ROWS: [(&str, f32, f32); 4] = [
    ("CPU", 75.0, 90.0),
    ("GPU", 70.0, 85.0),
    ("RAM", 60.0, 80.0),
    ("SSD", 55.0, 70.0),
];

// ---- shared state -----------------------------------------------------------

/// Written by the sensor thread, read by the UI thread.
static LATEST: Mutex<Readings> = Mutex::new(Readings::EMPTY);

const HK_TOGGLE: i32 = 1;
const HK_CORNER: i32 = 2;
const HK_QUIT: i32 = 3;
const TIMER_ID: usize = 1;

struct Ui {
    font: HFONT,
    small_font: HFONT,
    scale: f32,
    shown: Readings,
    visible: bool,
    corner: u8, // 0 TL, 1 TR, 2 BR, 3 BL
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn px(scale: f32, v: i32) -> i32 {
    (v as f32 * scale).round() as i32
}

fn color_for(t: f32, warm: f32, hot: f32) -> COLORREF {
    if t >= hot {
        HOT
    } else if t >= warm {
        WARM
    } else {
        OK
    }
}

fn values(r: &Readings) -> [Option<f32>; 4] {
    [r.cpu, r.gpu, r.ram, r.ssd]
}

// ---- layout & painting ------------------------------------------------------

struct Layout {
    w: i32,
    h: i32,
    pad: i32,
    line_h: i32,
    footer_h: i32,
}

unsafe fn text_size(hdc: HDC, font: HFONT, s: &str) -> SIZE {
    let w: Vec<u16> = s.encode_utf16().collect();
    let old = SelectObject(hdc, font as _);
    let mut sz = SIZE { cx: 0, cy: 0 };
    GetTextExtentPoint32W(hdc, w.as_ptr(), w.len() as i32, &mut sz);
    SelectObject(hdc, old);
    sz
}

unsafe fn layout(ui: &Ui) -> Layout {
    let hdc = GetDC(null_mut());
    let label = text_size(hdc, ui.font, "CPU");
    let value = text_size(hdc, ui.font, "100°C");
    let footer = text_size(hdc, ui.small_font, "LHM offline");
    ReleaseDC(null_mut(), hdc);

    let pad = px(ui.scale, 8);
    let gap = px(ui.scale, 14);
    let line_h = label.cy + px(ui.scale, 2);
    let footer_h = if ui.shown.lhm_online { 0 } else { footer.cy + px(ui.scale, 2) };
    let w = (pad * 2 + label.cx + gap + value.cx).max(pad * 2 + footer.cx);
    let h = pad * 2 + line_h * ROWS.len() as i32 + footer_h;
    Layout { w, h, pad, line_h, footer_h }
}

unsafe fn draw_text(hdc: HDC, s: &str, rc: &mut RECT, color: COLORREF, flags: u32) {
    let mut w: Vec<u16> = s.encode_utf16().collect();
    SetTextColor(hdc, color);
    DrawTextW(hdc, w.as_mut_ptr(), w.len() as i32, rc, flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
}

unsafe fn paint(hwnd: HWND, ui: &Ui) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);

    // Double buffer to avoid flicker.
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old_bmp = SelectObject(mem, bmp as _);

    let bg = CreateSolidBrush(BG);
    FillRect(mem, &rc, bg);
    DeleteObject(bg as _);
    SetBkMode(mem, TRANSPARENT as _);

    let l = layout(ui);
    let old_font = SelectObject(mem, ui.font as _);
    for (i, ((label, warm, hot), v)) in ROWS.iter().zip(values(&ui.shown)).enumerate() {
        let top = l.pad + i as i32 * l.line_h;
        let mut row = RECT { left: l.pad, top, right: w - l.pad, bottom: top + l.line_h };
        draw_text(mem, label, &mut row, LABEL, DT_LEFT);
        match v {
            Some(t) => draw_text(mem, &format!("{:.0}°C", t), &mut row, color_for(t, *warm, *hot), DT_RIGHT),
            None => draw_text(mem, "--", &mut row, DIM, DT_RIGHT),
        }
    }
    if l.footer_h > 0 {
        SelectObject(mem, ui.small_font as _);
        let top = l.pad + ROWS.len() as i32 * l.line_h;
        let mut row = RECT { left: l.pad, top, right: w - l.pad, bottom: top + l.footer_h };
        draw_text(mem, "LHM offline", &mut row, DIM, DT_LEFT);
    }
    SelectObject(mem, old_font);

    BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old_bmp);
    DeleteObject(bmp as _);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
}

/// Size the window for the current content and pin it to the chosen corner of the work area.
unsafe fn place(hwnd: HWND, ui: &Ui) {
    let l = layout(ui);
    let mut work: RECT = std::mem::zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut _ as *mut c_void, 0);
    let m = px(ui.scale, MARGIN);
    let x = if ui.corner == 1 || ui.corner == 2 { work.right - l.w - m } else { work.left + m };
    let y = if ui.corner >= 2 { work.bottom - l.h - m } else { work.top + m };
    SetWindowPos(hwnd, HWND_TOPMOST, x, y, l.w, l.h, SWP_NOACTIVATE);
}

// ---- window procedure -------------------------------------------------------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            UI.with(|u| {
                if let Some(ui) = u.borrow().as_ref() {
                    paint(hwnd, ui);
                }
            });
            0
        }
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            let latest = *LATEST.lock().unwrap();
            UI.with(|u| {
                if let Some(ui) = u.borrow_mut().as_mut() {
                    let resize = latest.lhm_online != ui.shown.lhm_online;
                    let changed = latest != ui.shown;
                    ui.shown = latest;
                    if resize {
                        place(hwnd, ui);
                    }
                    if changed {
                        InvalidateRect(hwnd, null(), 0);
                    }
                    if ui.visible {
                        // Some full-screen apps push themselves above us; reassert topmost.
                        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                }
            });
            0
        }
        WM_HOTKEY => {
            match wp as i32 {
                HK_TOGGLE => UI.with(|u| {
                    if let Some(ui) = u.borrow_mut().as_mut() {
                        ui.visible = !ui.visible;
                        ShowWindow(hwnd, if ui.visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
                    }
                }),
                HK_CORNER => UI.with(|u| {
                    if let Some(ui) = u.borrow_mut().as_mut() {
                        ui.corner = (ui.corner + 1) % 4;
                        place(hwnd, ui);
                    }
                }),
                HK_QUIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            UI.with(|u| {
                if let Some(ui) = u.borrow().as_ref() {
                    place(hwnd, ui);
                }
            });
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- entry point ------------------------------------------------------------

fn parse_port() -> u16 {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(8085)
}

unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    CreateFontW(
        -height, 0, 0, 0, weight, 0, 0, 0,
        DEFAULT_CHARSET as _, OUT_DEFAULT_PRECIS as _, CLIP_DEFAULT_PRECIS as _,
        CLEARTYPE_QUALITY as _, (DEFAULT_PITCH | FF_DONTCARE) as _,
        face.as_ptr(),
    )
}

/// LibreHardwareMonitor.exe as installed by winget. Use the real package folder, not the
/// WinGet\Links symlink: LHM looks for its settings file next to the path it was started from.
fn find_lhm() -> Option<std::path::PathBuf> {
    let base = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(r"Microsoft\WinGet");
    std::fs::read_dir(base.join("Packages"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("LibreHardwareMonitor.LibreHardwareMonitor"))
        .map(|e| e.path().join("LibreHardwareMonitor.exe"))
        .find(|p| p.exists())
}

/// If LibreHardwareMonitor's web server isn't answering, start LHM elevated (one UAC prompt).
fn ensure_lhm_running(port: u16) {
    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if TcpStream::connect_timeout(&addr, Duration::from_millis(1500)).is_ok() {
        return;
    }
    let Some(exe) = find_lhm() else { return };
    let file = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            null_mut(), verb.as_ptr(), file.as_ptr(), null(), null(), SW_SHOWMINNOACTIVE,
        );
    }
}

fn main() {
    // Diagnostic mode: print one reading and exit (e.g. `temp-overlay.exe --once | more`).
    if std::env::args().any(|a| a == "--once") {
        println!("{:?}", Sensors::new(parse_port()).read());
        return;
    }
    unsafe {
        // Single instance.
        let mutex_name = wide("Local\\TempOverlaySingleInstance");
        CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let scale = GetDpiForSystem() as f32 / 96.0;

        // Sensor thread: refresh once per second.
        let port = parse_port();
        std::thread::spawn(move || ensure_lhm_running(port));
        std::thread::spawn(move || {
            let mut sensors = Sensors::new(port);
            loop {
                let r = sensors.read();
                *LATEST.lock().unwrap() = r;
                std::thread::sleep(Duration::from_secs(1));
            }
        });

        let hinst = GetModuleHandleW(null());
        let class = wide("TempOverlayWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);

        // Layered + transparent = click-through; toolwindow = hidden from Alt+Tab/taskbar;
        // noactivate = never steals focus from the game/app.
        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let title = wide("Temp Overlay");
        let hwnd = CreateWindowExW(
            ex, class.as_ptr(), title.as_ptr(), WS_POPUP,
            0, 0, 100, 100,
            null_mut(), null_mut(), hinst, null(),
        );
        if hwnd.is_null() {
            return;
        }
        SetLayeredWindowAttributes(hwnd, 0, OPACITY, LWA_ALPHA);
        let round: u32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &round as *const _ as *const c_void, 4);

        let ui = Ui {
            font: make_font(px(scale, FONT_PT), FW_SEMIBOLD as i32),
            small_font: make_font(px(scale, FONT_PT - 3), FW_NORMAL as i32),
            scale,
            shown: Readings::EMPTY,
            visible: true,
            corner: 0,
        };
        place(hwnd, &ui);
        UI.with(|u| *u.borrow_mut() = Some(ui));

        let mods = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
        RegisterHotKey(hwnd, HK_TOGGLE, mods, 'T' as u32);
        RegisterHotKey(hwnd, HK_CORNER, mods, 'P' as u32);
        RegisterHotKey(hwnd, HK_QUIT, mods, 'Q' as u32);

        SetTimer(hwnd, TIMER_ID, 500, None);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
