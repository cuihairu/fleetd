//! 注册中心面:节点心跳登记(agent_seen)与存活汇总。
//! 2026-11-27 拆为独立 crate fleetd-register(注册中心/转发两相分离)。

use anyhow::{Context, Result};
use rusqlite::Connection;

pub const TABLE: &str = "
CREATE TABLE IF NOT EXISTS agent_seen (
    seat TEXT PRIMARY KEY,
    last_seen INTEGER NOT NULL,
    head TEXT,
    dirty INTEGER,
    pulls INTEGER NOT NULL DEFAULT 0
);
";

pub fn db() -> Result<Connection> {
    let c = store::ledger::open()?;
    c.execute_batch(TABLE)
        .with_context(|| "create agent_seen table")?;
    Ok(c)
}

/// Record one heartbeat's liveness payload.
pub fn record_heartbeat(seat: &str, head: Option<String>, dirty: Option<i64>) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let c = db()?;
    c.execute(
        "INSERT INTO agent_seen(seat,last_seen,head,dirty,pulls)
         VALUES(?1,?2,?3,?4,0)
         ON CONFLICT(seat) DO UPDATE SET
           last_seen=excluded.last_seen, head=excluded.head,
           dirty=excluded.dirty",
        rusqlite::params![seat, now, head, dirty],
    )
    .with_context(|| "upsert agent_seen")?;
    Ok(())
}

/// Ships with a heartbeat in the last `minutes` (the `fleetd status` face).
pub fn alive_since(minutes: i64) -> Result<Vec<(String, i64)>> {
    let c = db()?;
    let cutoff = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64)
        - minutes * 60;
    let mut stmt = c.prepare(
        "SELECT seat, last_seen FROM agent_seen WHERE last_seen > ?1 ORDER BY seat",
    )?;
    let rows = stmt.query_map([cutoff], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Pull counter bump — called by fleetd-relay::forward when a dispatch is
/// handed out (registration center owns the counter, forward only consumes).
pub fn bump_pulls(seat: &str) -> Result<()> {
    let c = db()?;
    c.execute("UPDATE agent_seen SET pulls = pulls + 1 WHERE seat = ?1", [seat])
        .with_context(|| "bump pulls")?;
    Ok(())
}
