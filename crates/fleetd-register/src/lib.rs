//! fleetd-register: node registry (registration center face).
//!
//! 注册中心(2026-11-27 拆分):agent_seen 表的登记/存活/计数——纯 API 层,
//! 不含协议形态(Heartbeat 类型在 fleetd-relay)也不含转发队列(tasks 表在
//! fleetd-relay::forward;转发经 bump_pulls 只增计数,表结构互不知晓)。

pub mod registry;

pub use registry::{alive_since, bump_pulls, record_heartbeat};
