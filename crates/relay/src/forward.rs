//! 转发面:pull 任务队列与待投递取件(flagship 侧投递、ship 侧心跳领走)。
//! 2026-11-27 从 relay.rs 按职责拆出(注册/汇总/转发三面)。

use anyhow::{Context, Result};
use register::registry::db;
use rusqlite::Connection;

/// Pending pull-dispatch handed back on the next heartbeat.
#[derive(Debug, serde::Serialize)]
pub struct PullDispatch {
    pub id: String,
    pub task_text: String,
    pub repo: String,
}

/// Oldest pending pull task for `seat`, if any (consumed on handout).
pub(crate) fn pending_pull(c: &Connection, seat: &str) -> Result<Option<PullDispatch>> {
    let mut stmt = c.prepare(
        "SELECT id, task_file, repo FROM tasks
         WHERE host = ?1 AND state = 'pending-pull'
         ORDER BY created_at LIMIT 1",
    )?;
    let mut rows = stmt.query_map([seat], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    if let Some(row) = rows.next() {
        let (id, task_file, repo) = row?;
        drop(rows);
        let text = std::fs::read_to_string(&task_file).unwrap_or_default();
        c.execute("UPDATE tasks SET state = 'dispatched' WHERE id = ?1", [&id])
            .ok();
        let _ = register::bump_pulls(seat);
        return Ok(Some(PullDispatch {
            id,
            task_text: text,
            repo,
        }));
    }
    Ok(None)
}

/// Tiny unique suffix without a uuid dependency.
pub(crate) fn uuid_lite() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{n:x}")
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
        rusqlite::params![
            format!("pull-{}-{now}", uuid_lite()),
            seat,
            repo,
            task_file,
            now
        ],
    )
    .with_context(|| "queue pull task")?;
    Ok(c.last_insert_rowid())
}
