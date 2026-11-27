//! store: persistence layer. The ledger is the source of truth for seats,
//! dispatches, events and jobs. Also hosts the IO half of dispatching
//! (`dispatch_io`) so fleetd_core::dispatcher stays a pure decision function.

pub mod dispatch_io;
pub mod ledger;
pub mod resources;
