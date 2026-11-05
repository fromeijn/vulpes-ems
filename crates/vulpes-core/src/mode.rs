use serde::{Deserialize, Serialize};

/// What a user-defined ESS mode physically does. The optimizer reasons about
/// these behaviours; the HA integration maps each mode to concrete actions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Behaviour {
    /// Charge the battery at up to `max_w` (from PV and/or grid).
    ForceCharge { max_w: f64 },
    /// Battery idle; grid covers any deficit, PV surplus is exported.
    Idle,
    /// Battery covers deficits / absorbs surplus to keep grid power near zero.
    SelfConsume,
    /// Discharge the battery at up to `max_w`, exporting what the house does not use.
    ForceDischarge { max_w: f64 },
}

/// A named mode as the user knows it (e.g. "Net Zero").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeDef {
    pub name: String,
    pub behaviour: Behaviour,
    /// Whether this mode is combined with PV curtailment.
    #[serde(default)]
    pub pv_curtail: bool,
}
