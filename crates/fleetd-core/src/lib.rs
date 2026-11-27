//! fleetd-core: ledger, config, dispatch, jobs, topology.
//! 纯逻辑层——不做任何服务器 IO 面定义(事件循环/监听器在 fleetd-scheduler)。

pub mod agent;
pub mod config;
pub mod dispatcher;
pub mod fleet_view;
pub mod ledger;
pub mod probe;
pub mod resources;
pub mod schedule;
pub mod summary;
pub mod topology;
