//! fleetd-scheduler: events HTTP face, scheduler, headless runs, MCP, pyhost, SCM.
//! 服务器面层——依赖 fleetd-core 的账本/派单原语。

pub mod events;
pub mod headless;
pub mod jobs;
pub mod pyhost;
pub mod scheduler;
// Windows SCM face is windows-only (the SCM protocol exists only there).
#[cfg(target_os = "windows")]
pub mod scm_service;
pub mod service;
