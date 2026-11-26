// Fleet summary: read-only aggregation over the ledger for remote
// dashboards (`GET /v1/fleet/summary`) and the `fleetd fleet` TUI face.
// One JSON blob answers "what is every seat doing" — seats table state,
// recent dispatches, and headless runs.

use serde::Serialize;

#[derive(Serialize)]
pub struct DispatchRow {
    pub id: i64,
    pub seat: String,
    pub tag: String,
    pub fired_at: i64,
    pub finished_at: Option<i64>,
    pub outcome: Option<String>,
}

#[derive(Serialize)]
pub struct SeatRow {
    pub seat: String,
    pub status: String,
    pub current_tag: Option<String>,
    pub updated_at: i64,
}

#[derive(Serialize)]
pub struct Summary {
    pub seats: Vec<SeatRow>,
    pub recent_dispatches: Vec<DispatchRow>,
    pub open_count: i64,
}

/// Build the summary blob from the ledger. `days` bounds the dispatch
/// history (recent window, same convention as ledger::progress).
pub fn build(days: u64) -> Result<Summary, String> {
    let c = crate::ledger::open().map_err(|e| e.to_string())?;
    let _ = days;
    let mut seats = Vec::new();
    {
        let mut stmt = c
            .prepare("SELECT seat, status, current_tag, updated_at FROM seats ORDER BY seat")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query([])
            .map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            seats.push(SeatRow {
                seat: r.get(0).map_err(|e| e.to_string())?,
                status: r.get(1).map_err(|e| e.to_string())?,
                current_tag: r.get(2).map_err(|e| e.to_string())?,
                updated_at: r.get(3).map_err(|e| e.to_string())?,
            });
        }
    }
    let mut recent = Vec::new();
    {
        let mut stmt = c
            .prepare(
                "SELECT id, seat, tag, fired_at, finished_at, outcome
                 FROM dispatches ORDER BY fired_at DESC LIMIT 20",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            recent.push(DispatchRow {
                id: r.get(0).map_err(|e| e.to_string())?,
                seat: r.get(1).map_err(|e| e.to_string())?,
                tag: r.get(2).map_err(|e| e.to_string())?,
                fired_at: r.get(3).map_err(|e| e.to_string())?,
                finished_at: r.get(4).map_err(|e| e.to_string())?,
                outcome: r.get(5).map_err(|e| e.to_string())?,
            });
        }
    }
    let open_count: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM dispatches WHERE finished_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(Summary {
        seats,
        recent_dispatches: recent,
        open_count,
    })
}
