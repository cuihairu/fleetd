//! Relay F2: agent-pull channel for ships behind NAT/unstable networks.
//!
//! Ships POST a heartbeat on an outbound timer; the flagship records liveness in
//! `agent_seen` and returns any pending pull-dispatch in the response body.
//! This is the ONLY channel for machines like cuihaitao / home-pve that
//! cannot hold inbound SSH (their network drops inbound; outbound works).
//!
//! Transport: plain HTTPS via the flagship relay face (FLEETD_RELAY_TOKEN).
//! Payload today carries seat metadata only (no task text) — the vault
//! distribution face (docs/vault-design.md) adds encrypted payloads later
//! over this same channel.

use anyhow::{Context, Result};
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

#[derive(Debug, Serialize)]
pub struct PullDispatch {
    pub id: String,
    pub task_text: String,
    pub repo: String,
}

const TABLE: &str = "
CREATE TABLE IF NOT EXISTS agent_seen (
    seat TEXT PRIMARY KEY,
    last_seen INTEGER NOT NULL,
    head TEXT,
    dirty INTEGER,
    pulls INTEGER NOT NULL DEFAULT 0
);
";

fn db() -> Result<rusqlite::Connection> {
    let c = crate::ledger::open()?;
    c.execute_batch(TABLE)
        .with_context(|| "create agent_seen table")?;
    Ok(c)
}

/// Handle one heartbeat: record liveness, hand back the pending dispatch.
pub fn heartbeat(raw: &str) -> Result<HeartbeatReply> {
    let hb: Heartbeat =
        serde_json::from_str(raw).with_context(|| "parse heartbeat JSON")?;
    if hb.seat.is_empty() || hb.seat.contains('/') {
        anyhow::bail!("invalid seat name");
    }
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
        rusqlite::params![hb.seat, now, hb.head, hb.dirty],
    )
    .with_context(|| "upsert agent_seen")?;
    Ok(HeartbeatReply {
        ok: true,
        dispatch: pending_pull(&c, &hb.seat)?,
    })
}

/// Claim the oldest pending pull-dispatch for a seat (FIFO). Claiming marks
/// it dispatched so a later heartbeat does not double-deliver.
fn pending_pull(c: &rusqlite::Connection, seat: &str) -> Result<Option<PullDispatch>> {
    let row = c
        .query_row(
            "SELECT id, task_file, repo FROM tasks
             WHERE host = ?1 AND state = 'pending-pull'
             ORDER BY rowid LIMIT 1",
            [seat],
            // tasks.id is a TEXT key (pull-<nanos>-…), not an integer.
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .ok();
    if let Some((id, task_file, repo)) = row {
        let text = std::fs::read_to_string(&task_file).unwrap_or_default();
        c.execute(
            "UPDATE tasks SET state='dispatched' WHERE id=?1",
            [&id],
        )
        .ok();
        c.execute(
            "UPDATE agent_seen SET pulls = pulls + 1 WHERE seat = ?1",
            [seat],
        )
        .ok();
        return Ok(Some(PullDispatch {
            id,
            task_text: text,
            repo,
        }));
    }
    Ok(None)
}

/// Queue a pull-dispatch for a ship (flagship side: `fleetd relay dispatch`).
pub fn queue_pull(seat: &str, task_file: &str, repo: &str) -> Result<i64> {
    let c = db()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    c.execute(
        "INSERT INTO tasks(id,host,repo,task_file,state,created_at)
         VALUES(?1,?2,?3,?4,'pending-pull',?5)",
        rusqlite::params![format!("pull-{}-{now}", uuid_lite()), seat, repo, task_file, now],
    )
    .with_context(|| "queue pull task")?;
    Ok(c.last_insert_rowid())
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

/// Tiny unique suffix without a uuid dependency.
fn uuid_lite() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_records_and_replies_empty() {
        let reply = heartbeat(r#"{"seat":"cuihaitao","head":"abc","dirty":2}"#).unwrap();
        assert!(reply.ok);
        assert!(reply.dispatch.is_none());
        let seen = alive_since(5).unwrap();
        assert!(seen.iter().any(|(s, _)| s == "cuihaitao"));
    }
}
