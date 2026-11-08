//! Fit a plane's orientation (azimuth, tilt) and DC capacity to measured
//! hourly AC power, using archived weather forecasts as input.
//!
//! Method: coarse grid over azimuth/tilt; for each geometry the capacity has a
//! closed-form least-squares solution (the model is linear in `pdc0` below the
//! inverter limit). A trimmed refit drops the worst hours, which are mostly
//! cloud timing errors in the weather input. The best geometry is refined on
//! a 1° grid.

use crate::model::{PlaneParams, Weather, plane_ac_power};
use crate::solpos::SunPosition;

/// Model input for one hour: four 15-minute sub-slots.
#[derive(Debug, Clone)]
pub struct HourInput {
    pub sun: [SunPosition; 4],
    pub weather: [Weather; 4],
    pub day_of_year: u32,
}

#[derive(Debug, Clone)]
pub struct FitResult {
    pub params: PlaneParams,
    /// RMSE (W) on the hours kept after trimming.
    pub rmse: f64,
    pub hours_used: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct FitOptions {
    /// Fraction of worst-fitting hours to drop in the trimmed refit.
    pub trim: f64,
    pub tilt_max: f64,
}

impl Default for FitOptions {
    fn default() -> Self {
        Self {
            trim: 0.2,
            tilt_max: 70.0,
        }
    }
}

/// Hourly mean AC power of `p` for each input hour.
pub fn simulate_hours(p: &PlaneParams, inputs: &[HourInput]) -> Vec<f64> {
    inputs
        .iter()
        .map(|h| {
            (0..4)
                .map(|k| plane_ac_power(p, h.sun[k], h.weather[k], h.day_of_year, 1.0))
                .sum::<f64>()
                / 4.0
        })
        .collect()
}

/// Least-squares `pdc0` and the SSE for a fixed geometry, over the hours
/// selected by `mask`. Hours near the inverter limit are excluded from the
/// closed-form scale, then the SSE is evaluated with clipping.
fn fit_scale(
    base: &PlaneParams,
    inputs: &[HourInput],
    actual: &[f64],
    mask: &[bool],
) -> (f64, f64, Vec<f64>) {
    let unit = PlaneParams {
        pdc0: 1000.0,
        ac_max: f64::INFINITY,
        ..base.clone()
    };
    let m = simulate_hours(&unit, inputs);
    let (mut num, mut den) = (0.0, 0.0);
    for i in 0..m.len() {
        if mask[i] && actual[i] < 0.9 * base.ac_max {
            num += actual[i] * m[i];
            den += m[i] * m[i];
        }
    }
    let pdc0 = if den > 0.0 { 1000.0 * num / den } else { 0.0 };
    let pred: Vec<f64> = m.iter().map(|&x| (x * pdc0 / 1000.0).min(base.ac_max)).collect();
    let sse = (0..m.len())
        .filter(|&i| mask[i])
        .map(|i| (pred[i] - actual[i]).powi(2))
        .sum();
    (pdc0, sse, pred)
}

fn evaluate(
    base: &PlaneParams,
    az: f64,
    tilt: f64,
    inputs: &[HourInput],
    actual: &[f64],
    trim: f64,
) -> (f64, f64, usize) {
    let geo = PlaneParams {
        azimuth: az,
        tilt,
        ..base.clone()
    };
    let all = vec![true; actual.len()];
    let (_, _, pred) = fit_scale(&geo, inputs, actual, &all);
    // Keep the best (1 - trim) fraction of hours by absolute residual.
    let mut resid: Vec<(f64, usize)> = pred
        .iter()
        .zip(actual)
        .enumerate()
        .map(|(i, (p, a))| ((p - a).abs(), i))
        .collect();
    resid.sort_by(|a, b| a.0.total_cmp(&b.0));
    let keep = ((1.0 - trim) * resid.len() as f64).round() as usize;
    let mut mask = vec![false; actual.len()];
    for &(_, i) in &resid[..keep] {
        mask[i] = true;
    }
    let (pdc0, sse, _) = fit_scale(&geo, inputs, actual, &mask);
    (pdc0, sse, keep)
}

/// Fit azimuth, tilt and `pdc0`; other fields of `base` are kept (notably `ac_max`).
pub fn fit_plane(base: &PlaneParams, inputs: &[HourInput], actual: &[f64], opt: FitOptions) -> FitResult {
    assert_eq!(inputs.len(), actual.len());
    let mut best = (f64::INFINITY, 0.0, 0.0, 0.0, 0);
    let search =
        |azs: &mut dyn Iterator<Item = f64>, tilts: &[f64], best: &mut (f64, f64, f64, f64, usize)| {
            for az in azs {
                for &tilt in tilts {
                    let (pdc0, sse, n) = evaluate(base, az.rem_euclid(360.0), tilt, inputs, actual, opt.trim);
                    if sse < best.0 {
                        *best = (sse, az.rem_euclid(360.0), tilt, pdc0, n);
                    }
                }
            }
        };
    let coarse_tilts: Vec<f64> = (0..=(opt.tilt_max / 5.0) as i32)
        .map(|t| t as f64 * 5.0)
        .collect();
    search(&mut (0..72).map(|a| a as f64 * 5.0), &coarse_tilts, &mut best);
    let (_, az0, tilt0, _, _) = best;
    let fine_tilts: Vec<f64> = (-5..=5)
        .map(|d| tilt0 + d as f64)
        .filter(|t| (0.0..=opt.tilt_max).contains(t))
        .collect();
    search(&mut (-5..=5).map(|d| az0 + d as f64), &fine_tilts, &mut best);
    let (sse, azimuth, tilt, pdc0, n) = best;
    FitResult {
        params: PlaneParams {
            azimuth,
            tilt,
            pdc0,
            ..base.clone()
        },
        rmse: (sse / n.max(1) as f64).sqrt(),
        hours_used: n,
    }
}

/// One panel in a geometry group: its own data, capacity and inverter limit,
/// sharing azimuth (plus `azimuth_offset`) and tilt with the group.
#[derive(Debug, Clone)]
pub struct GroupMember<'a> {
    pub inputs: &'a [HourInput],
    pub actual: &'a [f64],
    pub azimuth_offset: f64,
    pub base: PlaneParams,
}

/// Fit one shared azimuth/tilt for a group, with a separate `pdc0` per member.
/// Returns per-member results in input order.
pub fn fit_group(members: &[GroupMember<'_>], opt: FitOptions) -> Vec<FitResult> {
    let total = |az: f64, tilt: f64| -> (f64, Vec<(f64, f64, usize)>) {
        let per: Vec<_> = members
            .iter()
            .map(|m| {
                evaluate(
                    &m.base,
                    (az + m.azimuth_offset).rem_euclid(360.0),
                    tilt,
                    m.inputs,
                    m.actual,
                    opt.trim,
                )
            })
            .collect();
        (per.iter().map(|r| r.1).sum(), per)
    };
    let mut best = (f64::INFINITY, 0.0, 0.0, Vec::new());
    let consider = |az: f64, tilt: f64, best: &mut (f64, f64, f64, Vec<(f64, f64, usize)>)| {
        let (sse, per) = total(az.rem_euclid(360.0), tilt);
        if sse < best.0 {
            *best = (sse, az.rem_euclid(360.0), tilt, per);
        }
    };
    for a in 0..72 {
        for t in 0..=(opt.tilt_max / 5.0) as i32 {
            consider(a as f64 * 5.0, t as f64 * 5.0, &mut best);
        }
    }
    let (az0, tilt0) = (best.1, best.2);
    for da in -5..=5 {
        for dt in -5..=5 {
            let tilt = tilt0 + dt as f64;
            if (0.0..=opt.tilt_max).contains(&tilt) {
                consider(az0 + da as f64, tilt, &mut best);
            }
        }
    }
    let (_, az, tilt, per) = best;
    members
        .iter()
        .zip(per)
        .map(|(m, (pdc0, sse, n))| FitResult {
            params: PlaneParams {
                azimuth: (az + m.azimuth_offset).rem_euclid(360.0),
                tilt,
                pdc0,
                ..m.base.clone()
            },
            rmse: (sse / n.max(1) as f64).sqrt(),
            hours_used: n,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdata::{clear_hours, plane};

    fn no_trim() -> FitOptions {
        FitOptions {
            trim: 0.0,
            ..Default::default()
        }
    }

    /// Synthetic clear-sky data from a known plane must be recovered.
    #[test]
    fn recovers_known_plane() {
        let truth = plane(225.0, 35.0, 300.0, 290.0);
        let inputs = clear_hours(20);
        let actual = simulate_hours(&truth, &inputs);
        let base = plane(180.0, 30.0, 250.0, 290.0);
        let fit = fit_plane(&base, &inputs, &actual, no_trim());
        assert!(
            (fit.params.azimuth - 225.0).abs() <= 1.0,
            "azimuth {}",
            fit.params.azimuth
        );
        assert!((fit.params.tilt - 35.0).abs() <= 1.0, "tilt {}", fit.params.tilt);
        assert!((fit.params.pdc0 - 300.0).abs() < 3.0, "pdc0 {}", fit.params.pdc0);
        assert_eq!(fit.params.ac_max, 290.0, "other fields come from the base");
        assert_eq!(fit.hours_used, inputs.len());
        assert!(fit.rmse < 2.0, "rmse {}", fit.rmse);
    }

    #[test]
    fn tent_pair_shares_tilt_with_opposite_azimuths() {
        let inputs = clear_hours(20);
        let (east, west) = (plane(225.0, 35.0, 300.0, 290.0), plane(45.0, 35.0, 250.0, 290.0));
        let (a1, a2) = (simulate_hours(&east, &inputs), simulate_hours(&west, &inputs));
        let base = plane(180.0, 30.0, 250.0, 290.0);
        let members = [
            GroupMember {
                inputs: &inputs,
                actual: &a1,
                azimuth_offset: 0.0,
                base: base.clone(),
            },
            GroupMember {
                inputs: &inputs,
                actual: &a2,
                azimuth_offset: 180.0,
                base,
            },
        ];
        let r = fit_group(&members, no_trim());
        assert!((r[0].params.azimuth - 225.0).abs() <= 1.0 && (r[1].params.azimuth - 45.0).abs() <= 1.0);
        assert!((r[0].params.tilt - 35.0).abs() <= 1.0 && r[0].params.tilt == r[1].params.tilt);
        assert!(
            (r[1].params.pdc0 - 250.0).abs() < 3.0,
            "twin pdc0 {}",
            r[1].params.pdc0
        );
    }

    #[test]
    fn simulate_hours_is_the_mean_of_the_quarters() {
        let p = plane(180.0, 35.0, 300.0, 1000.0);
        let inputs = clear_hours(1);
        let h = &inputs[3];
        let quarters: f64 = (0..4)
            .map(|k| plane_ac_power(&p, h.sun[k], h.weather[k], h.day_of_year, 1.0))
            .sum();
        assert!((simulate_hours(&p, &inputs)[3] - quarters / 4.0).abs() < 1e-9);
        assert!(simulate_hours(&p, &[]).is_empty());
    }

    #[test]
    fn closed_form_scale_and_mask() {
        let truth = plane(180.0, 35.0, 420.0, 10_000.0);
        let inputs = clear_hours(3);
        let actual = simulate_hours(&truth, &inputs);
        let all = vec![true; actual.len()];
        let (pdc0, sse, _) = fit_scale(&truth, &inputs, &actual, &all);
        assert!((pdc0 - 420.0).abs() < 1e-6 && sse < 1e-6, "{pdc0} {sse}");
        let none = vec![false; actual.len()];
        let (pdc0, sse, _) = fit_scale(&truth, &inputs, &actual, &none);
        assert_eq!((pdc0, sse), (0.0, 0.0), "no hours selected");
    }

    #[test]
    fn clipped_hours_do_not_bias_the_capacity() {
        // The inverter clips at midday; the capacity must still come out right.
        let truth = plane(180.0, 35.0, 400.0, 250.0);
        let inputs = clear_hours(20);
        let actual = simulate_hours(&truth, &inputs);
        assert!(actual.iter().any(|&a| a >= 249.9), "test data should clip");
        let fit = fit_plane(&plane(180.0, 30.0, 300.0, 250.0), &inputs, &actual, no_trim());
        assert!((fit.params.pdc0 - 400.0).abs() < 5.0, "pdc0 {}", fit.params.pdc0);
    }

    #[test]
    fn trimming_ignores_cloud_timing_errors() {
        let truth = plane(200.0, 30.0, 300.0, 1000.0);
        let inputs = clear_hours(20);
        let mut actual = simulate_hours(&truth, &inputs);
        // Every 7th hour was cloudy although the weather input said clear.
        for a in actual.iter_mut().step_by(7) {
            *a *= 0.2;
        }
        let fit = fit_plane(
            &plane(180.0, 30.0, 250.0, 1000.0),
            &inputs,
            &actual,
            FitOptions::default(),
        );
        assert!((fit.params.pdc0 - 300.0).abs() < 10.0, "pdc0 {}", fit.params.pdc0);
        assert!(
            (fit.params.azimuth - 200.0).abs() <= 2.0,
            "azimuth {}",
            fit.params.azimuth
        );
        assert_eq!(fit.hours_used, (0.8 * inputs.len() as f64).round() as usize);
    }

    #[test]
    fn azimuth_near_north_wraps_around() {
        let truth = plane(2.0, 20.0, 300.0, 1000.0);
        let inputs = clear_hours(20);
        let actual = simulate_hours(&truth, &inputs);
        let fit = fit_plane(&plane(180.0, 30.0, 250.0, 1000.0), &inputs, &actual, no_trim());
        let az = fit.params.azimuth;
        assert!((0.0..360.0).contains(&az), "{az}");
        assert!(az <= 3.0 || az >= 359.0, "azimuth {az}");
    }

    #[test]
    fn tilt_is_bounded_by_the_option() {
        let truth = plane(180.0, 45.0, 300.0, 1000.0);
        let inputs = clear_hours(10);
        let actual = simulate_hours(&truth, &inputs);
        let opt = FitOptions {
            trim: 0.0,
            tilt_max: 20.0,
        };
        let fit = fit_plane(&plane(180.0, 10.0, 250.0, 1000.0), &inputs, &actual, opt);
        assert!(fit.params.tilt <= 20.0, "tilt {}", fit.params.tilt);
    }

    #[test]
    fn empty_input_does_not_panic() {
        let fit = fit_plane(
            &plane(180.0, 30.0, 250.0, 1000.0),
            &[],
            &[],
            FitOptions::default(),
        );
        assert_eq!((fit.hours_used, fit.params.pdc0), (0, 0.0));
    }

    #[test]
    #[should_panic]
    fn mismatched_lengths_panic() {
        fit_plane(
            &plane(180.0, 30.0, 250.0, 1000.0),
            &clear_hours(1),
            &[1.0],
            FitOptions::default(),
        );
    }
}
