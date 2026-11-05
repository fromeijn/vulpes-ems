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
        let frac = |x: f64| (0.0..=1.0).contains(&x);
        if self.capacity_wh <= 0.0
            || !frac(self.min_soc)
            || !frac(self.max_soc)
            || self.min_soc >= self.max_soc
            || !(0.0 < self.charge_eff && self.charge_eff <= 1.0)
            || !(0.0 < self.discharge_eff && self.discharge_eff <= 1.0)
        {
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
