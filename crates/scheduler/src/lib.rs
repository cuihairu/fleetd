//! fleetd-scheduler: events HTTP face, scheduler, headless runs, MCP, pyhost, SCM.
//! 服务器面层——依赖 fleetd-core 的账本/派单原语。

pub mod attach;
pub mod events;
pub mod headless;
pub mod infra;
pub mod jobs;
pub mod presence;
pub mod pyhost;
pub mod scheduler;
pub mod selfupdate;
// The Windows SCM face moved to the `service` crate with the 2026-11-27
// split; it is declared there (cfg-gated), not here.
