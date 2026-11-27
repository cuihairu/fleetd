//! fleetd-tray library face (windows-only; the tray is a Windows shell surface).

#![cfg(target_os = "windows")]

pub mod tray;

pub use tray::spawn;
pub use tray::wait_forever;
