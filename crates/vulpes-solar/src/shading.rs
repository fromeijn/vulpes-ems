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
    use crate::calibrate::simulate_hours;
    use crate::testdata::{clear_hours, plane};

    fn sun(az: f64, el: f64) -> SunPosition {
        SunPosition {
            zenith: 90.0 - el,
            azimuth: az,
        }
    }

    /// Truth: the direct sun is blocked when the sun is between 230° and 260°.
    fn afternoon_obstruction() -> ShadingMap {
        let mut truth = ShadingMap::unshaded(10.0, 5.0);
        for row in &mut truth.factors[23..26] {
            row.iter_mut().for_each(|f| *f = 0.0);
        }
        truth
    }

    #[test]
    fn map_dimensions_and_lookup() {
        let m = ShadingMap::unshaded(10.0, 5.0);
        assert_eq!((m.factors.len(), m.factors[0].len()), (36, 18));
        let odd = ShadingMap::unshaded(7.0, 4.0);
        assert_eq!(
            (odd.factors.len(), odd.factors[0].len()),
            (52, 23),
            "partial bins round up"
        );
        let mut m = m;
        m.factors[23][8] = 0.25;
        assert_eq!(m.factor(sun(235.0, 42.0)), 0.25);
        assert_eq!(m.factor(sun(235.0 + 360.0, 42.0)), 0.25, "azimuth wraps");
        assert_eq!(m.factor(sun(235.0, -5.0)), 1.0, "sun down: no shading");
        assert_eq!(m.factor(sun(359.99, 89.9)), 1.0, "last bins are valid");
        assert_eq!(m.factor(sun(0.0, 90.0)), 1.0, "zenith clamps into the top bin");
    }

    #[test]
    fn simulation_without_or_with_an_empty_map_is_unshaded() {
        let p = plane(180.0, 35.0, 300.0, 1000.0);
        let inputs = clear_hours(2);
        let plain = simulate_hours(&p, &inputs);
        assert_eq!(simulate_hours_shaded(&p, None, &inputs), plain);
        assert_eq!(
            simulate_hours_shaded(&p, Some(&ShadingMap::unshaded(10.0, 5.0)), &inputs),
            plain
        );
        let shaded = simulate_hours_shaded(&p, Some(&afternoon_obstruction()), &inputs);
        assert!(shaded.iter().zip(&plain).all(|(s, u)| s <= u) && shaded != plain);
    }

    #[test]
    fn learns_an_obstruction_in_the_afternoon() {
        let p = plane(180.0, 35.0, 300.0, 1000.0);
        let inputs = clear_hours(30);
        let actual = simulate_hours_shaded(&p, Some(&afternoon_obstruction()), &inputs);
        let learned = fit_shading(&p, &inputs, &actual, ShadingOptions::default());
        // Unshaded morning stays near 1; bins in the blocked sector that got data are mostly shaded.
        assert!(learned.factor(sun(135.0, 40.0)) > 0.95);
        let shaded: Vec<f64> = (23..26)
            .flat_map(|a| learned.factors[a].clone())
            .filter(|&f| f < 1.0)
            .collect();
        assert!(
            !shaded.is_empty() && shaded.iter().all(|&f| f < 0.6),
            "afternoon {shaded:?}"
        );
    }

    #[test]
    fn sparse_bins_are_pulled_toward_unshaded() {
        let p = plane(180.0, 35.0, 300.0, 1000.0);
        let inputs = clear_hours(30);
        // Pick one informative hour and make it fully shaded: ratio 0 in its bin.
        let all = simulate_hours(&p, &inputs);
        let i = all
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        let one = &inputs[i..=i];
        let no_beam = simulate_hours_shaded(
            &p,
            Some(&ShadingMap {
                az_step: 360.0,
                el_step: 90.0,
                factors: vec![vec![0.0]],
            }),
            one,
        );
        let opt = ShadingOptions {
            prior_hours: 3.0,
            ..Default::default()
        };
        let learned = fit_shading(&p, one, &no_beam, opt);
        let changed: Vec<f64> = learned
            .factors
            .iter()
            .flatten()
            .copied()
            .filter(|&f| f != 1.0)
            .collect();
        // One observation of 0 against 3 prior hours at 1.0: (0 + 3) / (1 + 3).
        assert_eq!(changed.len(), 1);
        assert!((changed[0] - 0.75).abs() < 1e-9, "{changed:?}");
    }

    #[test]
    fn hours_near_the_inverter_limit_carry_no_shading_information() {
        let p = plane(180.0, 35.0, 600.0, 200.0);
        let inputs = clear_hours(10);
        // Everything clipped: nothing is learned.
        let actual = vec![200.0; inputs.len()];
        let learned = fit_shading(&p, &inputs, &actual, ShadingOptions::default());
        assert!(learned.factors.iter().flatten().all(|&f| f == 1.0));
    }

    #[test]
    fn refit_capacity_with_the_shading_map() {
        let truth = plane(180.0, 35.0, 330.0, 1000.0);
        let map = afternoon_obstruction();
        let inputs = clear_hours(20);
        let actual = simulate_hours_shaded(&truth, Some(&map), &inputs);
        let start = plane(180.0, 35.0, 250.0, 1000.0);
        let with_map = refit_pdc0(&start, &map, &inputs, &actual, 0.0);
        assert!((with_map - 330.0).abs() < 1.0, "{with_map}");
        // Ignoring the obstruction underestimates the capacity.
        let without = refit_pdc0(&start, &ShadingMap::unshaded(10.0, 5.0), &inputs, &actual, 0.0);
        assert!(without < with_map - 1.0, "{without} vs {with_map}");
        // No usable hours: keeps the starting capacity.
        assert_eq!(refit_pdc0(&start, &map, &[], &[], 0.2), 250.0);
    }
}
