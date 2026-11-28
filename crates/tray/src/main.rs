//! fleetd-tray: standalone windows tray indicator binary.
//!
//! Standalone binary (split off 2026-11-27): zero python dependency, no DLL
//! closure baggage, deployment is copying one small exe. The main thread
//! blocks forever while the window thread lives (a returning main = process
//! exit = instant tray death, seen live on cuihaitao 2026-11-27).

#[cfg(target_os = "windows")]
fn main() {
    // Bind the StopHandle for the whole process lifetime: dropping it asks
    // the tray to remove its icon and quit (StopHandle::drop), so a plain
    // `tray_lib::spawn();` would kill the tray the instant main moves on.
    let _stop = fleetd_tray_lib::spawn();
    fleetd_tray_lib::wait_forever();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("fleetd-tray: windows only (tray is a Windows shell surface)");
}
