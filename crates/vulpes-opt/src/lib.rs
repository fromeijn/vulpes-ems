//! Dynamic-programming optimizer that picks an ESS mode per time slot.
//! Implemented in phase 7; this crate currently only exposes its version.

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
