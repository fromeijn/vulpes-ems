//! House load from meter data: what the house consumed, excluding the battery.
//!
//! `load = grid_import − grid_export + pv − battery_net_in`, all as energy per
//! step (Wh). With hourly steps, mean power in W equals energy in Wh.

/// One step of meter data in Wh. `battery_net_in` is positive when charging.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Balance {
    pub grid_import: f64,
    pub grid_export: f64,
    pub pv: f64,
    pub battery_net_in: f64,
}

impl Balance {
    /// House load in Wh. Small negatives from sensor timing jitter are clipped to 0.
    pub fn load(&self) -> f64 {
        (self.grid_import - self.grid_export + self.pv - self.battery_net_in).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance() {
        let b = Balance {
            grid_import: 100.0,
            grid_export: 0.0,
            pv: 500.0,
            battery_net_in: 300.0,
        };
        assert_eq!(b.load(), 300.0);
        let jitter = Balance {
            grid_import: 0.0,
            grid_export: 50.0,
            pv: 0.0,
            battery_net_in: -20.0,
        };
        assert_eq!(jitter.load(), 0.0);
    }
}
