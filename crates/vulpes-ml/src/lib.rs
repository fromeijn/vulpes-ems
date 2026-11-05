//! Shared ML utilities: weighted ridge regression and recency weights.

pub mod ridge;

/// Weight for a sample `age_days` old with the given half-life.
pub fn recency_weight(age_days: f64, half_life_days: f64) -> f64 {
    0.5f64.powf(age_days.max(0.0) / half_life_days)
}
