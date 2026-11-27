//! service: native service registration and lifecycle (systemd/launchd/SCM).
//! Deployment concern, not scheduling. The SCM entry point takes a callback
//! for the serve loop so this crate never depends on the scheduler.

#[cfg(windows)]
pub mod scm_service;
pub mod service;

pub use service::{detect, launchd_plist, systemd_unit, windows_commands, Platform};
#[cfg(windows)]
pub use scm_service::run;
