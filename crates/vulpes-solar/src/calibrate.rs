//! Fit a plane's orientation (azimuth, tilt) and DC capacity to measured
//! hourly AC power, using archived weather forecasts as input.
//!
//! Method: coarse grid over azimuth/tilt; for each geometry the capacity has a
//! closed-form least-squares solution (the model is linear in `pdc0` below the
//! inverter limit). A trimmed refit drops the worst hours, which are mostly
//! cloud timing errors in the weather input. The best geometry is refined on
//! a 1° grid.

use crate::model::{PlaneParams, Weather, combine_ac, plane_ac_power, plane_dc_power};
use crate::solpos::SunPosition;
use serde::{Deserialize, Serialize};

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
    /// Drop only hours that produced less than modelled (iterated a few
    /// times), instead of the largest absolute residuals. For data in which
    /// output was held back: a hybrid inverter curtailing solar with the
    /// battery full and export blocked, an MPPT at its current limit, or
    /// clouds the forecast missed. Those only ever push power down, and a
    /// symmetric trim would fit the capacity too low.
    pub one_sided: bool,
}

impl Default for FitOptions {
    fn default() -> Self {
        Self {
            trim: 0.2,
            tilt_max: 70.0,
            one_sided: false,
        }
    }
}

impl FitOptions {
    /// Settings for calibrating from a home's own history (string or total
    /// power), where curtailment and limits are common.
    pub fn robust() -> Self {
        Self {
            trim: 0.3,
            one_sided: true,
            ..Self::default()
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

/// Hourly mean AC power of a whole system (shared inverters clip together).
pub fn simulate_system_hours(planes: &[PlaneParams], inputs: &[HourInput]) -> Vec<f64> {
    inputs
        .iter()
        .map(|h| {
            (0..4)
                .map(|k| {
                    let dc: Vec<f64> = planes
                        .iter()
                        .map(|p| plane_dc_power(p, h.sun[k], h.weather[k], h.day_of_year, 1.0))
                        .collect();
                    combine_ac(planes, &dc)
                })
                .sum::<f64>()
                / 4.0
        })
        .collect()
}

/// Total AC capacity (W): each separate inverter once, each shared inverter once.
pub fn ac_capacity(planes: &[PlaneParams]) -> f64 {
    let mut seen = Vec::new();
    let mut total = 0.0;
    for p in planes {
        match p.inverter {
            Some(id) if seen.contains(&id) => {}
            Some(id) => {
                seen.push(id);
                total += p.ac_max;
            }
            None => total += p.ac_max,
        }
    }
    total
}

/// One multiplier on every plane's `pdc0`, fitted to the measured total AC
/// power of the system. The fallback when planes have no measurement of their
/// own: the geometry is kept, only the size is corrected. Hours near the
/// inverter limits are skipped and the worst `trim` fraction is dropped in a
/// refit, as in [`fit_plane`]. Returns 1.0 when there is nothing to fit on.
pub fn fit_total_scale(planes: &[PlaneParams], inputs: &[HourInput], actual: &[f64], opt: FitOptions) -> f64 {
    assert_eq!(inputs.len(), actual.len());
    let unclipped: Vec<PlaneParams> = planes
        .iter()
        .map(|p| PlaneParams {
            ac_max: f64::INFINITY,
            inverter: None,
            ..p.clone()
        })
        .collect();
    let m = simulate_system_hours(&unclipped, inputs);
    let cap = ac_capacity(planes);
    let ok: Vec<bool> = (0..m.len())
        .map(|i| actual[i] < 0.9 * cap && m[i] < 0.9 * cap && m[i] > 0.0)
        .collect();
    let scale = |keep: &dyn Fn(usize) -> bool| {
        let (mut num, mut den) = (0.0, 0.0);
        for i in (0..m.len()).filter(|&i| ok[i] && keep(i)) {
            num += actual[i] * m[i];
            den += m[i] * m[i];
        }
        (den > 0.0).then(|| num / den)
    };
    let Some(mut s) = scale(&|_| true) else {
        return 1.0;
    };
    let idx: Vec<usize> = (0..m.len()).filter(|&i| ok[i]).collect();
    let keep = ((1.0 - opt.trim) * idx.len() as f64).round() as usize;
    let rounds = if opt.one_sided { ONE_SIDED_ROUNDS } else { 1 };
    for _ in 0..rounds {
        let pred: Vec<f64> = idx.iter().map(|&i| m[i] * s).collect();
        let act: Vec<f64> = idx.iter().map(|&i| actual[i]).collect();
        let mask = keep_mask(&pred, &act, keep, opt.one_sided);
        let kept: std::collections::HashSet<usize> =
            idx.iter().zip(&mask).filter(|x| *x.1).map(|x| *x.0).collect();
        s = scale(&|i| kept.contains(&i)).unwrap_or(s);
    }
    s
}

/// Forecast error statistics (W).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub hours: usize,
    pub mae_w: f64,
    pub rmse_w: f64,
    pub bias_w: f64,
}

/// Score `pred` against `actual` (forecast − actual), skipping pairs where both are zero (night).
pub fn score(pred: &[f64], actual: &[f64]) -> Score {
    let (mut n, mut abs, mut sq, mut bias) = (0usize, 0.0, 0.0, 0.0);
    for (&p, &a) in pred.iter().zip(actual) {
        if p <= 0.0 && a <= 0.0 {
            continue;
        }
        let e = p - a;
        n += 1;
        abs += e.abs();
        sq += e * e;
        bias += e;
    }
    let k = n.max(1) as f64;
    Score {
        hours: n,
        mae_w: abs / k,
        rmse_w: (sq / k).sqrt(),
        bias_w: bias / k,
    }
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

/// Mask keeping the best `keep` hours: smallest absolute residual, or with
/// `one_sided` the hours that produced most relative to the model.
fn keep_mask(pred: &[f64], actual: &[f64], keep: usize, one_sided: bool) -> Vec<bool> {
    let mut resid: Vec<(f64, usize)> = pred
        .iter()
        .zip(actual)
        .enumerate()
        .map(|(i, (p, a))| (if one_sided { p - a } else { (p - a).abs() }, i))
        .collect();
    resid.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut mask = vec![false; actual.len()];
    for &(_, i) in &resid[..keep] {
        mask[i] = true;
    }
    mask
}

/// Rounds of drop-and-refit for a one-sided trim: the first fit is pulled
/// down by the hours that will be dropped, so the selection is redone.
const ONE_SIDED_ROUNDS: usize = 3;

pub(crate) fn evaluate(
    base: &PlaneParams,
    az: f64,
    tilt: f64,
    inputs: &[HourInput],
    actual: &[f64],
    opt: FitOptions,
) -> (f64, f64, usize) {
    let geo = PlaneParams {
        azimuth: az,
        tilt,
        ..base.clone()
    };
    let all = vec![true; actual.len()];
    let (mut pdc0, mut sse, mut pred) = fit_scale(&geo, inputs, actual, &all);
    // Keep the best (1 - trim) fraction of hours.
    let keep = ((1.0 - opt.trim) * actual.len() as f64).round() as usize;
    let rounds = if opt.one_sided { ONE_SIDED_ROUNDS } else { 1 };
    for _ in 0..rounds {
        let mask = keep_mask(&pred, actual, keep, opt.one_sided);
        (pdc0, sse, pred) = fit_scale(&geo, inputs, actual, &mask);
    }
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
                    let (pdc0, sse, n) = evaluate(base, az.rem_euclid(360.0), tilt, inputs, actual, opt);
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
                    opt,
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
    fn one_sided_trim_ignores_curtailed_hours() {
        // A third of the hours were curtailed to 30% (battery full, no export).
        let truth = plane(200.0, 30.0, 4000.0, 1e9);
        let inputs = clear_hours(20);
        let mut actual = simulate_hours(&truth, &inputs);
        for a in actual.iter_mut().step_by(3) {
            *a *= 0.3;
        }
        let base = plane(180.0, 30.0, 3000.0, 1e9);
        let symmetric = fit_plane(&base, &inputs, &actual, FitOptions::default());
        let robust = fit_plane(&base, &inputs, &actual, FitOptions::robust());
        assert!(
            (robust.params.pdc0 - 4000.0).abs() < 60.0,
            "robust {}",
            robust.params.pdc0
        );
        let err = |r: &FitResult| (r.params.pdc0 - 4000.0).abs() + 20.0 * (r.params.azimuth - 200.0).abs();
        assert!(
            err(&robust) < err(&symmetric),
            "{:?} vs {:?}",
            robust.params,
            symmetric.params
        );
        assert!(
            (robust.params.azimuth - 200.0).abs() <= 2.0,
            "azimuth {}",
            robust.params.azimuth
        );
        // And on clean data both agree.
        let clean = simulate_hours(&truth, &inputs);
        let r = fit_plane(&base, &inputs, &clean, FitOptions::robust());
        assert!((r.params.pdc0 - 4000.0).abs() < 20.0, "{}", r.params.pdc0);
    }

    #[test]
    fn one_sided_total_scale_ignores_curtailed_hours() {
        let truth = vec![plane(180.0, 30.0, 3000.0, 1e9)];
        let inputs = clear_hours(20);
        let mut actual = simulate_system_hours(&truth, &inputs);
        for a in actual.iter_mut().step_by(3) {
            *a *= 0.3;
        }
        let s = fit_total_scale(&truth, &inputs, &actual, FitOptions::robust());
        assert!((s - 1.0).abs() < 0.02, "scale {s}");
        let symmetric = fit_total_scale(&truth, &inputs, &actual, FitOptions::default());
        assert!((s - 1.0).abs() < (symmetric - 1.0).abs(), "{s} vs {symmetric}");
    }

    #[test]
    fn keep_mask_modes() {
        let pred = [100.0, 100.0, 100.0, 100.0];
        let actual = [90.0, 30.0, 130.0, 100.0];
        assert_eq!(keep_mask(&pred, &actual, 2, false), [true, false, false, true]);
        assert_eq!(keep_mask(&pred, &actual, 2, true), [false, false, true, true]);
        assert_eq!(keep_mask(&pred, &actual, 0, true), [false; 4]);
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
            ..Default::default()
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
    fn system_hours_match_single_planes_without_sharing() {
        let inputs = clear_hours(2);
        let (a, b) = (plane(90.0, 30.0, 300.0, 250.0), plane(270.0, 30.0, 300.0, 250.0));
        let sys = simulate_system_hours(&[a.clone(), b.clone()], &inputs);
        let (sa, sb) = (simulate_hours(&a, &inputs), simulate_hours(&b, &inputs));
        for i in 0..inputs.len() {
            assert!((sys[i] - sa[i] - sb[i]).abs() < 1e-9);
        }
        assert!(simulate_system_hours(&[], &inputs).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn capacity_counts_shared_inverters_once() {
        let mut planes = vec![plane(90.0, 30.0, 300.0, 250.0), plane(270.0, 30.0, 300.0, 250.0)];
        assert_eq!(ac_capacity(&planes), 500.0);
        planes[0].inverter = Some(0);
        planes[1].inverter = Some(0);
        assert_eq!(ac_capacity(&planes), 250.0);
        planes.push(plane(180.0, 30.0, 100.0, 80.0));
        assert_eq!(ac_capacity(&planes), 330.0);
        assert_eq!(ac_capacity(&[]), 0.0);
    }

    #[test]
    fn total_scale_recovers_an_undersized_guess() {
        let inputs = clear_hours(20);
        let mut truth = vec![
            plane(100.0, 30.0, 3000.0, 4000.0),
            plane(260.0, 30.0, 2000.0, 4000.0),
        ];
        for p in &mut truth {
            p.inverter = Some(0);
        }
        let actual = simulate_system_hours(&truth, &inputs);
        // The user entered 20% too little capacity on both planes.
        let guess: Vec<PlaneParams> = truth
            .iter()
            .map(|p| PlaneParams {
                pdc0: p.pdc0 * 0.8,
                ..p.clone()
            })
            .collect();
        let s = fit_total_scale(&guess, &inputs, &actual, no_trim());
        assert!((s - 1.25).abs() < 0.01, "scale {s}");
        assert_eq!(
            fit_total_scale(&guess, &[], &[], FitOptions::default()),
            1.0,
            "no data"
        );
    }

    #[test]
    fn total_scale_skips_clipped_hours() {
        let inputs = clear_hours(20);
        let mut truth = vec![
            plane(150.0, 30.0, 3000.0, 2000.0),
            plane(210.0, 30.0, 3000.0, 2000.0),
        ];
        for p in &mut truth {
            p.inverter = Some(0);
        }
        let actual = simulate_system_hours(&truth, &inputs);
        assert!(actual.iter().any(|&a| a >= 1999.9), "test data should clip");
        let s = fit_total_scale(&truth, &inputs, &actual, no_trim());
        assert!((s - 1.0).abs() < 0.01, "scale {s}");
    }

    #[test]
    fn scores() {
        let s = score(&[110.0, 0.0, 70.0, 0.0], &[100.0, 0.0, 100.0, 10.0]);
        assert_eq!(s.hours, 3, "the dark hour is skipped");
        assert!((s.mae_w - 50.0 / 3.0).abs() < 1e-9);
        assert!((s.bias_w - (10.0 - 30.0 - 10.0) / 3.0).abs() < 1e-9);
        assert!((s.rmse_w - ((100.0 + 900.0 + 100.0) / 3.0f64).sqrt()).abs() < 1e-9);
        let empty = score(&[], &[]);
        assert_eq!((empty.hours, empty.rmse_w), (0, 0.0));
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
