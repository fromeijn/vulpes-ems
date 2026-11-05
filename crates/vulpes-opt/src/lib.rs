//! ESS mode optimizer: slot physics shared with the replay simulator, and a
//! dynamic-programming planner over (slot × state of charge × mode).

pub mod dp;
pub mod physics;

pub use dp::{Plan, PlanSlot, Problem, SlotInput, optimize};
pub use physics::{SlotOutcome, step};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
