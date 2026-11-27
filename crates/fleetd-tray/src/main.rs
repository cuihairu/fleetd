//! fleetd-tray: standalone windows tray indicator binary.
//!
//! 独立二进制(2026-11-27 拆分):零 python 依赖、无 DLL 闭包负担,部署只拷
//! 一个小 exe。主线程 spawn 后阻塞等待窗口线程结束(主线程 return = 进程
//! 退出 = tray 秒退,2026-11-27 cuihaitao 真机实证)。

#[cfg(target_os = "windows")]
fn main() {
    let _spawn = fleetd_tray_lib::spawn;
    // spawn then block forever inside the library face
    fleetd_tray_lib::spawn();
    fleetd_tray_lib::wait_forever();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("fleetd-tray: windows only (tray is a Windows shell surface)");
}
