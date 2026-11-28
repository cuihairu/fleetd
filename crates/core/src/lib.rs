//! core: pure domain models and shared primitives (zero IO).
//! Layering (user-approved 2026-11-27): dependency edges only point downward.
//! Persistence lives in `store`, machine facts in `probe`, business views in
//! `report`, service lifecycle in `service`, server IO in `scheduler`.

pub mod agent;
pub mod attach;
pub mod config;
pub mod dispatcher;
pub mod executor;
pub mod gate;
pub mod presence;
pub mod schedule;
pub mod topology;
pub mod verdict;
