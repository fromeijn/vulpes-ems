//! Consumer tariff as a linear function of the wholesale price:
//! `all_in = a · wholesale + b` (EUR/kWh). Typically `a` is VAT (e.g. 1.21)
//! and `b` is (energy tax + supplier markup) × VAT.
//!
//! The fit uses total cost per step, `cost = a · (E · wholesale) + b · E`,
//! which weights each step by its energy. Fitting the per-kWh price instead
//! would be dominated by low-consumption steps, where rounding in the
//! supplier's cost figures is amplified.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinearTariff {
    pub a: f64,
    pub b: f64,
}

impl LinearTariff {
    pub fn price(&self, wholesale: f64) -> f64 {
        self.a * wholesale + self.b
    }
}

/// One step: energy (kWh), wholesale price (EUR/kWh) and what was charged (EUR).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TariffSample {
    pub energy_kwh: f64,
    pub wholesale: f64,
    pub cost: f64,
}

/// Fit result with the energy-weighted RMS price error (EUR/kWh).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TariffFit {
    pub tariff: LinearTariff,
    pub rms_error: f64,
    pub samples: usize,
}

/// Least squares on `cost = a · E·w + b · E`. Steps with no energy carry no
/// information and are ignored. Returns `None` if the system is degenerate.
pub fn fit(samples: &[TariffSample]) -> Option<TariffFit> {
    let (mut sxx, mut sxe, mut see, mut sxc, mut sec, mut n) = (0.0, 0.0, 0.0, 0.0, 0.0, 0);
    for s in samples.iter().filter(|s| s.energy_kwh > 0.0) {
        let x = s.energy_kwh * s.wholesale;
        let e = s.energy_kwh;
        sxx += x * x;
        sxe += x * e;
        see += e * e;
        sxc += x * s.cost;
        sec += e * s.cost;
        n += 1;
    }
    let det = sxx * see - sxe * sxe;
    if n < 2 || det.abs() < 1e-12 {
        return None;
    }
    let tariff = LinearTariff {
        a: (see * sxc - sxe * sec) / det,
        b: (sxx * sec - sxe * sxc) / det,
    };
    // Energy-weighted RMS error of the implied per-kWh price.
    let (mut num, mut den) = (0.0, 0.0);
    for s in samples.iter().filter(|s| s.energy_kwh > 0.0) {
        let err = s.cost / s.energy_kwh - tariff.price(s.wholesale);
        num += s.energy_kwh * err * err;
        den += s.energy_kwh;
    }
    Some(TariffFit {
        tariff,
        rms_error: (num / den).sqrt(),
        samples: n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_tariff_despite_rounded_costs() {
        let truth = LinearTariff { a: 1.21, b: 0.135 };
        let samples: Vec<TariffSample> = (0..500)
            .map(|i| {
                let e = 0.05 + (i % 20) as f64 * 0.05;
                let w = -0.02 + (i % 37) as f64 * 0.01;
                // Supplier rounds hourly cost to whole cents.
                let cost = (truth.price(w) * e * 100.0).round() / 100.0;
                TariffSample {
                    energy_kwh: e,
                    wholesale: w,
                    cost,
                }
            })
            .collect();
        let f = fit(&samples).unwrap();
        assert!((f.tariff.a - 1.21).abs() < 0.02, "{f:?}");
        assert!((f.tariff.b - 0.135).abs() < 0.003, "{f:?}");
    }
}
