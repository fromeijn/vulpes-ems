//! Reference values generated with pvlib 0.16.1 (NREL SPA for solar position).
//! Locations are generic test points, not any user's site.

use vulpes_solar::irradiance::{extra_dni, perez_sky_diffuse};
use vulpes_solar::model::{cell_temp, iam_ashrae};
use vulpes_solar::sun_position;

#[test]
fn solar_position_matches_spa() {
    // (ts, lat, lon, zenith, azimuth)
    let cases = [
        (1750506000.0, 52.0, 5.0, 28.56545, 179.11039),
        (1766318400.0, 52.0, 5.0, 75.59191, 185.16571),
        (1742454900.0, 52.0, 5.0, 76.75341, 107.59092),
        (1757521800.0, 52.0, 5.0, 76.09216, 259.56468),
        (1768503600.0, 39.74, -105.18, 60.80294, 177.26901),
        (1720101600.0, 39.74, -105.18, 65.45930, 79.96400),
    ];
    for (ts, lat, lon, z, az) in cases {
        let s = sun_position(ts, lat, lon);
        assert!((s.zenith - z).abs() < 0.02, "zenith {} vs {z} at {ts}", s.zenith);
        assert!(
            (s.azimuth - az).abs() < 0.03,
            "azimuth {} vs {az} at {ts}",
            s.azimuth
        );
    }
}

#[test]
fn perez_matches_pvlib() {
    // (tilt, surface_azimuth, dhi, dni, day_of_year, zenith, sun_azimuth, sky_diffuse)
    let cases = [
        (30.0, 180.0, 120.0, 700.0, 172, 35.0, 190.0, 147.230853),
        (45.0, 45.0, 300.0, 50.0, 80, 70.0, 120.0, 278.108608),
        (20.0, 225.0, 80.0, 850.0, 200, 30.0, 170.0, 86.306125),
        (60.0, 135.0, 400.0, 0.0, 350, 80.0, 150.0, 1811.268465),
        (35.0, 90.0, 150.0, 400.0, 100, 60.0, 100.0, 201.540503),
    ];
    for (tilt, saz, dhi, dni, doy, z, az, expected) in cases {
        let got = perez_sky_diffuse(tilt, saz, dhi, dni, extra_dni(doy), z, az);
        assert!(
            (got - expected).abs() < 1e-3 * expected.max(1.0),
            "perez {got} vs {expected}"
        );
    }
}

#[test]
fn extraterrestrial_matches_spencer() {
    for (doy, expected) in [
        (1, 1413.981805),
        (100, 1360.789792),
        (172, 1321.623593),
        (300, 1383.636203),
    ] {
        assert!((extra_dni(doy) - expected).abs() < 1e-3);
    }
}

#[test]
fn iam_and_cell_temperature_match() {
    for (aoi, expected) in [
        (0.0f64, 1.0),
        (30.0, 0.992265),
        (60.0, 0.95),
        (80.0, 0.762061),
        (89.0, 0.0),
    ] {
        assert!(
            (iam_ashrae(aoi.to_radians().cos(), 0.05) - expected).abs() < 1e-5,
            "iam at {aoi}"
        );
    }
    for (poa, ta, ws, expected) in [
        (800.0, 20.0, 2.0, 40.682523),
        (1000.0, 30.0, 0.5, 65.186488),
        (300.0, 5.0, 6.0, 9.542701),
    ] {
        assert!((cell_temp(poa, ta, ws, 25.0, 6.84) - expected).abs() < 1e-5);
    }
}
