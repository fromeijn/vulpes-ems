use serde::{Deserialize, Serialize};

/// What a user-defined ESS mode physically does. The optimizer reasons about
/// these behaviours; the HA integration maps each mode to concrete actions.
/// Powers are AC-side in W; grid power is positive when importing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Behaviour {
    /// Hold grid power at `w` (e.g. Victron ESS setpoint). The battery takes
    /// whatever balances it: `battery = w + pv − load`. Positive `w` charges
    /// from the grid, negative `w` exports from the battery.
    GridSetpoint { w: f64 },
    /// Hold grid power at 0: the battery covers deficits and absorbs surplus.
    SelfConsume,
    /// Never discharge; PV surplus may still charge the battery.
    NoDischarge,
    /// Battery idle; the grid covers deficits and takes surplus.
    Idle,
    /// Fixed battery power (positive = charge), for inverters controlled that way.
    BatteryPower { w: f64 },
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
