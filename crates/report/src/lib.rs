//! report: business-facing aggregation and views (read-only).
//! summary = ledger aggregation for dashboards (`GET /v1/fleet/summary`).
//! fleet_view = remote ANSI fleet overview (`fleetd fleet <flagship-url>`).

pub mod fleet_view;
pub mod summary;
