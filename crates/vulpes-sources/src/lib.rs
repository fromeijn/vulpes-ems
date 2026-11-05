//! External data sources: Open-Meteo weather forecasts (live and archived)
//! and EnergyZero day-ahead prices (NL; interim until ENTSO-E support).
//!
//! Privacy: locations are rounded to 0.01° (~1 km) before being sent anywhere.

pub mod energyzero;
pub mod open_meteo;

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] ureq::Error),
    #[error("unexpected response: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A geographic location. Use [`Location::rounded`] before sending it to a service.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
}

impl Location {
    /// Rounded to 0.01° (~1 km): plenty for weather models (1-2 km grids),
    /// without disclosing an exact address.
    pub fn rounded(self) -> Self {
        let r = |x: f64| (x * 100.0).round() / 100.0;
        Self {
            lat: r(self.lat),
            lon: r(self.lon),
        }
    }
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(concat!("vulpes-ems/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_location() {
        let l = Location {
            lat: 52.123_456,
            lon: 4.987_654,
        }
        .rounded();
        assert_eq!((l.lat, l.lon), (52.12, 4.99));
    }
}
