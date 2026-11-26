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
const FRAME_MS: u32 = 150;
const FRAME_DATA: &[u8] = include_bytes!("tray_frames/frames.bin");

const TIMER_ID: usize = 1;
const WM_APP: u32 = 0x8000;

static G_NID: AtomicIsize = AtomicIsize::new(0);
static G_EXIT: AtomicBool = AtomicBool::new(false);

// --- Right-click menu (2026-11-27 用户令「右键能看状态」) -------------------
// Status line lives in a global set by the serve thread (UTF-16 ready);
// menu ids are small consts so WM_COMMAND can act on them.
const MENU_STATUS: usize = 100;
const MENU_OPEN_PANEL: usize = 101;
const MENU_QUIT: usize = 102;

/// Status text shown as the disabled first row of the menu, e.g.
/// "fleetd 0.1.0 — hub :3790". Written by serve before the menu opens.
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
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
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
pub fn spawn() -> Option<StopHandle> {
    std::thread::Builder::new()
        .name("fleetd-tray".into())
        .spawn(|| unsafe {
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

            SetTimer(hwnd, TIMER_ID, FRAME_MS, None);

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
    Some(StopHandle)
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
