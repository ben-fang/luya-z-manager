//! The load ladder: what this place holds up under fixed loads (docs/designs/load-ladder.md).
//!
//! `judge` turns measurements into findings and nothing else; `engine` makes the measurements.
mod bucket;
mod engine;
mod judge;
mod probe;
pub(crate) mod stop;

pub use engine::{
    pick_nodes, public_nodes, run, Event, EventKind, Node, Plan, Progress, Report, Stall,
    StepKind, StepReport, Watch,
};
pub use judge::{Findings, StepStats};
pub use stop::Stopper;
