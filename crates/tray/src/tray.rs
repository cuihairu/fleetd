//! Windows system tray for fleetd: a wood-colored gear slowly rotating so the
//! person at the machine can see fleetd is running.
//!
//! Frames are baked into the binary as raw RGBA bytes (24 frames, 64x64,
//! 150 ms each = one slow revolution every 3.6 s), generated from the
//! user-supplied gear SVG in `assets/tray/`:
//!   `gen_frames.py` renders the rotations -> PNGs -> `frames.bin`.
//!
//! Icons are built straight from those pixels via a DIB section +
//! `CreateIconIndirect` (same proven path as meshlet's tray host), so there is
//! no PNG/zlib decoder at runtime. Only the Windows build compiles this; other
//! platforms run fleetd headless. Any tray failure is silent — the daemon must
//! never die because of the tray.

// Windows-only desktop face. serve is a pure service on every platform;
// macOS/Linux desktop indicators are future UDS-socket faces (not "tray").
#![cfg(target_os = "windows")]

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HGDIOBJ,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DispatchMessageW, GetMessageW, GetCursorPos, KillTimer,
    PostQuitMessage, RegisterClassExW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, HICON, HMENU, ICONINFO, MF_DISABLED, MF_SEPARATOR, MF_STRING, MSG,
    TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WNDCLASSEXW, WM_COMMAND, WM_DESTROY, WM_RBUTTONUP,
    WM_TIMER,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const FRAME_LEN: usize = (WIDTH as usize) * (HEIGHT as usize) * 4;
const FRAME_COUNT: usize = 24;
const FRAME_DATA: &[u8] = include_bytes!("tray_frames/frames.bin");

const TIMER_ID: usize = 1;
const WM_APP: u32 = 0x8000;

/// Rotation cadence by in-flight dispatch count (2026-11-27 用户令「任务越多
/// 转动越快，最快也要看得清」): 0 tasks = idle drift (1.2 s/frame), each
/// running task speeds the gear up, clamped at 400 ms/frame (2.5 rev/min —
/// clearly readable, never a blur). The WM_TIMER tick re-arms itself with the
/// current cadence, so the speed tracks live load with no extra thread.
fn cadence_ms(in_flight: i64) -> u32 {
    if in_flight <= 0 {
        1200
    } else {
        // 1 task = 800 ms, 2 = 600, 3 = 480 ... floor at 400 ms so the
        // rotation always stays legible to a human eye.
        (800 / in_flight).max(400) as u32
    }
}

static G_NID: AtomicIsize = AtomicIsize::new(0);
static G_EXIT: AtomicBool = AtomicBool::new(false);

// --- Right-click menu (2026-11-27 用户令「右键能看状态」) -------------------
// Status line lives in a global set by the serve thread (UTF-16 ready);
// menu ids are small consts so WM_COMMAND can act on them.
const MENU_STATUS: usize = 100;
const MENU_OPEN_PANEL: usize = 101;
const MENU_QUIT: usize = 102;

/// Status text shown as the disabled first row of the menu, e.g.
/// "fleetd 0.1.0 — flagship :3790". Written by serve before the menu opens.
static G_STATUS: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_status(line: String) {
    if let Ok(mut slot) = G_STATUS.lock() {
        *slot = Some(line);
    }
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Build the context menu. Returns (menu, handled); caller destroys the menu.
unsafe fn build_menu() -> HMENU {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return std::ptr::null_mut();
    }
    // Admiral framing (2026-11-27 用户令): the human running this fleet is the
    // admiral; the menu speaks to them in fleet vocabulary.
    let status = G_STATUS
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_else(|| "fleetd — running".into());
    let wide = utf16(&status);
    AppendMenuW(menu, MF_DISABLED, 0, wide.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    let open_label = utf16("打开面板 (fleet overview)");
    AppendMenuW(menu, MF_STRING, MENU_OPEN_PANEL, open_label.as_ptr());
    let quit_label = utf16("退出 fleetd");
    AppendMenuW(menu, MF_STRING, MENU_QUIT, quit_label.as_ptr());
    menu
}

/// Shell out to open the fleet overview page in the default browser.
fn open_panel() {
    let url = std::env::var("FLEETD_PANEL_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3790/v1/fleet/summary".into());
    let _ = Command::new("cmd")
        .args(["/C", "start", "", &url])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn();
}

fn flagship_base() -> String {
    std::env::var("FLEETD_STATUS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3790/v1/status".into())
}

/// GET /v1/status → in_flight count; any failure = 0 (idle).
fn fetch_in_flight() -> i64 {
    fetch_status_field("in_flight")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
}

/// GET /v1/status → the human status line for the menu's first row.
fn fetch_status_line() -> Option<String> {
    let busy = fetch_status_field("in_flight")?;
    let version = fetch_status_field("version")?;
    let port = std::env::var("FLEETD_PANEL_URL")
        .ok()
        .and_then(|u| url_port(&u))
        .unwrap_or_else(|| 3790.to_string());
    Some(format!("fleetd {version} — flagship :{port} ({busy} 在飞)"))
}

fn url_port(url: &str) -> Option<String> {
    url.split("://")
        .nth(1)?
        .split('/')
        .next()?
        .split(':')
        .nth(1)
        .map(String::from)
}

/// One-line loopback GET returning a JSON numeric/string field.
/// Tray↔flagship 本机通信(2026-11-27 钦定):UNIX socket(Windows 10 1803+
/// 原生 AF_UNIX,std 同 API),不走 loopback TCP。路径与 flagship serve 一致:
/// %USERPROFILE%\.fleetd\flagship.sock。覆盖 FLEETD_STATUS_URL 仅在测试用。
fn fetch_status_field(field: &str) -> Option<String> {
    use std::io::{Read as _, Write as _};

    let path = std::env::var("FLEETD_STATUS_SOCK")
        .unwrap_or_else(|_| {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| ".".into());
            format!("{home}\\.fleetd\\flagship.sock")
        });
    let mut stream = uds::UnixStream::connect(path).ok()?;
    let _ = stream.write_all(
        format!("GET /v1/status HTTP/1.1\r\nHost: flagship\r\nConnection: close\r\n\r\n").as_bytes(),
    );
    let mut body = Vec::new();
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.read_to_end(&mut body);
    let body = String::from_utf8_lossy(&body);
    // HTTP response: skip headers (first empty line), then parse JSON field.
    let json = body.split("\r\n\r\n").nth(1).unwrap_or("");
    let pat = format!("\"{}\"", field);
    let start = json.find(&pat)? + pat.len();
    let rest = &json[start..];
    let rest = rest.trim_start_matches(|c| c == ':' || c == ' ');
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    let value = rest[..end].trim_matches('"').to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// One frame's raw RGBA pixels (top-down rows, as PIL writes them).
pub fn frame(idx: usize) -> &'static [u8] {
    let i = idx % FRAME_COUNT;
    &FRAME_DATA[i * FRAME_LEN..(i + 1) * FRAME_LEN]
}

/// Build an HICON from one frame: RGBA -> BGRA, blit into a 32-bit DIB section,
/// then CreateIconIndirect with a 1bpp AND mask (alpha drives transparency).
fn rgba_to_icon(rgba: &[u8]) -> HICON {
    let mut bgra = vec![0u8; FRAME_LEN];
    for (src, dst) in rgba.chunks_exact(4).zip(bgra.chunks_exact_mut(4)) {
        dst[0] = src[2]; // B
        dst[1] = src[1]; // G
        dst[2] = src[0]; // R
        dst[3] = src[3]; // A
    }
    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: WIDTH as i32,
            biHeight: -(HEIGHT as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits: *mut fleetd_core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            std::ptr::null_mut(),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if dib.is_null() || bits.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());

        let mut ii: ICONINFO = std::mem::zeroed();
        ii.fIcon = 1;
        ii.hbmColor = dib;
        ii.hbmMask = CreateBitmap(WIDTH as i32, HEIGHT as i32, 1, 1, std::ptr::null());
        let icon = CreateIconIndirect(&ii);

        DeleteObject(ii.hbmMask as HGDIOBJ);
        DeleteObject(dib as HGDIOBJ);
        icon
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match msg {
        WM_TIMER if wparam == TIMER_ID => {
            let nid_ptr = G_NID.load(Ordering::SeqCst) as *mut NOTIFYICONDATAW;
            if !nid_ptr.is_null() {
                // Rotate: advance frame, swap HICON, free the previous one.
                static FRAME_IDX: AtomicIsize = AtomicIsize::new(0);
                let idx = (FRAME_IDX.load(Ordering::SeqCst) + 1) as usize % FRAME_COUNT;
                FRAME_IDX.store(idx as isize, Ordering::SeqCst);
                let next = rgba_to_icon(frame(idx));
                if !next.is_null() {
                    let nid = &mut *nid_ptr;
                    let old = nid.hIcon;
                    nid.hIcon = next;
                    Shell_NotifyIconW(NIM_MODIFY, nid);
                    if !old.is_null() {
                        DestroyIcon(old);
                    }
                }
            }
            // Pull status over the loopback face (2026-11-27 用户令「tray 尽量
            // 用 UNIX socket 通信」): tray ↔ serve talk over the local status
            // endpoint instead of shared process memory, so the tray survives
            // serve moving to another process. Any failure reads as idle —
            // the tray must never die because the flagship hiccuped.
            let busy = fetch_in_flight();
            if let Some(line) = fetch_status_line() {
                if let Ok(mut slot) = G_STATUS.lock() {
                    *slot = Some(line);
                }
            }
            SetTimer(hwnd, TIMER_ID, cadence_ms(busy), None);
            0
        }
        // Tray icon callback: low word of lparam = mouse message.
        m if m == WM_APP + 1 && (lparam as u32 & 0xffff) == WM_RBUTTONUP => {
            // Foreground trick: the menu needs its owner window foreground or
            // it won't dismiss on outside click (classic Shell_NotifyIcon quirk).
            SetForegroundWindow(hwnd);
            let menu = build_menu();
            if !menu.is_null() {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                TrackPopupMenu(
                    menu,
                    TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                    pt.x,
                    pt.y,
                    0,
                    hwnd,
                    std::ptr::null(),
                );
                DestroyMenu(menu);
            }
            0
        }
        WM_COMMAND => {
            match wparam & 0xffff {
                MENU_OPEN_PANEL => open_panel(),
                MENU_QUIT => {
                    G_EXIT.store(true, Ordering::SeqCst);
                    // Ask the whole process to leave: tray is advisory only,
                    // and the user clicked Quit — serve should shut down.
                    std::process::exit(0);
                }
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Spawn the tray on a dedicated thread. Returns a handle whose drop asks the
/// tray to remove its icon and exit; failures are silent by design.
/// Window-thread handle for the standalone tray process (main blocks on
/// it: a returning main = process exit = instant tray death, seen live on
/// cuihaitao 2026-11-27).
static TRAY_THREAD: std::sync::Mutex<Option<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(None);

pub fn spawn() -> Option<StopHandle> {
    let handle = std::thread::Builder::new()
        .name("fleetd-tray".into())
        .spawn(move || unsafe {
            let class_name: Vec<u16> = "FleetdTrayHost\0".encode_utf16().collect();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(tray_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: GetModuleHandleW(std::ptr::null()),
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            RegisterClassExW(&wc);

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return;
            }

            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            nid.uCallbackMessage = WM_APP + 1;
            nid.hIcon = rgba_to_icon(frame(0));
            let tip: Vec<u16> = "fleetd - ship running\0".encode_utf16().collect();
            for (i, &w) in tip.iter().enumerate().take(127) {
                nid.szTip[i] = w;
            }
            Shell_NotifyIconW(NIM_ADD, &nid);
            G_NID.store(&mut nid as *mut _ as isize, Ordering::SeqCst);

            SetTimer(hwnd, TIMER_ID, 1200, None); // start idle; first tick re-arms by load

            // Message loop: serves the timer and keeps the window alive.
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if G_EXIT.load(Ordering::SeqCst) {
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            KillTimer(hwnd, TIMER_ID);
            Shell_NotifyIconW(NIM_DELETE, &nid);
            if !nid.hIcon.is_null() {
                DestroyIcon(nid.hIcon);
            }
        })
        .ok()?;
    if let Ok(mut slot) = TRAY_THREAD.lock() {
        let prev = slot.replace(handle);
        if let Some(prev) = prev {
            let _ = prev.join();
        }
    }
    Some(StopHandle)
}

/// standalone 进程主线程阻塞:窗口线程活着进程就活着;用户菜单退出路径
/// process::exit(0) 直接收进程,不需要跨线程协调。
pub fn wait_forever() {
    loop {
        let handle = TRAY_THREAD.lock().ok().and_then(|mut s| s.take());
        if let Some(h) = handle {
            let _ = h.join();
            std::process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub struct StopHandle;

impl Drop for StopHandle {
    fn drop(&mut self) {
        G_EXIT.store(true, Ordering::SeqCst);
        let nid_ptr = G_NID.swap(0, Ordering::SeqCst) as *mut NOTIFYICONDATAW;
        if !nid_ptr.is_null() {
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, nid_ptr);
                PostQuitMessage(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_frames_present_and_distinct() {
        assert_eq!(FRAME_DATA.len(), FRAME_LEN * FRAME_COUNT);
        for i in 0..FRAME_COUNT {
            assert_eq!(frame(i).len(), FRAME_LEN, "frame {i} length");
        }
        let diff = frame(0)
            .iter()
            .zip(frame(1).iter())
            .filter(|(a, b)| a != b)
            .count();
        assert!(diff > 100, "rotation must change pixels ({diff} bytes)");
    }

    #[test]
    fn cadence_speeds_up_with_load_and_clamps() {
        assert_eq!(cadence_ms(0), 1200); // idle drift
        assert_eq!(cadence_ms(1), 800);
        assert_eq!(cadence_ms(2), 400);
        assert_eq!(cadence_ms(3), 400); // 800/3=266 → clamp 400
        assert_eq!(cadence_ms(50), 400); // clamp: never faster than legible
        assert_eq!(cadence_ms(-3), 1200); // negative reads as idle
    }

    #[test]
    fn frames_are_wood_colored() {
        // User choice: light oak #C8A165 = (200, 161, 101).
        let woodish = frame(0)
            .chunks_exact(4)
            .filter(|p| {
                (p[0] as i32 - 200).abs() < 40
                    && (p[1] as i32 - 161).abs() < 40
                    && (p[2] as i32 - 101).abs() < 40
            })
            .count();
        assert!(woodish > 500, "expected wood pixels, got {woodish}");
    }
}
