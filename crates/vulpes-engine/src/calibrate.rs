//! PV calibration from measured history: refines the planes a user entered
//! (orientation, tilt, capacity) and scores the result against the current
//! settings, so the host can propose it rather than apply it blindly.
//!
//! - A plane with its own string measurement (DC power of one MPPT) gets its
//!   azimuth, tilt and DC capacity fitted on clear hours, using short-lead
//!   archived forecasts ([`vulpes_solar::calibrate::fit_plane`]). Hours in
//!   which its inverter was near its limit are skipped.
//! - When no plane has a string measurement, one capacity multiplier is
//!   fitted to the system's total AC power; the geometry is kept. Two
//!   orientations cannot be told apart from one total reliably (east + west
//!   looks much like a flatter south-facing array with less capacity).
//! - Inverter limits and efficiencies are kept as entered: hourly means
//!   understate the limit.
//!
//! Scores use day-ahead weather (information-time discipline). With enough
//! data the planes are fitted on even ISO weeks and scored on odd ones; the
//! proposal itself is then refitted on all weeks.

use crate::engine::{WEATHER_VARS, average_models};
use crate::{CalibrateRequest, CalibrateResponse, Error, FitMethod, PlaneFit, Result, WeatherSlot};
use chrono::{DateTime, Datelike, NaiveDate};
use std::collections::HashMap;
use vulpes_solar::calibrate::{
    FitOptions, ac_capacity, fit_plane, fit_total_scale, score, simulate_system_hours,
};
use vulpes_solar::hours::HourlyInputs;
use vulpes_solar::model::{PlaneParams, Weather, validate_planes};
use vulpes_sources::{Location as SrcLocation, open_meteo};

/// Fewest clear hours a plane needs before its fit replaces the entered values.
pub const MIN_FIT_HOURS: usize = 40;
/// Fewest scored hours for a held-out comparison.
const MIN_SCORE_HOURS: usize = 24;
/// A total-scale fit outside this range points at a wrong sensor, not a wrong size.
const SCALE_RANGE: (f64, f64) = (0.5, 2.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Weeks {
    All,
    Even,
    Odd,
}

impl Weeks {
    fn includes(self, ts: i64) -> bool {
        let week = DateTime::from_timestamp(ts, 0).map_or(0, |d| d.iso_week().week());
        match self {
            Self::All => true,
            Self::Even => week % 2 == 0,
            Self::Odd => week % 2 == 1,
        }
    }
}

fn to_weather(slots: &[WeatherSlot]) -> HashMap<i64, Weather> {
    slots
        .iter()
        .map(|s| {
            (
                s.start,
                Weather {
                    ghi: s.ghi,
                    dni: s.dni,
                    dhi: s.dhi,
                    temp_air: s.temp_air,
                    wind_speed: s.wind_speed,
                },
            )
        })
        .collect()
}

fn date(ts: i64) -> NaiveDate {
    DateTime::from_timestamp(ts, 0).map_or(NaiveDate::MIN, |d| d.date_naive())
}

fn fetch(req: &CalibrateRequest, from: i64, to: i64, lead_days: Option<u8>) -> Result<Vec<WeatherSlot>> {
    let (start, end) = (date(from), date(to - 1));
    let range = match lead_days {
        None => open_meteo::Range::Historical { start, end },
        Some(lead_days) => open_meteo::Range::PreviousRuns {
            start,
            end,
            lead_days,
        },
    };
    let models: Vec<&str> = req.weather_models.iter().map(String::as_str).collect();
    let series = open_meteo::fetch(&open_meteo::Request {
        location: SrcLocation {
            lat: req.location.lat,
            lon: req.location.lon,
        },
        models: &models,
        vars: WEATHER_VARS,
        range,
    })
    .map_err(|e| Error::Weather(e.to_string()))?;
    Ok(average_models(series).into_values().collect())
}

/// Measured data, keyed by hour start, with non-finite values dropped.
struct Measured {
    strings: Vec<HashMap<i64, f64>>,
    total: HashMap<i64, f64>,
}

impl Measured {
    fn new(req: &CalibrateRequest) -> Self {
        let map = |v: &[(i64, f64)]| v.iter().copied().filter(|(_, w)| w.is_finite()).collect();
        Self {
            strings: (0..req.planes.len())
                .map(|i| req.strings.get(i).map_or_else(HashMap::new, |s| map(s)))
                .collect(),
            total: map(&req.total),
        }
    }

    fn range(&self) -> Option<(i64, i64)> {
        let ts = self
            .strings
            .iter()
            .flat_map(|s| s.keys())
            .chain(self.total.keys());
        let (lo, hi) = ts.fold((i64::MAX, i64::MIN), |(lo, hi), &t| (lo.min(t), hi.max(t)));
        (lo <= hi).then(|| (lo - lo.rem_euclid(3600), hi - hi.rem_euclid(3600) + 3600))
    }

    /// AC power (W) the system produced in hour `h`, from the total meter or,
    /// when every plane has a string measurement, from the strings.
    fn system_ac(&self, planes: &[PlaneParams], h: i64) -> Option<f64> {
        if let Some(&t) = self.total.get(&h) {
            return Some(t);
        }
        let mut sum = 0.0;
        for (p, s) in planes.iter().zip(&self.strings) {
            sum += s.get(&h)? * p.inverter_eff;
        }
        Some(sum)
    }

    /// Whether plane `i`'s inverter was near its AC limit in hour `h`. Hours
    /// without any measurement count as clipped, so they are left out of the fit.
    fn clipped(&self, planes: &[PlaneParams], i: usize, h: i64) -> bool {
        let p = &planes[i];
        let group: Vec<usize> = match p.inverter {
            None => vec![i],
            Some(id) => (0..planes.len())
                .filter(|&j| planes[j].inverter == Some(id))
                .collect(),
        };
        let known: Vec<Option<f64>> = group
            .iter()
            .map(|&j| self.strings[j].get(&h).map(|w| w * planes[j].inverter_eff))
            .collect();
        let partial: f64 = known.iter().flatten().sum();
        let ac = if known.iter().all(Option::is_some) {
            // Every string on this inverter is measured: that is its output.
            Some(partial)
        } else if let Some(&t) = self.total.get(&h) {
            // The total meter may cover more inverters than this one, which
            // only makes the check more cautious.
            Some(t.max(partial))
        } else if known.iter().any(Option::is_some) {
            // Unmeasured strings count as 0; the robust trim of the fit drops
            // hours that were clipped anyway.
            Some(partial)
        } else {
            None
        };
        ac.is_none_or(|w| w >= 0.9 * p.ac_max)
    }
}

/// Fit plane `i` to its string measurement on clear hours of `weeks`.
fn fit_string(
    planes: &[PlaneParams],
    i: usize,
    m: &Measured,
    data: &HourlyInputs,
    weeks: Weeks,
) -> (PlaneParams, PlaneFit) {
    let (mut inputs, mut actual) = (Vec::new(), Vec::new());
    for (k, &h) in data.hours.iter().enumerate() {
        if !data.is_clear(k) || !weeks.includes(h) || m.clipped(planes, i, h) {
            continue;
        }
        if let Some(&w) = m.strings[i].get(&h) {
            inputs.push(data.inputs[k].clone());
            actual.push(w.max(0.0));
        }
    }
    let p = &planes[i];
    if inputs.len() < MIN_FIT_HOURS {
        let fit = PlaneFit {
            method: FitMethod::Unchanged,
            hours: inputs.len(),
            rmse_w: 0.0,
        };
        return (p.clone(), fit);
    }
    // Fit the DC side: no inverter efficiency, no clipping.
    let base = PlaneParams {
        inverter_eff: 1.0,
        ac_max: f64::INFINITY,
        inverter: None,
        ..p.clone()
    };
    let r = fit_plane(&base, &inputs, &actual, FitOptions::robust());
    let fitted = PlaneParams {
        azimuth: r.params.azimuth,
        tilt: r.params.tilt,
        pdc0: r.params.pdc0,
        ..p.clone()
    };
    let fit = PlaneFit {
        method: FitMethod::String,
        hours: r.hours_used,
        rmse_w: r.rmse,
    };
    (fitted, fit)
}

/// Fit every plane on `weeks`; returns the planes, per-plane fits and warnings.
fn fit_all(
    planes: &[PlaneParams],
    m: &Measured,
    data: &HourlyInputs,
    weeks: Weeks,
) -> (Vec<PlaneParams>, Vec<PlaneFit>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut out = planes.to_vec();
    let unchanged = PlaneFit {
        method: FitMethod::Unchanged,
        hours: 0,
        rmse_w: 0.0,
    };
    let mut fits = vec![unchanged; planes.len()];
    let measured: Vec<bool> = m.strings.iter().map(|s| !s.is_empty()).collect();
    for i in (0..planes.len()).filter(|&i| measured[i]) {
        let (p, fit) = fit_string(planes, i, m, data, weeks);
        if fit.method == FitMethod::Unchanged {
            warnings.push(format!(
                "plane {}: only {} clear hours with string data (need {MIN_FIT_HOURS}); kept as entered",
                i + 1,
                fit.hours
            ));
        }
        out[i] = p;
        fits[i] = fit;
    }
    if measured.iter().all(|&x| !x) && !m.total.is_empty() {
        let (mut inputs, mut actual) = (Vec::new(), Vec::new());
        for (k, &h) in data.hours.iter().enumerate() {
            if let (true, Some(&w)) = (data.is_clear(k) && weeks.includes(h), m.total.get(&h)) {
                inputs.push(data.inputs[k].clone());
                actual.push(w.max(0.0));
            }
        }
        let n = inputs.len();
        // Hours below the inverter limits: the ones the scale is fitted on.
        let cap = ac_capacity(planes);
        let usable = actual.iter().filter(|&&a| a > 0.0 && a < 0.9 * cap).count();
        let s = fit_total_scale(planes, &inputs, &actual, FitOptions::robust());
        if n < MIN_FIT_HOURS {
            warnings.push(format!(
                "only {n} clear hours with PV data (need {MIN_FIT_HOURS}); planes kept as entered"
            ));
        } else if usable < n / 2 || !(SCALE_RANGE.0..=SCALE_RANGE.1).contains(&s) {
            warnings.push(format!(
                "measured PV doesn't match the entered planes (about {s:.2}× the forecast, {usable} of {n} clear hours below the inverter limit); check the PV energy sensor, the kWp values and the inverter limit. Planes kept as entered"
            ));
        } else {
            for (p, f) in out.iter_mut().zip(fits.iter_mut()) {
                p.pdc0 *= s;
                *f = PlaneFit {
                    method: FitMethod::TotalScale,
                    hours: n,
                    rmse_w: 0.0,
                };
            }
            let pred = simulate_system_hours(&out, &inputs);
            let rmse = score(&pred, &actual).rmse_w;
            fits.iter_mut().for_each(|f| f.rmse_w = rmse);
        }
    } else if measured.iter().any(|&x| x) && measured.iter().any(|&x| !x) {
        warnings.push("planes without a string power sensor are kept as entered".into());
    }
    (out, fits, warnings)
}

/// Warn when the strings don't add up to the total: a wrong sensor or unit.
fn check_strings_vs_total(planes: &[PlaneParams], m: &Measured) -> Option<String> {
    if m.total.is_empty() || m.strings.iter().any(|s| s.is_empty()) {
        return None;
    }
    let cap = ac_capacity(planes);
    let (mut s_sum, mut t_sum) = (0.0, 0.0);
    for (&h, &t) in &m.total {
        // Low output is noisy, and an inverter near its AC limit clips the
        // total while its strings (DC) are not clipped: skip those hours.
        if t < 0.2 * cap
            || m.strings.iter().any(|s| !s.contains_key(&h))
            || (0..planes.len()).any(|i| m.clipped(planes, i, h))
        {
            continue;
        }
        s_sum += planes
            .iter()
            .zip(&m.strings)
            .map(|(p, s)| s[&h] * p.inverter_eff)
            .sum::<f64>();
        t_sum += t;
    }
    let ratio = s_sum / t_sum;
    (t_sum > 0.0 && !(0.85..=1.15).contains(&ratio)).then(|| {
        format!(
            "string power sensors add up to {ratio:.2}× the total PV power; check that they measure DC power per string"
        )
    })
}

pub fn calibrate(req: &CalibrateRequest) -> Result<CalibrateResponse> {
    if req.planes.is_empty() {
        return Err(Error::Request("no planes to calibrate".into()));
    }
    validate_planes(&req.planes).map_err(Error::Request)?;
    if !req.strings.is_empty() && req.strings.len() != req.planes.len() {
        return Err(Error::Request(format!(
            "strings has {} entries for {} planes",
            req.strings.len(),
            req.planes.len()
        )));
    }
    let m = Measured::new(req);
    let (from, to) = m
        .range()
        .ok_or_else(|| Error::Request("no PV measurements to calibrate on".into()))?;
    let mut warnings = Vec::new();

    let fit_weather = match &req.weather {
        Some(w) => w.clone(),
        None => fetch(req, from, to, None)?,
    };
    let fit_data = HourlyInputs::build(
        &to_weather(&fit_weather),
        req.location.lat,
        req.location.lon,
        from,
        to,
    );
    let score_weather = match &req.weather_day_ahead {
        Some(w) => w.clone(),
        None => fetch(req, from, to, Some(1)).unwrap_or_else(|e| {
            warnings.push(format!(
                "day-ahead weather unavailable ({e}); scored with short-lead weather"
            ));
            fit_weather.clone()
        }),
    };
    let score_data = HourlyInputs::build(
        &to_weather(&score_weather),
        req.location.lat,
        req.location.lon,
        from,
        to,
    );

    // Hours to score on: daylight hours of `weeks` with a system measurement.
    let scored = |weeks: Weeks| -> (Vec<usize>, Vec<f64>) {
        let mut idx = Vec::new();
        let mut actual = Vec::new();
        for (k, &h) in score_data.hours.iter().enumerate() {
            if let (true, Some(a)) = (weeks.includes(h), m.system_ac(&req.planes, h)) {
                idx.push(k);
                actual.push(a.max(0.0));
            }
        }
        (idx, actual)
    };
    let score_on = |planes: &[PlaneParams], idx: &[usize], actual: &[f64]| {
        let inputs: Vec<_> = idx.iter().map(|&k| score_data.inputs[k].clone()).collect();
        score(&simulate_system_hours(planes, &inputs), actual)
    };

    let (held_planes, held_fits, _) = fit_all(&req.planes, &m, &fit_data, Weeks::Even);
    let (odd_idx, odd_actual) = scored(Weeks::Odd);
    let fitted_any = held_fits.iter().any(|f| f.method != FitMethod::Unchanged);
    let odd_hours = score_on(&req.planes, &odd_idx, &odd_actual).hours;
    let held_out = fitted_any && odd_hours >= MIN_SCORE_HOURS;

    let (planes, fits, fit_warnings) = fit_all(&req.planes, &m, &fit_data, Weeks::All);
    warnings.extend(fit_warnings);
    warnings.extend(check_strings_vs_total(&req.planes, &m));

    let (score_current, score_proposed) = if held_out {
        (
            Some(score_on(&req.planes, &odd_idx, &odd_actual)),
            Some(score_on(&held_planes, &odd_idx, &odd_actual)),
        )
    } else {
        let (idx, actual) = scored(Weeks::All);
        if score_on(&req.planes, &idx, &actual).hours == 0 {
            warnings.push("no total PV measurement to score against".into());
            (None, None)
        } else {
            if fits.iter().any(|f| f.method != FitMethod::Unchanged) {
                warnings.push("too little data to hold weeks out; scores are in-sample".into());
            }
            (
                Some(score_on(&req.planes, &idx, &actual)),
                Some(score_on(&planes, &idx, &actual)),
            )
        }
    };
    Ok(CalibrateResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        planes,
        fits,
        score_current,
        score_proposed,
        held_out,
        from,
        to,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Location;
    use vulpes_solar::calibrate::simulate_hours;
    use vulpes_solar::sun_position;

    // 2026-05-04 00:00 UTC, a Monday (ISO week 19).
    const START: i64 = 1_777_852_800;

    /// Clear-sky-ish 15-minute weather at 52° N, 5° E; every third day cloudy.
    fn weather(days: i64) -> Vec<WeatherSlot> {
        (0..days * 96)
            .map(|k| {
                let t = START + k * 900;
                let s = sun_position((t + 450) as f64, 52.0, 5.0);
                let cz = s.zenith.to_radians().cos().max(0.0);
                let cloudy = (k / 96) % 3 == 2;
                let dni = if cz > 0.0 && !cloudy {
                    850.0 * (-0.15 / cz.max(0.05)).exp()
                } else {
                    0.0
                };
                let dhi = if cloudy { 150.0 * cz } else { 90.0 * cz };
                WeatherSlot {
                    start: t,
                    ghi: dni * cz + dhi,
                    dni,
                    dhi,
                    temp_air: 18.0,
                    wind_speed: 2.0,
                }
            })
            .collect()
    }

    fn east_west() -> Vec<PlaneParams> {
        let mut planes = vec![
            PlaneParams::new(95.0, 25.0, 3200.0, 4000.0),
            PlaneParams::new(275.0, 25.0, 2400.0, 4000.0),
        ];
        for p in &mut planes {
            p.inverter = Some(0);
        }
        planes
    }

    /// Hourly DC per plane and AC total of `truth` under `w`.
    type Series = Vec<(i64, f64)>;

    fn measurements(truth: &[PlaneParams], w: &[WeatherSlot]) -> (Vec<Series>, Series) {
        let end = w.iter().map(|s| s.start).max().unwrap_or(START) + 900;
        let data = HourlyInputs::build(&to_weather(w), 52.0, 5.0, START, end);
        let strings = truth
            .iter()
            .map(|p| {
                let dc = PlaneParams {
                    inverter_eff: 1.0,
                    ac_max: f64::INFINITY,
                    inverter: None,
                    ..p.clone()
                };
                data.hours
                    .iter()
                    .copied()
                    .zip(simulate_hours(&dc, &data.inputs))
                    .collect()
            })
            .collect();
        let total = data
            .hours
            .iter()
            .copied()
            .zip(simulate_system_hours(truth, &data.inputs))
            .collect();
        (strings, total)
    }

    fn request(planes: Vec<PlaneParams>, days: i64) -> (CalibrateRequest, Vec<PlaneParams>) {
        let truth = east_west();
        let w = weather(days);
        let (strings, total) = measurements(&truth, &w);
        let req = CalibrateRequest {
            location: Location { lat: 52.0, lon: 5.0 },
            planes,
            strings,
            total,
            weather_models: vec![],
            weather: Some(w.clone()),
            weather_day_ahead: Some(w),
        };
        (req, truth)
    }

    /// A rough guess as a user would enter it: compass direction, round tilt, nameplate kWp.
    fn guess() -> Vec<PlaneParams> {
        let mut planes = vec![
            PlaneParams::new(90.0, 35.0, 3000.0, 4000.0),
            PlaneParams::new(270.0, 35.0, 3000.0, 4000.0),
        ];
        for p in &mut planes {
            p.inverter = Some(0);
        }
        planes
    }

    #[test]
    fn string_data_recovers_each_plane() {
        let (req, truth) = request(guess(), 28);
        let r = calibrate(&req).unwrap();
        for (p, t) in r.planes.iter().zip(&truth) {
            assert!(
                (p.azimuth - t.azimuth).abs() <= 2.0,
                "azimuth {} vs {}",
                p.azimuth,
                t.azimuth
            );
            assert!((p.tilt - t.tilt).abs() <= 2.0, "tilt {} vs {}", p.tilt, t.tilt);
            assert!(
                (p.pdc0 / t.pdc0 - 1.0).abs() < 0.03,
                "pdc0 {} vs {}",
                p.pdc0,
                t.pdc0
            );
            assert_eq!(
                (p.ac_max, p.inverter, p.inverter_eff),
                (4000.0, Some(0), 0.965),
                "kept as entered"
            );
        }
        assert!(
            r.fits
                .iter()
                .all(|f| f.method == FitMethod::String && f.hours >= MIN_FIT_HOURS)
        );
        assert!(r.held_out, "{:?}", r.warnings);
        let (cur, new) = (r.score_current.unwrap(), r.score_proposed.unwrap());
        assert!(new.rmse_w < 0.5 * cur.rmse_w, "{cur:?} → {new:?}");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(r.from, START);
    }

    #[test]
    fn hours_at_the_shared_limit_are_left_out() {
        // A 3 kW inverter on 5.6 kWp: the total clips around midday. Fitting
        // the strings in those hours would see the inverter, not the panels.
        let mut truth = east_west();
        truth.iter_mut().for_each(|p| p.ac_max = 3000.0);
        let w = weather(28);
        let (strings, total) = measurements(&truth, &w);
        // While clipping, the inverter moves off the maximum power point: the
        // strings then deliver only what the AC limit allows.
        let clipped: HashMap<i64, f64> = total.iter().copied().filter(|x| x.1 >= 2999.0).collect();
        assert!(clipped.len() > 20, "test data should clip");
        let dc_sum = |h: i64| {
            strings
                .iter()
                .map(|s| s.iter().find(|x| x.0 == h).unwrap().1)
                .sum::<f64>()
        };
        let factor: HashMap<i64, f64> = clipped
            .iter()
            .map(|(&h, &t)| (h, t / 0.965 / dc_sum(h)))
            .collect();
        let strings: Vec<Vec<(i64, f64)>> = strings
            .iter()
            .map(|s| {
                s.iter()
                    .map(|&(h, w)| (h, w * factor.get(&h).unwrap_or(&1.0)))
                    .collect()
            })
            .collect();
        let mut planes = guess();
        planes.iter_mut().for_each(|p| p.ac_max = 3000.0);
        let req = CalibrateRequest {
            location: Location { lat: 52.0, lon: 5.0 },
            planes,
            strings,
            total,
            weather_models: vec![],
            weather: Some(w.clone()),
            weather_day_ahead: Some(w),
        };
        let r = calibrate(&req).unwrap();
        for (p, t) in r.planes.iter().zip(&truth) {
            assert!(
                (p.pdc0 / t.pdc0 - 1.0).abs() < 0.03,
                "pdc0 {} vs {}",
                p.pdc0,
                t.pdc0
            );
        }
    }

    #[test]
    fn without_strings_only_the_capacity_is_scaled() {
        // Right orientation, but 25% too much capacity entered on both arrays.
        let entered: Vec<PlaneParams> = east_west()
            .into_iter()
            .map(|p| PlaneParams {
                pdc0: p.pdc0 * 1.25,
                ..p
            })
            .collect();
        let (mut req, truth) = request(entered.clone(), 28);
        req.strings.clear();
        let r = calibrate(&req).unwrap();
        assert!(r.fits.iter().all(|f| f.method == FitMethod::TotalScale));
        for ((p, e), t) in r.planes.iter().zip(&entered).zip(&truth) {
            assert_eq!((p.azimuth, p.tilt), (e.azimuth, e.tilt), "geometry kept");
            assert!(
                (p.pdc0 / t.pdc0 - 1.0).abs() < 0.02,
                "pdc0 {} vs {}",
                p.pdc0,
                t.pdc0
            );
        }
        let (cur, new) = (r.score_current.unwrap(), r.score_proposed.unwrap());
        assert!(new.rmse_w < 0.5 * cur.rmse_w, "{cur:?} → {new:?}");
    }

    #[test]
    fn a_wildly_off_total_is_reported_not_applied() {
        let (mut req, _) = request(guess(), 28);
        req.strings.clear();
        req.total.iter_mut().for_each(|x| x.1 *= 3.0); // e.g. a sensor counted three times
        let r = calibrate(&req).unwrap();
        assert_eq!(r.planes, guess());
        assert!(r.fits.iter().all(|f| f.method == FitMethod::Unchanged));
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("check the PV energy sensor")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn too_little_data_keeps_the_planes() {
        let (req, _) = request(guess(), 2);
        let r = calibrate(&req).unwrap();
        assert_eq!(r.planes, guess());
        assert!(r.fits.iter().all(|f| f.method == FitMethod::Unchanged));
        assert!(!r.held_out);
        assert!(
            r.warnings.iter().any(|w| w.contains("clear hours")),
            "{:?}",
            r.warnings
        );
        let (cur, new) = (r.score_current.unwrap(), r.score_proposed.unwrap());
        assert_eq!(cur, new);
    }

    #[test]
    fn short_history_scores_in_sample() {
        // 10 days within weeks 19 and 20: even weeks alone have too few hours,
        // all weeks together are enough to fit.
        let (req, _) = request(guess(), 10);
        let r = calibrate(&req).unwrap();
        assert!(
            r.fits.iter().all(|f| f.method == FitMethod::String),
            "{:?}",
            r.warnings
        );
        if !r.held_out {
            assert!(
                r.warnings.iter().any(|w| w.contains("in-sample")),
                "{:?}",
                r.warnings
            );
        }
        assert!(r.score_proposed.is_some());
    }

    #[test]
    fn one_plane_without_a_string_is_kept() {
        let (mut req, truth) = request(guess(), 28);
        req.strings[1].clear();
        let r = calibrate(&req).unwrap();
        assert_eq!(r.fits[0].method, FitMethod::String);
        assert_eq!(r.fits[1].method, FitMethod::Unchanged);
        assert_eq!(r.planes[1], guess()[1]);
        assert!((r.planes[0].azimuth - truth[0].azimuth).abs() <= 2.0);
        assert!(
            r.warnings.iter().any(|w| w.contains("without a string")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn strings_that_dont_add_up_are_reported() {
        let (mut req, _) = request(guess(), 28);
        for x in &mut req.strings[0] {
            x.1 /= 1000.0; // kW sensor read as W
        }
        let r = calibrate(&req).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("add up")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn errors() {
        let (req, _) = request(guess(), 2);
        let check = |f: &dyn Fn(&mut CalibrateRequest), want: &str| {
            let mut r = req.clone();
            f(&mut r);
            let err = calibrate(&r).expect_err(want).to_string();
            assert!(err.contains(want), "{want}: got '{err}'");
        };
        check(&|r| r.planes.clear(), "no planes");
        check(&|r| r.strings.truncate(1), "strings has 1 entries for 2 planes");
        check(
            &|r| {
                r.strings.clear();
                r.total.clear();
            },
            "no PV measurements",
        );
        check(&|r| r.planes[1].ac_max = 1.0, "same ac_max");
    }

    #[test]
    fn non_finite_measurements_are_ignored() {
        let (mut req, _) = request(guess(), 28);
        for s in &mut req.strings {
            s.iter_mut().step_by(5).for_each(|x| x.1 = f64::NAN);
        }
        req.total.iter_mut().step_by(7).for_each(|x| x.1 = f64::INFINITY);
        let r = calibrate(&req).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("NaN") && !json.contains("null,\"score"), "{json}");
        assert!(r.planes.iter().all(|p| p.pdc0.is_finite() && p.pdc0 > 0.0));
    }

    #[test]
    fn json_round_trip() {
        let (req, _) = request(guess(), 28);
        let text = serde_json::to_string(&req).unwrap();
        let out = crate::calibrate_json(&text).unwrap();
        let resp: CalibrateResponse = serde_json::from_str(&out).unwrap();
        assert_eq!(resp.planes.len(), 2);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "fits",
                "from",
                "held_out",
                "planes",
                "score_current",
                "score_proposed",
                "to",
                "version",
                "warnings"
            ]
        );
        assert_eq!(v["fits"][0]["method"], "string");
        let mut sk: Vec<&str> = v["score_current"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        sk.sort();
        assert_eq!(sk, ["bias_w", "hours", "mae_w", "rmse_w"]);
        assert!(matches!(crate::calibrate_json("{"), Err(Error::Request(_))));
    }

    /// The JSON the Home Assistant integration sends: optional fields left out,
    /// extra plane keys present, tuples as arrays.
    #[test]
    fn integration_request_shape_parses_with_defaults() {
        let json = serde_json::json!({
            "location": {"lat": 52.0, "lon": 5.0},
            "planes": [{"azimuth": 90, "tilt": 35, "pdc0": 3000, "ac_max": 4000, "inverter": 0,
                        "power_entity": "sensor.pv1_power"}],
            "strings": [[[1_777_852_800, 120.5]]],
            "total": [[1_777_852_800, 115]]
        });
        let r: CalibrateRequest = serde_json::from_value(json).unwrap();
        assert_eq!(r.planes[0].inverter, Some(0));
        assert_eq!(r.strings[0], vec![(1_777_852_800, 120.5)]);
        assert_eq!(
            r.weather_models,
            vec!["knmi_harmonie_arome_netherlands", "icon_d2"]
        );
        assert!(r.weather.is_none() && r.weather_day_ahead.is_none());
    }

    #[test]
    fn iso_week_parity() {
        assert!(Weeks::Odd.includes(START) && !Weeks::Even.includes(START));
        assert!(Weeks::Even.includes(START + 7 * 86_400));
        assert!(Weeks::All.includes(START));
    }

    /// Measurements for hour START only: per-plane string DC power and an optional total.
    fn measured(strings: &[Option<f64>], total: Option<f64>) -> Measured {
        Measured {
            strings: strings
                .iter()
                .map(|s| s.map(|w| (START, w)).into_iter().collect())
                .collect(),
            total: total.map(|t| (START, t)).into_iter().collect(),
        }
    }

    fn plane(ac_max: f64, inverter: Option<u32>) -> PlaneParams {
        PlaneParams {
            inverter,
            inverter_eff: 1.0,
            ..PlaneParams::new(180.0, 35.0, 3000.0, ac_max)
        }
    }

    #[test]
    fn separate_inverters_are_judged_by_their_own_string() {
        // Two 2 kW inverters at 1.5 kW each: the 3 kW total is not either inverter's output.
        let planes = [plane(2000.0, None), plane(2000.0, None)];
        let m = measured(&[Some(1500.0), Some(1500.0)], Some(3000.0));
        assert!(!m.clipped(&planes, 0, START) && !m.clipped(&planes, 1, START));
        let m = measured(&[Some(1900.0), Some(1500.0)], Some(3400.0));
        assert!(m.clipped(&planes, 0, START) && !m.clipped(&planes, 1, START));
    }

    #[test]
    fn shared_inverter_with_one_string_sensor_and_no_total() {
        let planes = [plane(4000.0, Some(0)), plane(4000.0, Some(0))];
        // Only plane 0 is measured: the unmeasured string counts as 0, so the hour is usable.
        let m = measured(&[Some(1500.0), None], None);
        assert!(!m.clipped(&planes, 0, START));
        assert!(measured(&[Some(3800.0), None], None).clipped(&planes, 0, START));
        // With a total, that is used for the unmeasured part.
        assert!(measured(&[Some(1500.0), None], Some(3700.0)).clipped(&planes, 0, START));
        // Nothing measured: unknown, left out.
        assert!(measured(&[None, None], None).clipped(&planes, 0, START));
        // Both measured: their sum is the inverter's output.
        assert!(measured(&[Some(1900.0), Some(1900.0)], None).clipped(&planes, 1, START));
        assert!(!measured(&[Some(1500.0), Some(1500.0)], Some(9000.0)).clipped(&planes, 1, START));
    }

    #[test]
    fn clipped_hours_do_not_trigger_the_string_sum_warning() {
        // 6 kWp DC on a 4 kW inverter: at midday the strings give 5.5 kW, the total 4 kW.
        let planes = [plane(4000.0, Some(0)), plane(4000.0, Some(0))];
        let hours = |n: i64, s: f64, t: f64| -> Vec<(i64, f64, f64)> {
            (0..n).map(|k| (START + k * 3600, s, t)).collect()
        };
        let build = |rows: &[(i64, f64, f64)]| Measured {
            strings: vec![
                rows.iter().map(|&(h, s, _)| (h, s / 2.0)).collect(),
                rows.iter().map(|&(h, s, _)| (h, s / 2.0)).collect(),
            ],
            total: rows.iter().map(|&(h, _, t)| (h, t)).collect(),
        };
        let mut rows = hours(10, 2000.0, 2000.0);
        rows.extend(
            hours(30, 5500.0, 4000.0)
                .into_iter()
                .map(|(h, s, t)| (h + 10 * 3600, s, t)),
        );
        assert_eq!(check_strings_vs_total(&planes, &build(&rows)), None);
        // A wrong sensor still warns: strings at twice the total in unclipped hours.
        assert!(check_strings_vs_total(&planes, &build(&hours(10, 3000.0, 1500.0))).is_some());
    }

    #[test]
    fn measured_range_covers_whole_hours() {
        let (mut req, _) = request(guess(), 1);
        req.strings = vec![vec![(START + 1800, 1.0)], vec![]];
        req.total = vec![(START + 7200, 1.0)];
        let m = Measured::new(&req);
        assert_eq!(m.range(), Some((START, START + 3 * 3600)));
        req.strings.clear();
        req.total.clear();
        assert_eq!(Measured::new(&req).range(), None);
    }
}
