//! Solar position with the PSA algorithm (Blanco-Muriel et al., 2001,
//! "Computing the solar vector", Solar Energy 70(5)). Accuracy ~0.01° for
//! 1999-2015 and still well below 0.05° today, ample for PV forecasting.

use std::f64::consts::{PI, TAU};

/// Sun position in degrees. Azimuth is clockwise from north (180 = south).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunPosition {
    pub zenith: f64,
    pub azimuth: f64,
}

impl SunPosition {
    pub fn elevation(&self) -> f64 {
        90.0 - self.zenith
    }
}

/// Sun position at Unix time `ts` (seconds, UTC) for `lat`/`lon` in degrees.
pub fn sun_position(ts: f64, lat: f64, lon: f64) -> SunPosition {
    let decimal_hours = ts.rem_euclid(86_400.0) / 3600.0;
    let n = ts / 86_400.0 + 2_440_587.5 - 2_451_545.0; // days since J2000.0

    // Ecliptic coordinates.
    let omega = 2.1429 - 0.001_039_459_4 * n;
    let mean_longitude = 4.895_063_0 + 0.017_202_791_698 * n;
    let mean_anomaly = 6.240_060_0 + 0.017_201_969_9 * n;
    let ecl_longitude =
        mean_longitude + 0.033_416_07 * mean_anomaly.sin() + 0.000_348_94 * (2.0 * mean_anomaly).sin()
            - 0.000_113_4
            - 0.000_020_3 * omega.sin();
    let ecl_obliquity = 0.409_092_8 - 6.2140e-9 * n + 0.000_039_6 * omega.cos();

    // Celestial coordinates.
    let (sin_l, cos_l) = ecl_longitude.sin_cos();
    let mut right_ascension = (ecl_obliquity.cos() * sin_l).atan2(cos_l);
    if right_ascension < 0.0 {
        right_ascension += TAU;
    }
    let declination = (ecl_obliquity.sin() * sin_l).asin();

    // Local coordinates.
    let gmst = 6.697_424_324_2 + 0.065_709_828_3 * n + decimal_hours;
    let lmst = (gmst * 15.0 + lon).to_radians();
    let hour_angle = lmst - right_ascension;
    let lat_r = lat.to_radians();
    let (sin_lat, cos_lat) = lat_r.sin_cos();
    let cos_h = hour_angle.cos();
    let mut zenith = (cos_lat * cos_h * declination.cos() + declination.sin() * sin_lat).acos();
    let mut azimuth = (-hour_angle.sin()).atan2(declination.tan() * cos_lat - sin_lat * cos_h);
    if azimuth < 0.0 {
        azimuth += TAU;
    }
    // Parallax correction.
    const EARTH_MEAN_RADIUS_KM: f64 = 6371.01;
    const ASTRONOMICAL_UNIT_KM: f64 = 149_597_890.0;
    zenith += EARTH_MEAN_RADIUS_KM / ASTRONOMICAL_UNIT_KM * zenith.sin();

    SunPosition {
        zenith: zenith * 180.0 / PI,
        azimuth: azimuth * 180.0 / PI,
    }
}
