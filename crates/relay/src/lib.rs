//! fleetd-relay: heartbeat pull protocol.
//!
//! 中继协议层(2026-11-27 拆分):flagship 侧的 heartbeat 端点处理与 ship 侧
//! 的心跳拉单循环共用同一契约(Heartbeat/HeartbeatReply/PullDispatch),
//! 单独成 crate 让 server(端点)与 agent(循环)各自依赖,协议形态单一事实。

pub mod forward;
pub mod relay;

pub use forward::queue_pull;
pub use forward::PullDispatch;
pub use relay::{heartbeat, Heartbeat, HeartbeatReply};
