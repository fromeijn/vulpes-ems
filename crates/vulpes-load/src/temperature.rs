//! Temperature-anomaly correction for a profile forecast.
//!
//! A similar-day profile already reflects the recent temperature regime; it
//! cannot anticipate a change (a heatwave or cold snap starting tomorrow).
//! This correction regresses the profile's past errors on how much the target
//! hour's cooling/heating demand differs from the reference days' average:
//!
//! `error ≈ β_c · ΔCDH + β_h · ΔHDH`, with cooling degree-hours
//! `CDH = max(0, T − cool_base)` and heating degree-hours `HDH = max(0, heat_base − T)`.

#[derive(Debug, Clone, Copy)]
pub struct DegreeBases {
    pub cool_base: f64,
    pub heat_base: f64,
}

impl Default for DegreeBases {
    fn default() -> Self {
        Self {
            cool_base: 18.0,
            heat_base: 12.0,
        }
    }
}

impl DegreeBases {
    pub fn cdh(&self, t: f64) -> f64 {
        (t - self.cool_base).max(0.0)
    }
    pub fn hdh(&self, t: f64) -> f64 {
        (self.heat_base - t).max(0.0)
    }
}

/// One training sample: profile error (actual − profile) and the degree-hour anomalies.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub error: f64,
    pub d_cdh: f64,
    pub d_hdh: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Correction {
    pub beta_cool: f64,
    pub beta_heat: f64,
}

impl Correction {
    pub const NONE: Self = Self {
        beta_cool: 0.0,
        beta_heat: 0.0,
    };

    pub fn apply(&self, d_cdh: f64, d_hdh: f64) -> f64 {
        self.beta_cool * d_cdh + self.beta_heat * d_hdh
    }

    /// Ridge least squares (no intercept: the profile is unbiased on average).
    /// Coefficients are clamped at ≥ 0: more cooling/heating demand never
    /// lowers load. Returns `NONE` without enough informative samples.
    pub fn fit(samples: &[Sample], ridge: f64) -> Self {
        let informative = samples
            .iter()
            .filter(|s| s.d_cdh != 0.0 || s.d_hdh != 0.0)
            .count();
        if informative < 48 {
            return Self::NONE;
        }
        let (mut a11, mut a12, mut a22, mut b1, mut b2) = (ridge, 0.0, ridge, 0.0, 0.0);
        for s in samples {
            a11 += s.d_cdh * s.d_cdh;
            a12 += s.d_cdh * s.d_hdh;
            a22 += s.d_hdh * s.d_hdh;
            b1 += s.d_cdh * s.error;
            b2 += s.d_hdh * s.error;
        }
        let det = a11 * a22 - a12 * a12;
        if det.abs() < 1e-12 {
            return Self::NONE;
        }
        Self {
            beta_cool: ((a22 * b1 - a12 * b2) / det).max(0.0),
            beta_heat: ((a11 * b2 - a12 * b1) / det).max(0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_cooling_slope() {
        let samples: Vec<Sample> = (0..200)
            .map(|i| {
                let d = (i % 10) as f64 - 2.0;
                Sample {
                    error: 60.0 * d + if i % 2 == 0 { 15.0 } else { -15.0 },
                    d_cdh: d,
                    d_hdh: 0.0,
                }
            })
            .collect();
        let c = Correction::fit(&samples, 1.0);
        assert!((c.beta_cool - 60.0).abs() < 2.0, "{c:?}");
        assert_eq!(c.beta_heat, 0.0);
    }

    #[test]
    fn needs_enough_data() {
        let few = [Sample {
            error: 10.0,
            d_cdh: 1.0,
            d_hdh: 0.0,
        }; 10];
        assert_eq!(Correction::fit(&few, 1.0), Correction::NONE);
    }

    #[test]
    fn degree_hours() {
        let b = DegreeBases::default();
        assert_eq!((b.cdh(25.0), b.cdh(18.0), b.cdh(5.0)), (7.0, 0.0, 0.0));
        assert_eq!((b.hdh(2.0), b.hdh(12.0), b.hdh(30.0)), (10.0, 0.0, 0.0));
        let c = Correction {
            beta_cool: 30.0,
            beta_heat: 8.0,
        };
        assert_eq!(c.apply(2.0, 0.5), 64.0);
        assert_eq!(Correction::NONE.apply(5.0, 5.0), 0.0);
    }

    #[test]
    fn coefficients_are_never_negative() {
        // Load *drops* when it gets warmer: the fit must not learn a negative slope.
        let samples: Vec<Sample> = (0..100)
            .map(|i| {
                let d = (i % 10) as f64 - 5.0;
                Sample {
                    error: -20.0 * d,
                    d_cdh: d,
                    d_hdh: 0.0,
                }
            })
            .collect();
        let c = Correction::fit(&samples, 1.0);
        assert_eq!(c.beta_cool, 0.0);
        assert!(c.beta_heat >= 0.0);
    }

    #[test]
    fn heating_slope_is_learned_independently() {
        let samples: Vec<Sample> = (0..200)
            .map(|i| {
                let (dc, dh) = (((i % 7) as f64 - 3.0).max(0.0), ((i % 5) as f64 - 2.0).max(0.0));
                Sample {
                    error: 40.0 * dc + 10.0 * dh,
                    d_cdh: dc,
                    d_hdh: dh,
                }
            })
            .collect();
        let c = Correction::fit(&samples, 1e-6);
        assert!(
            (c.beta_cool - 40.0).abs() < 0.01 && (c.beta_heat - 10.0).abs() < 0.01,
            "{c:?}"
        );
    }
}
