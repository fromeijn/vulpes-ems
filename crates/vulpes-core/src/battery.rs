use serde::{Deserialize, Serialize};

/// Static battery parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryParams {
    pub capacity_wh: f64,
    pub max_charge_w: f64,
    pub max_discharge_w: f64,
    pub charge_eff: f64,
    pub discharge_eff: f64,
    /// Minimum and maximum state of charge as fractions (0..=1).
    pub min_soc: f64,
    pub max_soc: f64,
    /// Wear cost per kWh of throughput (charged + discharged), in currency units.
    pub wear_cost_per_kwh: f64,
}

impl BatteryParams {
    pub fn validate(&self) -> crate::Result<()> {
        // Written so that NaN fails every check (comparisons with NaN are false).
        let frac = |x: f64| (0.0..=1.0).contains(&x);
        let eff = |x: f64| x > 0.0 && x <= 1.0;
        let non_negative = |x: f64| x >= 0.0 && x.is_finite();
        let ok = self.capacity_wh > 0.0
            && self.capacity_wh.is_finite()
            && non_negative(self.max_charge_w)
            && non_negative(self.max_discharge_w)
            && eff(self.charge_eff)
            && eff(self.discharge_eff)
            && frac(self.min_soc)
            && frac(self.max_soc)
            && self.min_soc < self.max_soc
            && non_negative(self.wear_cost_per_kwh);
        if !ok {
            return Err(crate::Error::Config(format!(
                "invalid battery parameters: {self:?}"
            )));
        }
        Ok(())
    }
}

/// Grid connection limits (W).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GridLimits {
    pub max_import_w: f64,
    pub max_export_w: f64,
}

impl GridLimits {
    pub fn validate(&self) -> crate::Result<()> {
        // Written so that NaN fails. Zero is valid: a connection that may not export.
        let ok = |x: f64| x >= 0.0 && x.is_finite();
        if !(ok(self.max_import_w) && ok(self.max_export_w)) {
            return Err(crate::Error::Config(format!("invalid grid limits: {self:?}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> BatteryParams {
        BatteryParams {
            capacity_wh: 10_000.0,
            max_charge_w: 3000.0,
            max_discharge_w: 3000.0,
            charge_eff: 0.95,
            discharge_eff: 0.95,
            min_soc: 0.1,
            max_soc: 1.0,
            wear_cost_per_kwh: 0.02,
        }
    }

    #[test]
    fn accepts_valid_and_boundary_values() {
        assert!(good().validate().is_ok());
        let edge = BatteryParams {
            charge_eff: 1.0,
            discharge_eff: 1.0,
            min_soc: 0.0,
            max_soc: 1.0,
            max_charge_w: 0.0,
            wear_cost_per_kwh: 0.0,
            ..good()
        };
        assert!(edge.validate().is_ok());
    }

    #[test]
    fn rejects_each_invalid_field() {
        let cases: Vec<(&str, BatteryParams)> = vec![
            (
                "zero capacity",
                BatteryParams {
                    capacity_wh: 0.0,
                    ..good()
                },
            ),
            (
                "negative capacity",
                BatteryParams {
                    capacity_wh: -1.0,
                    ..good()
                },
            ),
            (
                "NaN capacity",
                BatteryParams {
                    capacity_wh: f64::NAN,
                    ..good()
                },
            ),
            (
                "infinite capacity",
                BatteryParams {
                    capacity_wh: f64::INFINITY,
                    ..good()
                },
            ),
            (
                "negative charge power",
                BatteryParams {
                    max_charge_w: -1.0,
                    ..good()
                },
            ),
            (
                "NaN discharge power",
                BatteryParams {
                    max_discharge_w: f64::NAN,
                    ..good()
                },
            ),
            (
                "zero charge eff",
                BatteryParams {
                    charge_eff: 0.0,
                    ..good()
                },
            ),
            (
                "charge eff > 1",
                BatteryParams {
                    charge_eff: 1.01,
                    ..good()
                },
            ),
            (
                "NaN discharge eff",
                BatteryParams {
                    discharge_eff: f64::NAN,
                    ..good()
                },
            ),
            (
                "min soc < 0",
                BatteryParams {
                    min_soc: -0.1,
                    ..good()
                },
            ),
            (
                "max soc > 1",
                BatteryParams {
                    max_soc: 1.1,
                    ..good()
                },
            ),
            (
                "min == max",
                BatteryParams {
                    min_soc: 0.5,
                    max_soc: 0.5,
                    ..good()
                },
            ),
            (
                "min > max",
                BatteryParams {
                    min_soc: 0.9,
                    max_soc: 0.2,
                    ..good()
                },
            ),
            (
                "negative wear",
                BatteryParams {
                    wear_cost_per_kwh: -0.01,
                    ..good()
                },
            ),
        ];
        for (name, p) in cases {
            assert!(
                matches!(p.validate(), Err(crate::Error::Config(_))),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn grid_limits_must_be_finite_and_not_negative() {
        let limits = |i, e| GridLimits {
            max_import_w: i,
            max_export_w: e,
        };
        assert!(limits(17_000.0, 5000.0).validate().is_ok());
        assert!(limits(0.0, 0.0).validate().is_ok());
        for (name, l) in [
            ("negative import", limits(-1.0, 5000.0)),
            ("negative export", limits(17_000.0, -1.0)),
            ("NaN import", limits(f64::NAN, 5000.0)),
            ("NaN export", limits(17_000.0, f64::NAN)),
            ("infinite import", limits(f64::INFINITY, 5000.0)),
            ("infinite export", limits(17_000.0, f64::INFINITY)),
        ] {
            assert!(
                matches!(l.validate(), Err(crate::Error::Config(_))),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn serde_round_trip() {
        let json = serde_json::to_string(&good()).unwrap();
        assert_eq!(serde_json::from_str::<BatteryParams>(&json).unwrap(), good());
        let limits = GridLimits {
            max_import_w: 17_000.0,
            max_export_w: 5000.0,
        };
        let back: GridLimits = serde_json::from_str(&serde_json::to_string(&limits).unwrap()).unwrap();
        assert_eq!(back, limits);
    }

    #[test]
    fn missing_field_is_an_error() {
        let r = serde_json::from_str::<BatteryParams>(r#"{"capacity_wh": 1000}"#);
        assert!(r.is_err());
    }
}
