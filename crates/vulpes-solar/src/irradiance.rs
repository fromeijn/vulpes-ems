//! Irradiance helpers: extraterrestrial radiation, air mass, angle of
//! incidence and Perez (1990) transposition to a tilted plane.

/// Extraterrestrial normal irradiance (W/m²), Spencer (1971), solar constant 1366.1 W/m².
pub fn extra_dni(day_of_year: u32) -> f64 {
    let b = std::f64::consts::TAU * (day_of_year as f64 - 1.0) / 365.0;
    1366.1
        * (1.000_11
            + 0.034_221 * b.cos()
            + 0.001_28 * b.sin()
            + 0.000_719 * (2.0 * b).cos()
            + 0.000_077 * (2.0 * b).sin())
}

/// Relative optical air mass (Kasten & Young, 1989); `None` with the sun down.
pub fn airmass(zenith_deg: f64) -> Option<f64> {
    (zenith_deg < 90.0)
        .then(|| 1.0 / (zenith_deg.to_radians().cos() + 0.505_72 * (96.079_95 - zenith_deg).powf(-1.6364)))
}

/// Cosine of the angle of incidence between the sun and a plane.
pub fn cos_aoi(tilt: f64, surface_azimuth: f64, zenith: f64, sun_azimuth: f64) -> f64 {
    let (t, z) = (tilt.to_radians(), zenith.to_radians());
    z.cos() * t.cos() + z.sin() * t.sin() * (sun_azimuth - surface_azimuth).to_radians().cos()
}

/// Perez "allsitescomposite1990" coefficients: rows are sky-clearness bins,
/// columns are (constant, delta, zenith) terms.
const F1: [[f64; 3]; 8] = [
    [-0.008, 0.588, -0.062],
    [0.130, 0.683, -0.151],
    [0.330, 0.487, -0.221],
    [0.568, 0.187, -0.295],
    [0.873, -0.392, -0.362],
    [1.132, -1.237, -0.412],
    [1.060, -1.600, -0.359],
    [0.678, -0.327, -0.250],
];
const F2: [[f64; 3]; 8] = [
    [-0.060, 0.072, -0.022],
    [-0.019, 0.066, -0.029],
    [0.055, -0.064, -0.026],
    [0.109, -0.152, -0.014],
    [0.226, -0.462, 0.001],
    [0.288, -0.823, 0.056],
    [0.264, -1.127, 0.131],
    [0.156, -1.377, 0.251],
];
const EPS_BINS: [f64; 7] = [1.065, 1.23, 1.5, 1.95, 2.8, 4.5, 6.2];

/// Sky-diffuse irradiance on a tilted plane (W/m²), Perez 1990.
pub fn perez_sky_diffuse(
    tilt: f64,
    surface_azimuth: f64,
    dhi: f64,
    dni: f64,
    dni_extra: f64,
    zenith: f64,
    sun_azimuth: f64,
) -> f64 {
    let Some(am) = airmass(zenith) else {
        return 0.0;
    };
    if dhi <= 0.0 {
        return 0.0;
    }
    const KAPPA: f64 = 1.041;
    let z = zenith.to_radians();
    let eps = ((dhi + dni) / dhi + KAPPA * z.powi(3)) / (1.0 + KAPPA * z.powi(3));
    let bin = EPS_BINS.iter().take_while(|&&b| eps >= b).count();
    let delta = dhi * am / dni_extra;
    let f1 = (F1[bin][0] + F1[bin][1] * delta + F1[bin][2] * z).max(0.0);
    let f2 = F2[bin][0] + F2[bin][1] * delta + F2[bin][2] * z;
    let a = cos_aoi(tilt, surface_azimuth, zenith, sun_azimuth).max(0.0);
    let b = z.cos().max(85f64.to_radians().cos());
    let t = tilt.to_radians();
    let term1 = 0.5 * (1.0 - f1) * (1.0 + t.cos());
    let term2 = f1 * a / b;
    let term3 = f2 * t.sin();
    (dhi * (term1 + term2 + term3)).max(0.0)
}

/// Ground-reflected irradiance on a tilted plane (W/m²).
pub fn ground_diffuse(tilt: f64, ghi: f64, albedo: f64) -> f64 {
    ghi * albedo * (1.0 - tilt.to_radians().cos()) * 0.5
}
