//! Weighted ridge regression: minimise Σ wᵢ (yᵢ − β·xᵢ)² + λ‖β‖², solved via
//! the normal equations with Gaussian elimination (feature counts are small).
//! Add a constant-1 feature for an intercept (it is penalised like the rest).

#[derive(Debug, Clone, PartialEq)]
pub struct Ridge {
    pub coef: Vec<f64>,
}

impl Ridge {
    pub fn predict(&self, x: &[f64]) -> f64 {
        self.coef.iter().zip(x).map(|(b, v)| b * v).sum()
    }

    /// `rows` are `(features, target, weight)`. Returns `None` if there are no
    /// rows or the system is singular.
    pub fn fit(rows: &[(Vec<f64>, f64, f64)], lambda: f64) -> Option<Self> {
        let k = rows.first()?.0.len();
        let mut a = vec![vec![0.0; k + 1]; k]; // augmented [XᵀWX + λI | XᵀWy]
        for (x, y, w) in rows {
            for i in 0..k {
                for j in 0..k {
                    a[i][j] += w * x[i] * x[j];
                }
                a[i][k] += w * x[i] * y;
            }
        }
        for (i, row) in a.iter_mut().enumerate() {
            row[i] += lambda;
        }
        // Gaussian elimination with partial pivoting.
        for col in 0..k {
            let piv = (col..k).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
            if a[piv][col].abs() < 1e-12 {
                return None;
            }
            a.swap(col, piv);
            let pivot_row = a[col].clone();
            for (r, row) in a.iter_mut().enumerate() {
                if r != col {
                    let f = row[col] / pivot_row[col];
                    for (v, p) in row.iter_mut().zip(&pivot_row).skip(col) {
                        *v -= f * p;
                    }
                }
            }
        }
        Some(Self {
            coef: (0..k).map(|i| a[i][k] / a[i][i]).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_linear_model() {
        let rows: Vec<_> = (0..100)
            .map(|i| {
                let (a, b) = ((i % 7) as f64, (i % 11) as f64 - 5.0);
                (vec![1.0, a, b], 2.0 + 3.0 * a - 1.5 * b, 1.0)
            })
            .collect();
        let m = Ridge::fit(&rows, 1e-9).unwrap();
        for (got, want) in m.coef.iter().zip([2.0, 3.0, -1.5]) {
            assert!((got - want).abs() < 1e-6, "{:?}", m.coef);
        }
    }

    #[test]
    fn weights_select_the_regime() {
        // Two regimes; weight only the second.
        let mut rows: Vec<_> = (0..50).map(|i| (vec![i as f64], 1.0 * i as f64, 0.0)).collect();
        rows.extend((0..50).map(|i| (vec![i as f64], 4.0 * i as f64, 1.0)));
        let m = Ridge::fit(&rows, 1e-9).unwrap();
        assert!((m.coef[0] - 4.0).abs() < 1e-6);
    }

    #[test]
    fn ridge_shrinks() {
        let rows = vec![(vec![1.0], 10.0, 1.0)];
        let m = Ridge::fit(&rows, 1.0).unwrap();
        assert!((m.coef[0] - 5.0).abs() < 1e-9);
    }
}
