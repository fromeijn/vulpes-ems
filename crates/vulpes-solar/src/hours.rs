//! Hourly calibration inputs built from a 15-minute weather series: sun
//! position and weather for each quarter, plus a forecast clearness index
//! used to select the clear hours that calibration fits on.

use crate::calibrate::HourInput;
use crate::model::Weather;
use crate::solpos::{SunPosition, sun_position};
use chrono::{DateTime, Datelike};
use std::collections::HashMap;

/// Minimum forecast clearness (GHI / clear-sky GHI) for a calibration hour.
pub const CLEAR_KT: f64 = 0.65;
/// Minimum sun elevation (°, at the third quarter) for a calibration hour.
pub const MIN_ELEVATION: f64 = 10.0;

/// Haurwitz clear-sky GHI (W/m²).
pub fn clear_sky_ghi(zenith: f64) -> f64 {
    let cz = zenith.to_radians().cos();
    if cz <= 0.0 {
        0.0
    } else {
        1098.0 * cz * (-0.057 / cz).exp()
    }
}

/// Day of the year (1-based, UTC) of a Unix timestamp.
pub fn day_of_year(ts: i64) -> u32 {
    DateTime::from_timestamp(ts, 0).map_or(1, |d| d.ordinal())
}

/// Model inputs per complete hour (all four quarters have weather).
#[derive(Debug, Clone, Default)]
pub struct HourlyInputs {
    /// Hour starts (Unix seconds), ascending.
    pub hours: Vec<i64>,
    pub inputs: Vec<HourInput>,
    /// Forecast clearness: hourly GHI / clear-sky GHI.
    pub kt: Vec<f64>,
}

impl HourlyInputs {
    /// Build from 15-minute weather keyed by slot start, for hours in `[from, to)`.
    pub fn build(weather: &HashMap<i64, Weather>, lat: f64, lon: f64, from: i64, to: i64) -> Self {
        let mut out = Self::default();
        let mut h = from - from.rem_euclid(3600);
        while h < to {
            let mut sun = [SunPosition {
                zenith: 0.0,
                azimuth: 0.0,
            }; 4];
            let mut wx = [Weather {
                ghi: 0.0,
                dni: 0.0,
                dhi: 0.0,
                temp_air: 0.0,
                wind_speed: 0.0,
            }; 4];
            let (mut g_sum, mut cs_sum, mut complete) = (0.0, 0.0, true);
            for k in 0..4 {
                let t = h + k as i64 * 900;
                let Some(w) = weather.get(&t) else {
                    complete = false;
                    break;
                };
                sun[k] = sun_position((t + 450) as f64, lat, lon);
                wx[k] = *w;
                g_sum += w.ghi;
                cs_sum += clear_sky_ghi(sun[k].zenith);
            }
            if complete {
                out.hours.push(h);
                out.inputs.push(HourInput {
                    sun,
                    weather: wx,
                    day_of_year: day_of_year(h),
                });
                out.kt.push(if cs_sum > 0.0 { g_sum / cs_sum } else { 0.0 });
            }
            h += 3600;
        }
        out
    }

    /// Whether hour `i` is clear enough, with the sun high enough, to calibrate on.
    pub fn is_clear(&self, i: usize) -> bool {
        self.kt[i] >= CLEAR_KT && self.inputs[i].sun[2].elevation() > MIN_ELEVATION
    }

    pub fn len(&self) -> usize {
        self.hours.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hours.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-21 00:00 UTC.
    const DAY: i64 = 1_782_000_000;

    fn weather(ghi: f64) -> Weather {
        Weather {
            ghi,
            dni: 500.0,
            dhi: 100.0,
            temp_air: 20.0,
            wind_speed: 2.0,
        }
    }

    #[test]
    fn haurwitz_clear_sky() {
        assert!((clear_sky_ghi(0.0) - 1098.0 * (-0.057f64).exp()).abs() < 1e-9);
        assert!(clear_sky_ghi(60.0) < clear_sky_ghi(30.0));
        assert_eq!(clear_sky_ghi(90.0), 0.0);
        assert_eq!(clear_sky_ghi(120.0), 0.0);
    }

    #[test]
    fn day_of_year_in_utc() {
        assert_eq!(day_of_year(DAY), 172);
        assert_eq!(day_of_year(0), 1);
        assert_eq!(day_of_year(DAY + 86_400 - 1), 172);
    }

    #[test]
    fn builds_complete_hours_only() {
        let start = DAY + 10 * 3600;
        let mut w: HashMap<i64, Weather> = (0..12).map(|k| (start + k * 900, weather(400.0))).collect();
        w.remove(&(start + 3600 + 900)); // second hour incomplete
        let d = HourlyInputs::build(&w, 52.0, 5.0, DAY, DAY + 86_400);
        assert_eq!(d.hours, vec![start, start + 2 * 3600]);
        assert_eq!((d.len(), d.kt.len()), (2, 2));
        assert_eq!(d.inputs[0].day_of_year, 172);
        assert_eq!(d.inputs[0].weather[3].ghi, 400.0);
        // Midday in June at 52° N: 400 W/m² is well below clear sky.
        assert!(d.kt[0] > 0.3 && d.kt[0] < 0.6, "{}", d.kt[0]);
        assert!(!d.is_clear(0));
        assert!(HourlyInputs::build(&HashMap::new(), 52.0, 5.0, DAY, DAY + 86_400).is_empty());
    }

    #[test]
    fn clear_hours_need_sun_and_clearness() {
        let noon = DAY + 11 * 3600;
        let night = DAY;
        let w: HashMap<i64, Weather> = (0..4)
            .flat_map(|k| [(noon + k * 900, weather(900.0)), (night + k * 900, weather(0.0))])
            .collect();
        let d = HourlyInputs::build(&w, 52.0, 5.0, DAY, DAY + 86_400);
        assert_eq!(d.hours, vec![night, noon]);
        assert!(!d.is_clear(0), "night");
        assert!(d.is_clear(1), "kt {}", d.kt[1]);
        assert_eq!(d.kt[0], 0.0, "no clear-sky GHI at night");
    }

    #[test]
    fn unaligned_start_rounds_down_to_the_hour() {
        let start = DAY + 10 * 3600;
        let w: HashMap<i64, Weather> = (0..4).map(|k| (start + k * 900, weather(400.0))).collect();
        let d = HourlyInputs::build(&w, 52.0, 5.0, start + 1200, start + 3600);
        assert_eq!(d.hours, vec![start]);
    }
}
