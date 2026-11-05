//! Per-plane shading map: a beam (direct-sun) transmission factor per sun
//! position bin, learned from clear-sky hours. 1.0 = unshaded, 0.0 = the
//! direct sun is fully blocked (diffuse light still reaches the panel).

use crate::calibrate::HourInput;
use crate::model::{PlaneParams, plane_ac_power};
use crate::solpos::SunPosition;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadingMap {
    pub az_step: f64,
    pub el_step: f64,
    /// `factors[az_bin][el_bin]`.
    pub factors: Vec<Vec<f64>>,
}

impl ShadingMap {
    pub fn unshaded(az_step: f64, el_step: f64) -> Self {
        let (na, ne) = (
            (360.0 / az_step).ceil() as usize,
            (90.0 / el_step).ceil() as usize,
        );
        Self {
            az_step,
            el_step,
            factors: vec![vec![1.0; ne]; na],
        }
    }

    fn bin(&self, sun: SunPosition) -> Option<(usize, usize)> {
        let el = sun.elevation();
        if el <= 0.0 {
            return None;
        }
        let a = ((sun.azimuth.rem_euclid(360.0) / self.az_step) as usize).min(self.factors.len() - 1);
        let e = ((el / self.el_step) as usize).min(self.factors[0].len() - 1);
        Some((a, e))
    }

    pub fn factor(&self, sun: SunPosition) -> f64 {
        self.bin(sun).map_or(1.0, |(a, e)| self.factors[a][e])
    }
}

/// Hourly mean AC power with the shading map applied to the beam component.
pub fn simulate_hours_shaded(p: &PlaneParams, map: Option<&ShadingMap>, inputs: &[HourInput]) -> Vec<f64> {
    inputs
        .iter()
        .map(|h| {
            (0..4)
                .map(|k| {
                    let f = map.map_or(1.0, |m| m.factor(h.sun[k]));
                    plane_ac_power(p, h.sun[k], h.weather[k], h.day_of_year, f)
                })
                .sum::<f64>()
                / 4.0
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct ShadingOptions {
    pub az_step: f64,
    pub el_step: f64,
    /// Pseudo-observations pulling sparse bins toward 1.0 (unshaded).
    pub prior_hours: f64,
    /// Minimum modelled beam contribution (W) for an hour to be informative.
    pub min_beam_w: f64,
}

impl Default for ShadingOptions {
    fn default() -> Self {
        Self {
            az_step: 10.0,
            el_step: 5.0,
            prior_hours: 3.0,
            min_beam_w: 30.0,
        }
    }
}

/// Learn a shading map from (mostly clear) hours. Per hour the measured power
/// is split as `actual = diffuse_part + f · beam_part`; per bin, `f` is the
/// median over hours (robust against cloud timing errors in the weather
/// input), shrunk toward 1.0 when the bin has few hours. Hours near the
/// inverter limit carry no information about shading and are skipped.
pub fn fit_shading(p: &PlaneParams, inputs: &[HourInput], actual: &[f64], opt: ShadingOptions) -> ShadingMap {
    let mut map = ShadingMap::unshaded(opt.az_step, opt.el_step);
    let mut ratios: Vec<Vec<Vec<f64>>> = vec![vec![Vec::new(); map.factors[0].len()]; map.factors.len()];
    let unclipped = PlaneParams {
        ac_max: f64::INFINITY,
        ..p.clone()
    };
    for (h, &a) in inputs.iter().zip(actual) {
        if a > 0.97 * p.ac_max {
            continue;
        }
        let (mut full, mut no_beam) = (0.0, 0.0);
        for k in 0..4 {
            full += plane_ac_power(&unclipped, h.sun[k], h.weather[k], h.day_of_year, 1.0) / 4.0;
            no_beam += plane_ac_power(&unclipped, h.sun[k], h.weather[k], h.day_of_year, 0.0) / 4.0;
        }
        let beam = full - no_beam;
        // Sun position at the middle of the hour.
        let mid = SunPosition {
            zenith: 0.5 * (h.sun[1].zenith + h.sun[2].zenith),
            azimuth: 0.5 * (h.sun[1].azimuth + h.sun[2].azimuth),
        };
        if let (true, Some((ai, ei))) = (beam >= opt.min_beam_w, map.bin(mid)) {
            ratios[ai][ei].push(((a - no_beam) / beam).clamp(0.0, 1.5));
        }
    }
    for (ai, row) in ratios.iter_mut().enumerate() {
        for (ei, r) in row.iter_mut().enumerate() {
            if r.is_empty() {
                continue;
            }
            r.sort_by(f64::total_cmp);
            let median = r[r.len() / 2];
            let n = r.len() as f64;
            map.factors[ai][ei] = ((n * median + opt.prior_hours) / (n + opt.prior_hours)).clamp(0.0, 1.05);
        }
    }
    map
}

/// Least-squares `pdc0` with the shading map applied, using the best
/// `(1 - trim)` fraction of hours and skipping hours near the inverter limit.
pub fn refit_pdc0(p: &PlaneParams, map: &ShadingMap, inputs: &[HourInput], actual: &[f64], trim: f64) -> f64 {
    let unit = PlaneParams {
        pdc0: 1000.0,
        ac_max: f64::INFINITY,
        ..p.clone()
    };
    let m = simulate_hours_shaded(&unit, Some(map), inputs);
    let scale = |keep: &dyn Fn(usize) -> bool| {
        let (mut num, mut den) = (0.0, 0.0);
        for i in (0..m.len()).filter(|&i| keep(i) && actual[i] < 0.9 * p.ac_max) {
            num += actual[i] * m[i];
            den += m[i] * m[i];
        }
        if den > 0.0 { num / den } else { p.pdc0 / 1000.0 }
    };
    let s0 = scale(&|_| true);
    let mut resid: Vec<(f64, usize)> = (0..m.len())
        .map(|i| (((m[i] * s0).min(p.ac_max) - actual[i]).abs(), i))
        .collect();
    resid.sort_by(|a, b| a.0.total_cmp(&b.0));
    let keep: std::collections::HashSet<usize> = resid
        [..((1.0 - trim) * resid.len() as f64).round() as usize]
        .iter()
        .map(|r| r.1)
        .collect();
    1000.0 * scale(&|i| keep.contains(&i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Weather;
    use crate::solpos::sun_position;

    #[test]
    fn learns_an_obstruction_in_the_afternoon() {
        let p = PlaneParams {
            azimuth: 180.0,
            tilt: 35.0,
            pdc0: 300.0,
            ac_max: 1000.0,
            inverter_eff: 0.965,
            gamma: -0.004,
            iam_b0: 0.05,
            albedo: 0.2,
            u0: 25.0,
            u1: 6.84,
        };
        // Truth: the direct sun is blocked when the sun is between 230° and 260°.
        let mut truth = ShadingMap::unshaded(10.0, 5.0);
        for row in &mut truth.factors[23..26] {
            row.iter_mut().for_each(|f| *f = 0.0);
        }
        let start = 1_750_000_000_i64 - 1_750_000_000 % 3600;
        let mut inputs = Vec::new();
        for hour in 0..(24 * 30) {
            let t0 = start + hour * 3600;
            let mut sun = [SunPosition {
                zenith: 0.0,
                azimuth: 0.0,
            }; 4];
            let mut weather = [Weather {
                ghi: 0.0,
                dni: 0.0,
                dhi: 0.0,
                temp_air: 20.0,
                wind_speed: 2.0,
            }; 4];
            for k in 0..4 {
                let s = sun_position((t0 + k as i64 * 900 + 450) as f64, 52.0, 5.0);
                let cz = s.zenith.to_radians().cos().max(0.0);
                let dni = if cz > 0.0 {
                    850.0 * (-0.15 / cz.max(0.05)).exp()
                } else {
                    0.0
                };
                sun[k] = s;
                weather[k] = Weather {
                    ghi: dni * cz + 90.0 * cz,
                    dni,
                    dhi: 90.0 * cz,
                    temp_air: 20.0,
                    wind_speed: 2.0,
                };
            }
            if sun[2].zenith < 80.0 {
                inputs.push(HourInput {
                    sun,
                    weather,
                    day_of_year: 170,
                });
            }
        }
        let actual = simulate_hours_shaded(&p, Some(&truth), &inputs);
        let learned = fit_shading(&p, &inputs, &actual, ShadingOptions::default());
        let at = |az: f64, el: f64| {
            learned.factor(SunPosition {
                zenith: 90.0 - el,
                azimuth: az,
            })
        };
        // Unshaded morning stays near 1; the blocked afternoon sector is learned as mostly shaded.
        assert!(at(135.0, 40.0) > 0.95, "morning {}", at(135.0, 40.0));
        // Bins in the blocked sector that received data are learned as mostly shaded.
        let shaded: Vec<f64> = (23..26)
            .flat_map(|a| learned.factors[a].clone())
            .filter(|&f| f < 1.0)
            .collect();
        assert!(
            !shaded.is_empty() && shaded.iter().all(|&f| f < 0.6),
            "afternoon {shaded:?}"
        );
    }
}
