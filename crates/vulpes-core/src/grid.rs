use serde::{Deserialize, Serialize};

/// A fixed-step planning grid. Timestamps are Unix seconds (UTC); slot `i`
/// covers `[start + i*step, start + (i+1)*step)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeGrid {
    pub start: i64,
    pub step_s: i64,
    pub len: usize,
}

impl TimeGrid {
    pub const QUARTER_HOUR: i64 = 15 * 60;

    /// Grid starting at the slot containing `now`, aligned to `step_s`.
    pub fn aligned(now: i64, step_s: i64, len: usize) -> crate::Result<Self> {
        if step_s <= 0 || len == 0 {
            return Err(crate::Error::Config(
                "step and length must be positive".into(),
            ));
        }
        Ok(Self {
            start: now.div_euclid(step_s) * step_s,
            step_s,
            len,
        })
    }

    pub fn slot_start(&self, i: usize) -> i64 {
        self.start + i as i64 * self.step_s
    }

    /// Index of the slot containing `ts`, if inside the grid.
    pub fn index_of(&self, ts: i64) -> Option<usize> {
        let off = ts - self.start;
        if off < 0 {
            return None;
        }
        let i = (off / self.step_s) as usize;
        (i < self.len).then_some(i)
    }

    pub fn step_hours(&self) -> f64 {
        self.step_s as f64 / 3600.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_to_quarter_hour() {
        let g = TimeGrid::aligned(1_000_000_123, TimeGrid::QUARTER_HOUR, 192).unwrap();
        assert_eq!(g.start % 900, 0);
        assert!(g.start <= 1_000_000_123 && 1_000_000_123 < g.start + 900);
        assert_eq!(g.index_of(g.start + 900 * 5 + 1), Some(5));
        assert_eq!(g.index_of(g.start - 1), None);
        assert_eq!(g.index_of(g.slot_start(192)), None);
    }

    #[test]
    fn rejects_bad_params() {
        assert!(TimeGrid::aligned(0, 0, 10).is_err());
        assert!(TimeGrid::aligned(0, 900, 0).is_err());
    }
}
