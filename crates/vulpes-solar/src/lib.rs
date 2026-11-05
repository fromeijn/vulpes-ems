//! Physics-based PV forecasting: solar position, plane-of-array irradiance
//! (Perez), incidence-angle and temperature losses, inverter clipping.
//! Machine-learned corrections are layered on top in later phases.

pub mod calibrate;
pub mod irradiance;
pub mod model;
pub mod shading;
pub mod solpos;

pub use model::{PlaneParams, Weather, plane_ac_power};
pub use solpos::{SunPosition, sun_position};
