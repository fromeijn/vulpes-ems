//! Physics-based PV forecasting: solar position, plane-of-array irradiance
//! (Perez), incidence-angle and temperature losses, inverter clipping.
//! Machine-learned corrections are layered on top in later phases.

pub mod calibrate;
pub mod hours;
pub mod irradiance;
pub mod model;
pub mod shading;
pub mod solpos;

pub use model::{PlaneParams, Weather, combine_ac, plane_ac_power, plane_dc_power, system_ac_power};
pub use solpos::{SunPosition, sun_position};

/// Synthetic inputs shared by the unit tests.
#[cfg(test)]
pub(crate) mod testdata {
    use crate::calibrate::HourInput;
    use crate::model::{PlaneParams, Weather};
    use crate::solpos::{SunPosition, sun_position};

    /// Mid-June 2025, on the hour.
    pub const START: i64 = 1_750_000_000 - 1_750_000_000 % 3600;

    pub fn plane(azimuth: f64, tilt: f64, pdc0: f64, ac_max: f64) -> PlaneParams {
        PlaneParams::new(azimuth, tilt, pdc0, ac_max)
    }

    /// Clear-sky-ish hours at 52° N, 5° E with the sun above 10° elevation.
    pub fn clear_hours(days: i64) -> Vec<HourInput> {
        let mut inputs = Vec::new();
        for hour in 0..(24 * days) {
            let t0 = START + hour * 3600;
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
        inputs
    }
}
