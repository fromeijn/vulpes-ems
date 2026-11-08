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
    /// Whether this mode is combined with PV curtailment (no PV at all).
    #[serde(default)]
    pub pv_curtail: bool,
    /// Whether export is blocked in this mode: PV surplus beyond the load and
    /// battery charge is curtailed (the inverter's zero-export setting).
    #[serde(default)]
    pub no_export: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The JSON shape is shared with the Python integration: keep it stable.
    #[test]
    fn behaviour_json_format() {
        let cases = [
            (
                Behaviour::GridSetpoint { w: 3000.0 },
                json!({"type": "grid_setpoint", "w": 3000.0}),
            ),
            (Behaviour::SelfConsume, json!({"type": "self_consume"})),
            (Behaviour::NoDischarge, json!({"type": "no_discharge"})),
            (Behaviour::Idle, json!({"type": "idle"})),
            (
                Behaviour::BatteryPower { w: -1500.0 },
                json!({"type": "battery_power", "w": -1500.0}),
            ),
        ];
        for (b, want) in cases {
            assert_eq!(serde_json::to_value(b).unwrap(), want);
            assert_eq!(serde_json::from_value::<Behaviour>(want).unwrap(), b);
        }
    }

    #[test]
    fn integer_power_is_accepted() {
        let b: Behaviour = serde_json::from_value(json!({"type": "grid_setpoint", "w": 3318})).unwrap();
        assert_eq!(b, Behaviour::GridSetpoint { w: 3318.0 });
    }

    #[test]
    fn unknown_or_incomplete_behaviour_is_rejected() {
        assert!(serde_json::from_value::<Behaviour>(json!({"type": "force_charge"})).is_err());
        assert!(serde_json::from_value::<Behaviour>(json!({"type": "grid_setpoint"})).is_err());
        assert!(serde_json::from_value::<Behaviour>(json!({"w": 10.0})).is_err());
    }

    #[test]
    fn mode_def_curtail_and_no_export_default_to_false() {
        let m: ModeDef =
            serde_json::from_value(json!({"name": "hold", "behaviour": {"type": "no_discharge"}})).unwrap();
        assert_eq!(
            m,
            ModeDef {
                name: "hold".into(),
                behaviour: Behaviour::NoDischarge,
                pv_curtail: false,
                no_export: false,
            }
        );
        let back: ModeDef = serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn mode_def_no_export_json_round_trip() {
        let want = json!({"name": "self_consume:no_export", "behaviour": {"type": "self_consume"},
                          "pv_curtail": false, "no_export": true});
        let m: ModeDef = serde_json::from_value(want.clone()).unwrap();
        assert!(m.no_export && !m.pv_curtail);
        assert_eq!(serde_json::to_value(&m).unwrap(), want);
    }
}
