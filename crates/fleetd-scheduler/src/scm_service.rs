// Windows SCM (Service Control Manager) face for `fleetd serve`.
//
// When the binary is started by the Windows service controller, the process
// MUST talk the SCM protocol within ~30 s or the start is killed with error
// 1053 (observed live 2026-11-27). `fleetd service install` registers the
// service with the `--service` argument appended, so main() routes here:
// we report RUNNING immediately, spawn the real serve on a thread, and
// translate SCM stop into a clean process exit.
//
// Reference: windows-service 0.8 (dispatcher_table + service_control_handler).

#![cfg(target_os = "windows")]

use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{
    define_windows_service,
    service_control_handler::ServiceStatusHandle,
    service_dispatcher,
};

define_windows_service!(ffi_service_main, fleetd_service_main);

/// Entry invoked by the SCM thread: dispatch (blocks until stop).
pub fn run() -> windows_service::Result<()> {
    service_dispatcher::start("FleetD", ffi_service_main)
}

fn fleetd_service_main(args: Vec<std::ffi::OsString>) {
    // SCM context has no stderr — persist any failure to FLEETD_SVC_ERR so
    // remote debugging can read it (stdout/stderr are unusable in SCM).
    if let Err(e) = run_service(args) {
        let msg = format!("fleetd service error: {e:?}\n");
        let _ = std::fs::write(
            std::env::var("FLEETD_SVC_ERR").unwrap_or_else(|_| "C:\\fleetd-svc-err.txt".into()),
            msg,
        );
    }
}

fn run_service(_args: Vec<std::ffi::OsString>) -> windows_service::Result<()> {
    let (tx, rx) = mpsc::channel();
    let handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop => {
                let _ = tx.send(());
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle: ServiceStatusHandle = service_control_handler::register("FleetD", handler)?;

    // Announce RUNNING right away — the 30 s handshake clock is ticking.
    status_handle.set_service_status(running_status())?;

    // The real serve: serve on a thread so this thread can watch the stop
    // channel and shut down cleanly.
    let serve_thread = std::thread::Builder::new()
        .name("fleetd-serve".into())
        // 32 MiB: the serve stack hosts pyhost init with deep frames; the
        // 2 MiB default overflow-killed the whole process (7031 hunt).
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            if let Err(e) = crate::run_serve_from_service() {
                eprintln!("fleetd serve error: {e:#}");
            }
        })
        .map_err(|e| windows_service::Error::Winapi(e.into()))?;

    // Block until SCM says stop (1 s polling keeps shutdown responsive and
    // treats a dropped handler channel as stop).
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    let _ = serve_thread;

    status_handle.set_service_status(stopped_status())?;
    Ok(())
}

fn running_status() -> ServiceStatus {
    ServiceStatus {
        service_type: windows_service::service::ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    }
}

fn stopped_status() -> ServiceStatus {
    ServiceStatus {
        service_type: windows_service::service::ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    }
}
