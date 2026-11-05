//! Per-plane PV model: plane-of-array irradiance → incidence-angle loss →
//! cell temperature (Faiman) → DC power (PVWatts) → inverter efficiency and
//! clipping. One plane = one orientation; with microinverters, one panel.

use crate::irradiance::{cos_aoi, extra_dni, ground_diffuse, perez_sky_diffuse};
use crate::solpos::SunPosition;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaneParams {
    /// Degrees clockwise from north (180 = south).
    pub azimuth: f64,
    /// Degrees from horizontal.
    pub tilt: f64,
    /// DC power at 1000 W/m² and 25 °C cell temperature (W).
    pub pdc0: f64,
    /// Inverter AC limit (W).
    pub ac_max: f64,
    #[serde(default = "defaults::inverter_eff")]
    pub inverter_eff: f64,
    /// Power temperature coefficient (1/°C).
    #[serde(default = "defaults::gamma")]
    pub gamma: f64,
    /// ASHRAE incidence-angle modifier parameter.
    #[serde(default = "defaults::iam_b0")]
    pub iam_b0: f64,
    #[serde(default = "defaults::albedo")]
    pub albedo: f64,
    /// Faiman heat-loss coefficients (W/m²K and W/m²K per m/s).
    #[serde(default = "defaults::u0")]
    pub u0: f64,
    #[serde(default = "defaults::u1")]
    pub u1: f64,
}

mod defaults {
    pub fn inverter_eff() -> f64 {
        0.965
    }
    pub fn gamma() -> f64 {
        -0.004
    }
    pub fn iam_b0() -> f64 {
        0.05
    }
    pub fn albedo() -> f64 {
        0.2
    }
    pub fn u0() -> f64 {
        25.0
    }
    pub fn u1() -> f64 {
        6.84
    }
}

/// Weather at one instant. Irradiance in W/m², temperature in °C, wind in m/s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weather {
    pub ghi: f64,
    pub dni: f64,
    pub dhi: f64,
    pub temp_air: f64,
    pub wind_speed: f64,
}

/// Components of plane-of-array irradiance (W/m²), after incidence-angle losses on beam.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Poa {
    pub beam: f64,
    pub sky_diffuse: f64,
    pub ground: f64,
}

impl Poa {
    pub fn total(&self) -> f64 {
        self.beam + self.sky_diffuse + self.ground
    }
}

/// ASHRAE incidence-angle modifier.
pub fn iam_ashrae(cos_aoi: f64, b0: f64) -> f64 {
    if cos_aoi <= 0.0 {
        0.0
    } else {
        (1.0 - b0 * (1.0 / cos_aoi - 1.0)).clamp(0.0, 1.0)
    }
}

pub fn poa(p: &PlaneParams, sun: SunPosition, w: Weather, day_of_year: u32) -> Poa {
    if sun.zenith >= 90.0 {
        return Poa {
            beam: 0.0,
            sky_diffuse: 0.0,
            ground: 0.0,
        };
    }
    let c = cos_aoi(p.tilt, p.azimuth, sun.zenith, sun.azimuth);
    Poa {
        beam: w.dni.max(0.0) * c.max(0.0) * iam_ashrae(c, p.iam_b0),
        sky_diffuse: perez_sky_diffuse(
            p.tilt,
            p.azimuth,
            w.dhi.max(0.0),
            w.dni.max(0.0),
            extra_dni(day_of_year),
            sun.zenith,
            sun.azimuth,
        ),
        ground: ground_diffuse(p.tilt, w.ghi.max(0.0), p.albedo),
    }
}

/// Faiman cell temperature (°C).
pub fn cell_temp(poa_total: f64, temp_air: f64, wind_speed: f64, u0: f64, u1: f64) -> f64 {
    temp_air + poa_total / (u0 + u1 * wind_speed.max(0.0))
}

/// AC power (W) of one plane, with `beam_factor` in 0..=1 scaling the beam
/// component for shading (1 = unshaded).
pub fn plane_ac_power(
    p: &PlaneParams,
    sun: SunPosition,
    w: Weather,
    day_of_year: u32,
    beam_factor: f64,
) -> f64 {
    let mut irr = poa(p, sun, w, day_of_year);
    irr.beam *= beam_factor.clamp(0.0, 1.0);
    let g = irr.total();
    if g <= 0.0 {
        return 0.0;
    }
    let tc = cell_temp(g, w.temp_air, w.wind_speed, p.u0, p.u1);
    let dc = p.pdc0 * g / 1000.0 * (1.0 + p.gamma * (tc - 25.0));
    (dc * p.inverter_eff).clamp(0.0, p.ac_max)
}
