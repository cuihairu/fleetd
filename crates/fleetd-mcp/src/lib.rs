//! fleetd-mcp: stdio MCP tool face.
//!
//! 独立入口(fleetd mcp 子命令):对 Claude/Cursor 等外部 AI 客户端暴露
//! fleet 查询/操作工具。与旗舰 HTTP 面(scheduler/events)无运行时耦合,
//! 单独成 crate(2026-11-27 拆分:此前误混入 flagship 服务层)。

pub mod mcp_server;
pub mod tools;
pub use mcp_server::run_stdio;
