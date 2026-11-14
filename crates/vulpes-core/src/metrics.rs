//! Forecast error accumulation: MAE, RMSE and bias over forecast/actual pairs.
//! Shared by the CLI evaluations and the integration's review.

/// Running sums of forecast errors (`forecast − actual`).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ErrorStats {
    /// Number of pairs.
    pub n: usize,
    /// Sum of absolute errors.
    pub abs: f64,
    /// Sum of squared errors.
    pub sq: f64,
    /// Sum of signed errors (positive = forecast too high).
    pub sum: f64,
}

impl ErrorStats {
    /// Add one pair. Non-finite values are skipped, so the sums stay NaN-free.
    pub fn add(&mut self, forecast: f64, actual: f64) {
        let e = forecast - actual;
        if !e.is_finite() {
            return;
        }
        self.n += 1;
        self.abs += e.abs();
        self.sq += e * e;
        self.sum += e;
    }

    /// Combine with the sums of another set of pairs.
    pub fn merge(&mut self, other: &Self) {
        self.n += other.n;
        self.abs += other.abs;
        self.sq += other.sq;
        self.sum += other.sum;
    }

    /// Mean absolute error, or `None` without pairs.
    pub fn mae(&self) -> Option<f64> {
        self.mean(self.abs)
    }

    /// Root mean squared error, or `None` without pairs.
    pub fn rmse(&self) -> Option<f64> {
        self.mean(self.sq).map(f64::sqrt)
    }

    /// Mean signed error (positive = forecast too high), or `None` without pairs.
    pub fn bias(&self) -> Option<f64> {
        self.mean(self.sum)
    }

    fn mean(&self, total: f64) -> Option<f64> {
        (self.n > 0).then(|| total / self.n as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_and_means() {
        let mut s = ErrorStats::default();
        s.add(110.0, 100.0); // +10
        s.add(80.0, 100.0); // −20
        assert_eq!((s.n, s.abs, s.sq, s.sum), (2, 30.0, 500.0, -10.0));
        assert_eq!(s.mae(), Some(15.0));
        assert_eq!(s.rmse(), Some(250f64.sqrt()));
        assert_eq!(s.bias(), Some(-5.0));
    }

    #[test]
    fn empty_has_no_means() {
        let s = ErrorStats::default();
        assert_eq!((s.mae(), s.rmse(), s.bias()), (None, None, None));
    }

    #[test]
    fn single_perfect_pair_is_zero() {
        let mut s = ErrorStats::default();
        s.add(0.0, 0.0);
        assert_eq!((s.mae(), s.rmse(), s.bias()), (Some(0.0), Some(0.0), Some(0.0)));
    }

    #[test]
    fn non_finite_pairs_are_skipped() {
        let mut s = ErrorStats::default();
        s.add(f64::NAN, 1.0);
        s.add(1.0, f64::INFINITY);
        s.add(2.0, 1.0);
        assert_eq!(s.n, 1);
        assert!(s.mae().unwrap().is_finite());
    }

    #[test]
    fn merge_equals_adding_all_pairs() {
        let pairs = [(1.0, 2.0), (5.0, 3.0), (-1.0, 0.5), (4.0, 4.0)];
        let mut all = ErrorStats::default();
        let (mut a, mut b) = (ErrorStats::default(), ErrorStats::default());
        for (i, &(f, x)) in pairs.iter().enumerate() {
            all.add(f, x);
            if i < 2 { a.add(f, x) } else { b.add(f, x) }
        }
        a.merge(&b);
        assert_eq!(a, all);
    }
}
