//! External data sources: Open-Meteo weather forecasts (live and archived)
//! and EnergyZero day-ahead prices (NL; interim until ENTSO-E support).
//!
//! Privacy: locations are rounded to 0.01° (~1 km) before being sent anywhere.

pub mod energyzero;
pub mod entsoe;
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

/// The longest a whole request may take. The largest request (120 days of
/// 15-minute weather for the solar calibration) takes about 2 s.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// The longest connecting may take, over all of the host's addresses. ureq
/// tries them in turn and gives the first two thirds of this when there are
/// two, so a broken IPv6 route costs about 4 s before IPv4 is tried, instead
/// of most of [`TIMEOUT`].
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
// With an IPv6 and an IPv4 address, IPv4 still gets at least a second.
const _: () = assert!(CONNECT_TIMEOUT.as_secs() / 3 >= 1 && CONNECT_TIMEOUT.as_secs() < TIMEOUT.as_secs());

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .user_agent(concat!("vulpes-ems/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_gives_up_on_a_hanging_address() {
        let t = agent().config().timeouts();
        assert_eq!((t.global, t.connect), (Some(TIMEOUT), Some(CONNECT_TIMEOUT)));
    }

    #[test]
    fn rounds_location() {
        let l = Location {
            lat: 52.123_456,
            lon: 4.987_654,
        }
        .rounded();
        assert_eq!((l.lat, l.lon), (52.12, 4.99));
    }

    #[test]
    fn rounding_handles_negative_coordinates() {
        let r = Location {
            lat: -33.86882,
            lon: -151.20929,
        }
        .rounded();
        assert_eq!((r.lat, r.lon), (-33.87, -151.21));
        let same = Location { lat: 52.0, lon: 5.0 };
        assert_eq!(same.rounded(), same);
    }
}
