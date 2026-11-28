//! MCP server: exposes fleetd's control plane to coding agents (claude/codex/...).
//!
//! Built on rmcp (fleet-standard MCP SDK — no hand-rolled JSON-RPC, per global rule).
//! Mirrors the shape meshlet's agent MCP already compiles with locally:
//! `#[tool_handler(router=...)]` + `#[tool_router(router=tool_router)]` + `#[tool(...)]`
//! with `Json<T>` parameters and plain `String` returns.
//! Read-mostly surface: agents inspect dispatch status, seats, job stats; job
//! registration replaces hand-editing crontab. Task dispatching stays with the
//! control plane — agents never fire tasks at each other.
//!
//! Run with `fleetd mcp` (stdio transport); register with:
//!   claude mcp add fleetd -- /home/cui/workspaces/fleetd/target/release/fleetd mcp

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;

/// Tools exposed to agents.
#[derive(Debug, Clone)]
pub struct FleetMcp {
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, serde::Serialize, rmcp::schemars::JsonSchema)]
pub struct SeatQuery {
    /// Seat name; omit to list every seat with recent activity.
    #[serde(default)]
    pub seat: Option<String>,
}

#[derive(Deserialize, serde::Serialize, rmcp::schemars::JsonSchema)]
pub struct JobAddQuery {
    /// Unique job name.
    pub name: String,
    /// Schedule: cron 5-field ("*/5 * * * *") or "every 15m".
    pub schedule: String,
    /// Shell command to run.
    pub command: String,
}

#[derive(Deserialize, serde::Serialize, rmcp::schemars::JsonSchema)]
pub struct JobNameQuery {
    /// Job name (exact). Omit for all jobs.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, serde::Serialize, rmcp::schemars::JsonSchema)]
pub struct MemoryQuery {
    /// Filter by owning seat; omit to include every seat plus global notes.
    #[serde(default)]
    pub seat: Option<String>,
    /// Filter by tag (blocker / decision / credgap / ...).
    #[serde(default)]
    pub tag: Option<String>,
    /// Max rows (default 20).
    #[serde(default)]
    pub limit: Option<u32>,
}

impl FleetMcp {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for FleetMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FleetMcp {}

#[tool_router(router = tool_router)]
impl FleetMcp {
    /// Seat dispatch status: recent (1d) fired/done/inflight/stalled/partial counts.
    #[tool(
        name = "seat_status",
        description = "Fleet seat status: recent dispatch counts and outcomes for a seat (omit name for all). Read-only."
    )]
    pub async fn seat_status(&self, Parameters(query): Parameters<SeatQuery>) -> String {
        let c = match store::ledger::open() {
            Ok(c) => c,
            Err(e) => return format!("ledger error: {e}"),
        };
        let mut out = String::new();
        let rows = match store::ledger::progress(&c, 1) {
            Ok(r) => r,
            Err(e) => return format!("progress error: {e}"),
        };
        for r in rows {
            if let Some(want) = &query.seat {
                if &r.seat != want {
                    continue;
                }
            }
            out.push_str(&format!(
                "{} fired={} done={} inflight={} stalled={} partial={}\n",
                r.seat, r.fired, r.completed, r.inflight, r.stalled, r.partial
            ));
        }
        if out.is_empty() {
            out.push_str("(no dispatch activity in the last day)\n");
        }
        out
    }

    /// List scheduled jobs with schedule, enabled flag and run counts.
    #[tool(
        name = "job_list",
        description = "List fleetd scheduled jobs with schedule and run counts. Read-only."
    )]
    pub async fn job_list(&self) -> String {
        let c = match store::ledger::open() {
            Ok(c) => c,
            Err(e) => return format!("ledger error: {e}"),
        };
        let mut st = match c.prepare(
            "SELECT j.name, j.schedule, j.enabled,
                    (SELECT COUNT(*) FROM job_runs r WHERE r.job_id = j.id),
                    j.command
             FROM jobs j ORDER BY j.name",
        ) {
            Ok(s) => s,
            Err(e) => return format!("prepare error: {e}"),
        };
        let rows: Vec<(String, String, i64, i64, String)> = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .and_then(|m| m.collect::<std::result::Result<Vec<_>, _>>())
            .unwrap_or_default();
        let mut out = String::from("name | schedule | enabled | runs | command\n");
        for (name, schedule, enabled, runs, command) in rows {
            out.push_str(&format!(
                "{name} | {schedule} | {} | {runs} | {command}\n",
                if enabled == 1 { "yes" } else { "no" }
            ));
        }
        out
    }

    /// Register a scheduled shell job (cron or interval), replacing crontab edits.
    #[tool(
        name = "job_add",
        description = "Register a scheduled shell job (cron 5-field like '*/5 * * * *', or interval like 'every 15m'). Use this INSTEAD of editing crontab: runs are tracked with statistics in the fleet ledger."
    )]
    pub async fn job_add(&self, Parameters(query): Parameters<JobAddQuery>) -> String {
        let parsed = match fleetd_core::schedule::parse_schedule(&query.schedule) {
            Ok(p) => p,
            Err(e) => return format!("bad schedule: {e}"),
        };
        let c = match store::ledger::open() {
            Ok(c) => c,
            Err(e) => return format!("ledger error: {e}"),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let display = match parsed {
            fleetd_core::schedule::Schedule::Cron(x) => x,
            fleetd_core::schedule::Schedule::Interval { minutes } => format!("every {minutes}m"),
        };
        match c.execute(
            "INSERT INTO jobs(name,schedule,command,enabled,created_at) VALUES(?1,?2,?3,1,?4)
             ON CONFLICT(name) DO UPDATE SET schedule=excluded.schedule, command=excluded.command",
            rusqlite::params![query.name, display, query.command, now],
        ) {
            Ok(_) => format!("job saved: {} ({})", query.name, display),
            Err(e) => format!("save error: {e}"),
        }
    }

    /// Run statistics for one job (or all jobs).
    #[tool(
        name = "job_stats",
        description = "Run statistics (runs / ok / avg seconds / last run time) for a scheduled job, or all jobs when name is omitted."
    )]
    pub async fn job_stats(&self, Parameters(query): Parameters<JobNameQuery>) -> String {
        let c = match store::ledger::open() {
            Ok(c) => c,
            Err(e) => return format!("ledger error: {e}"),
        };
        let mut st = match c.prepare(
            "SELECT j.name, COUNT(r.id), SUM(r.exit_code=0),
                    ROUND(AVG(r.finished-r.started),1), MAX(r.started)
             FROM jobs j JOIN job_runs r ON r.job_id=j.id
             WHERE (?1 IS NULL OR j.name=?1)
             GROUP BY j.name ORDER BY j.name",
        ) {
            Ok(s) => s,
            Err(e) => return format!("prepare error: {e}"),
        };
        let rows: Vec<(String, i64, i64, f64, i64)> = st
            .query_map(rusqlite::params![query.name], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .and_then(|m| m.collect::<std::result::Result<Vec<_>, _>>())
            .unwrap_or_default();
        let mut out = String::from("name | runs | ok | avg_sec | last\n");
        for (name, runs, ok, avg, last) in rows {
            let t = fleetd_core::schedule::ts(last)
                .format("%m-%d %H:%M")
                .to_string();
            out.push_str(&format!("{name} | {runs} | {ok} | {avg} | {t}\n"));
        }
        out
    }

    /// Remove a scheduled job (history of runs stays).
    #[tool(
        name = "job_remove",
        description = "Remove a scheduled job by name. Run history is kept."
    )]
    pub async fn job_remove(&self, Parameters(query): Parameters<JobNameQuery>) -> String {
        let c = match store::ledger::open() {
            Ok(c) => c,
            Err(e) => return format!("ledger error: {e}"),
        };
        match c.execute(
            "DELETE FROM jobs WHERE name=?1",
            rusqlite::params![query.name],
        ) {
            Ok(0) => format!("no such job: {:?}", query.name),
            Ok(_) => format!("job removed: {:?}", query.name),
            Err(e) => format!("remove error: {e}"),
        }
    }

    /// Query fleet-memory notes (docs/agent-support.md §4.2): convention-keyed
    /// notes any writer can attach, e.g. `seat:<name>:blocked`. Read-only —
    /// writes go through the CLI / PUT face.
    #[tool(
        name = "fleet_memory_query",
        description = "Query fleet-memory notes (convention keys like seat:<name>:blocked, tags like blocker/decision). Read-only; seat/tag/limit all optional."
    )]
    pub async fn fleet_memory_query(&self, Parameters(query): Parameters<MemoryQuery>) -> String {
        let c = match store::ledger::open() {
            Ok(c) => c,
            Err(e) => return format!("ledger error: {e}"),
        };
        let rows = match store::ledger::memory_list(
            &c,
            query.seat.as_deref().filter(|s| !s.is_empty()),
            query.tag.as_deref().filter(|s| !s.is_empty()),
            query.limit.unwrap_or(20).clamp(1, 500) as i64,
        ) {
            Ok(r) => r,
            Err(e) => return format!("memory query error: {e}"),
        };
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "key": r.key,
                    "seat": r.seat,
                    "tag": r.tag,
                    "content": r.content,
                    "source": r.source,
                    "updated_at": r.updated_at,
                })
            })
            .collect();
        serde_json::to_string(&arr).unwrap_or_else(|e| format!("serialize error: {e}"))
    }
}

/// Run the MCP server over stdio (blocks until the client disconnects).
/// Used by `fleetd mcp`.
pub async fn run_stdio() -> anyhow::Result<()> {
    let service = FleetMcp::new();
    let server = service.serve(rmcp::transport::io::stdio()).await?;
    server.waiting().await?;
    Ok(())
}
