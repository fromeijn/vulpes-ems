//! Per-plane PV model: plane-of-array irradiance → incidence-angle loss →
//! cell temperature (Faiman) → DC power (PVWatts) → inverter efficiency and
//! clipping. One plane = one orientation; with microinverters, one panel.
//!
//! Planes that share a string inverter carry the same `inverter` id: their DC
//! power is summed before the shared AC limit is applied ([`combine_ac`]).

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
    /// Planes with the same id share one inverter (and its `ac_max`); `None`
    /// means the plane has an inverter of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverter: Option<u32>,
}

impl PlaneParams {
    /// A plane with default loss parameters and its own inverter.
    pub fn new(azimuth: f64, tilt: f64, pdc0: f64, ac_max: f64) -> Self {
        Self {
            azimuth,
            tilt,
            pdc0,
            ac_max,
            inverter_eff: defaults::inverter_eff(),
            gamma: defaults::gamma(),
            iam_b0: defaults::iam_b0(),
            albedo: defaults::albedo(),
            u0: defaults::u0(),
            u1: defaults::u1(),
            inverter: None,
        }
    }
}

/// Check that planes sharing an inverter agree on its limit and efficiency,
/// and that all values are finite and non-negative.
pub fn validate_planes(planes: &[PlaneParams]) -> Result<(), String> {
    for (i, p) in planes.iter().enumerate() {
        let fields = [p.azimuth, p.tilt, p.pdc0, p.ac_max, p.inverter_eff];
        if fields.iter().any(|x| !x.is_finite()) || p.pdc0 < 0.0 || p.ac_max < 0.0 || p.inverter_eff <= 0.0 {
            return Err(format!(
                "plane {i}: invalid azimuth/tilt/pdc0/ac_max/inverter_eff"
            ));
        }
        let Some(id) = p.inverter else { continue };
        if let Some(q) = planes[..i].iter().find(|q| q.inverter == Some(id))
            && (q.ac_max != p.ac_max || q.inverter_eff != p.inverter_eff)
        {
            return Err(format!(
                "planes on inverter {id} must have the same ac_max and inverter_eff"
            ));
        }
    }
    Ok(())
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

/// DC power (W, ≥ 0) of one plane, with `beam_factor` in 0..=1 scaling the
/// beam component for shading (1 = unshaded).
pub fn plane_dc_power(
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
    (p.pdc0 * g / 1000.0 * (1.0 + p.gamma * (tc - 25.0))).max(0.0)
}

/// AC power (W) of one plane on its own inverter (its `inverter` id is ignored).
pub fn plane_ac_power(
    p: &PlaneParams,
    sun: SunPosition,
    w: Weather,
    day_of_year: u32,
    beam_factor: f64,
) -> f64 {
    (plane_dc_power(p, sun, w, day_of_year, beam_factor) * p.inverter_eff).clamp(0.0, p.ac_max)
}

/// Total AC power (W) of a system from per-plane DC power `dc` (same order as
/// `planes`). Planes without an inverter id clip on their own; planes sharing
/// an id are summed first and clipped once at their common `ac_max`.
pub fn combine_ac(planes: &[PlaneParams], dc: &[f64]) -> f64 {
    debug_assert_eq!(planes.len(), dc.len());
    let mut total = 0.0;
    // (inverter id, summed AC before clipping, limit); few inverters, so a Vec.
    let mut shared: Vec<(u32, f64, f64)> = Vec::new();
    for (p, &d) in planes.iter().zip(dc) {
        let ac = d.max(0.0) * p.inverter_eff;
        match p.inverter {
            None => total += ac.min(p.ac_max),
            Some(id) => match shared.iter_mut().find(|s| s.0 == id) {
                Some(s) => s.1 += ac,
                None => shared.push((id, ac, p.ac_max)),
            },
        }
    }
    total + shared.iter().map(|s| s.1.min(s.2)).sum::<f64>()
}

/// Total AC power (W) of all planes, unshaded.
pub fn system_ac_power(planes: &[PlaneParams], sun: SunPosition, w: Weather, day_of_year: u32) -> f64 {
    let dc: Vec<f64> = planes
        .iter()
        .map(|p| plane_dc_power(p, sun, w, day_of_year, 1.0))
        .collect();
    combine_ac(planes, &dc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane() -> PlaneParams {
        serde_json::from_str(r#"{"azimuth": 180, "tilt": 35, "pdc0": 400, "ac_max": 300}"#).unwrap()
    }
    fn sun(zenith: f64) -> SunPosition {
        SunPosition {
            zenith,
            azimuth: 180.0,
        }
    }
    fn weather(ghi: f64, dni: f64, dhi: f64, temp_air: f64) -> Weather {
        Weather {
            ghi,
            dni,
            dhi,
            temp_air,
            wind_speed: 1.0,
        }
    }

    #[test]
    fn serde_fills_defaults() {
        let p = plane();
        assert_eq!((p.inverter_eff, p.gamma, p.iam_b0), (0.965, -0.004, 0.05));
        assert_eq!((p.albedo, p.u0, p.u1), (0.2, 25.0, 6.84));
        let back: PlaneParams = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        assert!(serde_json::from_str::<PlaneParams>(r#"{"azimuth": 180, "tilt": 35}"#).is_err());
    }

    #[test]
    fn iam_bounds_and_shape() {
        assert_eq!(iam_ashrae(1.0, 0.05), 1.0);
        assert_eq!(iam_ashrae(0.0, 0.05), 0.0);
        assert_eq!(iam_ashrae(-0.5, 0.05), 0.0);
        assert!(iam_ashrae(0.5, 0.05) < iam_ashrae(0.9, 0.05));
        assert_eq!(iam_ashrae(0.01, 0.05), 0.0, "clamped at grazing angles");
    }

    #[test]
    fn cell_temperature() {
        assert_eq!(cell_temp(0.0, 12.0, 3.0, 25.0, 6.84), 12.0);
        let calm = cell_temp(800.0, 20.0, 0.0, 25.0, 6.84);
        assert!((calm - 52.0).abs() < 1e-9);
        assert!(cell_temp(800.0, 20.0, 5.0, 25.0, 6.84) < calm);
        assert_eq!(
            cell_temp(800.0, 20.0, -3.0, 25.0, 6.84),
            calm,
            "negative wind treated as calm"
        );
    }

    #[test]
    fn dark_or_sun_down_gives_zero() {
        let p = plane();
        assert_eq!(
            plane_ac_power(&p, sun(100.0), weather(50.0, 50.0, 50.0, 10.0), 172, 1.0),
            0.0
        );
        assert_eq!(
            plane_ac_power(&p, sun(40.0), weather(0.0, 0.0, 0.0, 10.0), 172, 1.0),
            0.0
        );
        let poa_down = poa(&p, sun(90.0), weather(10.0, 10.0, 10.0, 10.0), 172);
        assert_eq!(poa_down.total(), 0.0);
    }

    #[test]
    fn negative_weather_inputs_are_ignored() {
        let p = plane();
        let w = weather(-5.0, -5.0, -5.0, 10.0);
        assert_eq!(plane_ac_power(&p, sun(40.0), w, 172, 1.0), 0.0);
    }

    #[test]
    fn clipping_and_linear_scaling() {
        let mut p = plane();
        let bright = weather(1000.0, 900.0, 100.0, 15.0);
        assert_eq!(plane_ac_power(&p, sun(35.0), bright, 172, 1.0), p.ac_max);
        p.ac_max = 1e9;
        let one = plane_ac_power(&p, sun(35.0), bright, 172, 1.0);
        p.pdc0 *= 2.0;
        let two = plane_ac_power(&p, sun(35.0), bright, 172, 1.0);
        assert!((two - 2.0 * one).abs() < 1e-9 && one > 0.0);
    }

    #[test]
    fn heat_and_shading_reduce_power() {
        let mut p = plane();
        p.ac_max = 1e9;
        let cool = plane_ac_power(&p, sun(35.0), weather(800.0, 700.0, 100.0, 0.0), 172, 1.0);
        let hot = plane_ac_power(&p, sun(35.0), weather(800.0, 700.0, 100.0, 35.0), 172, 1.0);
        assert!(hot < cool);
        let w = weather(800.0, 700.0, 100.0, 15.0);
        let full = plane_ac_power(&p, sun(35.0), w, 172, 1.0);
        let shaded = plane_ac_power(&p, sun(35.0), w, 172, 0.0);
        let clamped = plane_ac_power(&p, sun(35.0), w, 172, -1.0);
        assert!(
            shaded < full && shaded > 0.0,
            "diffuse remains when the beam is shaded"
        );
        assert_eq!(shaded, clamped);
        assert_eq!(plane_ac_power(&p, sun(35.0), w, 172, 2.0), full);
    }

    #[test]
    fn inverter_id_round_trips_and_is_optional() {
        let p = plane();
        assert_eq!(p.inverter, None);
        assert!(!serde_json::to_string(&p).unwrap().contains("inverter\""));
        let q: PlaneParams = serde_json::from_str(
            r#"{"azimuth": 90, "tilt": 30, "pdc0": 400, "ac_max": 300, "inverter": 2, "label": "x"}"#,
        )
        .unwrap();
        assert_eq!(q.inverter, Some(2));
        let back: PlaneParams = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(back, q);
        assert_eq!(PlaneParams::new(180.0, 35.0, 400.0, 300.0), p);
    }

    #[test]
    fn ac_is_dc_through_the_inverter() {
        let mut p = plane();
        p.ac_max = 1e9;
        let w = weather(800.0, 700.0, 100.0, 15.0);
        let dc = plane_dc_power(&p, sun(35.0), w, 172, 1.0);
        assert!(dc > 0.0);
        assert!((plane_ac_power(&p, sun(35.0), w, 172, 1.0) - dc * p.inverter_eff).abs() < 1e-9);
        assert_eq!(plane_dc_power(&p, sun(100.0), w, 172, 1.0), 0.0);
    }

    #[test]
    fn separate_inverters_clip_per_plane() {
        let planes = [
            PlaneParams::new(90.0, 30.0, 1000.0, 500.0),
            PlaneParams::new(270.0, 30.0, 1000.0, 500.0),
        ];
        // 800 W DC on one plane, 100 W on the other.
        let ac = combine_ac(&planes, &[800.0, 100.0]);
        assert!((ac - (500.0 + 100.0 * 0.965)).abs() < 1e-9, "{ac}");
        assert_eq!(combine_ac(&[], &[]), 0.0);
        assert_eq!(
            combine_ac(&planes, &[-5.0, 0.0]),
            0.0,
            "negative DC counts as zero"
        );
    }

    #[test]
    fn shared_inverter_clips_the_sum() {
        let mut planes = vec![
            PlaneParams::new(90.0, 30.0, 1000.0, 1000.0),
            PlaneParams::new(270.0, 30.0, 1000.0, 1000.0),
        ];
        for p in &mut planes {
            p.inverter = Some(0);
        }
        // Below the shared limit: plain sum.
        assert!((combine_ac(&planes, &[300.0, 400.0]) - 700.0 * 0.965).abs() < 1e-9);
        // Above it: one clip, although each plane alone is below the limit.
        assert_eq!(combine_ac(&planes, &[800.0, 600.0]), 1000.0);
        // A third plane on its own inverter adds separately.
        planes.push(PlaneParams::new(180.0, 30.0, 500.0, 200.0));
        assert_eq!(combine_ac(&planes, &[800.0, 600.0, 500.0]), 1200.0);
    }

    #[test]
    fn system_power_equals_the_sum_without_shared_inverters() {
        let w = weather(800.0, 700.0, 100.0, 15.0);
        let planes = [
            PlaneParams::new(135.0, 30.0, 2000.0, 1500.0),
            PlaneParams::new(225.0, 20.0, 1500.0, 1000.0),
        ];
        let sum: f64 = planes
            .iter()
            .map(|p| plane_ac_power(p, sun(35.0), w, 172, 1.0))
            .sum();
        assert!((system_ac_power(&planes, sun(35.0), w, 172) - sum).abs() < 1e-9);
    }

    #[test]
    fn validation() {
        let mut planes = vec![
            PlaneParams::new(90.0, 30.0, 1000.0, 1000.0),
            PlaneParams::new(270.0, 30.0, 1000.0, 900.0),
        ];
        assert!(validate_planes(&planes).is_ok(), "separate inverters may differ");
        planes[0].inverter = Some(1);
        planes[1].inverter = Some(1);
        let err = validate_planes(&planes).unwrap_err();
        assert!(err.contains("inverter 1"), "{err}");
        planes[1].ac_max = 1000.0;
        assert!(validate_planes(&planes).is_ok());
        planes[1].pdc0 = f64::NAN;
        assert!(validate_planes(&planes).is_err());
        planes[1].pdc0 = -1.0;
        assert!(validate_planes(&planes).is_err());
        assert!(validate_planes(&[]).is_ok());
    }

    #[test]
    fn beam_on_a_plane_facing_the_sun() {
        let p = plane();
        let irr = poa(&p, sun(35.0), weather(0.0, 800.0, 0.0, 15.0), 172);
        // Sun perpendicular to the plane: no incidence loss.
        assert!((irr.beam - 800.0).abs() < 1e-9 && irr.sky_diffuse == 0.0 && irr.ground == 0.0);
    }
}
