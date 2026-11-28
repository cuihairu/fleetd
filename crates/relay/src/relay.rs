//! relay 协议与编排面:Heartbeat/Reply 类型 + heartbeat 编排(调 register
//! 登记 + forward 取件)。登记细节在 fleetd-register,队列细节在 forward。

use crate::forward::{pending_pull, PullDispatch};
use anyhow::{Context, Result};
use register::registry::record_heartbeat;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct Heartbeat {
    pub seat: String,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub dirty: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatReply {
    pub ok: bool,
    /// Pending pull-dispatch for this seat (None = nothing to do).
    pub dispatch: Option<PullDispatch>,
}

/// Handle one heartbeat: record liveness, hand back the pending dispatch.
pub fn heartbeat(raw: &str) -> Result<HeartbeatReply> {
    let hb: Heartbeat = serde_json::from_str(raw).with_context(|| "parse heartbeat JSON")?;
    if hb.seat.is_empty() || hb.seat.contains('/') {
        anyhow::bail!("invalid seat name");
    }
    record_heartbeat(&hb.seat, hb.head.clone(), hb.dirty)?;
    let c = register::registry::db()?;
    Ok(HeartbeatReply {
        ok: true,
        dispatch: pending_pull(&c, &hb.seat)?,
    })
}

pub use register::alive_since;
