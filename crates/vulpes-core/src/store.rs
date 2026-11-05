//! SQLite-backed storage for measured series, forecasts (external and our own)
//! and per-model change points. Timestamps are Unix seconds (UTC); a sample at
//! `ts` with step `step_s` covers `[ts, ts + step_s)`.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS series (
    id   INTEGER PRIMARY KEY,
    key  TEXT NOT NULL UNIQUE,
    unit TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('mean', 'sum'))
);
CREATE TABLE IF NOT EXISTS samples (
    series_id INTEGER NOT NULL REFERENCES series(id),
    step_s    INTEGER NOT NULL,
    ts        INTEGER NOT NULL,
    value     REAL    NOT NULL,
    PRIMARY KEY (series_id, step_s, ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS forecasts (
    source    TEXT    NOT NULL,
    var       TEXT    NOT NULL,
    valid_ts  INTEGER NOT NULL,
    issued_ts INTEGER NOT NULL,
    value     REAL    NOT NULL,
    PRIMARY KEY (source, var, valid_ts, issued_ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS change_points (
    model TEXT    NOT NULL,
    ts    INTEGER NOT NULL,
    note  TEXT    NOT NULL DEFAULT '',
    PRIMARY KEY (model, ts)
);
";

/// How a series aggregates over a step: `Mean` for power/levels/prices,
/// `Sum` for energy or other quantities accumulated within the step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesKind {
    Mean,
    Sum,
}

impl SeriesKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mean => "mean",
            Self::Sum => "sum",
        }
    }
}

/// One forecast value: what `source` predicted at `issued_ts` for `valid_ts`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForecastPoint {
    pub valid_ts: i64,
    pub issued_ts: i64,
    pub value: f64,
}

/// Coverage summary of one stored series at one step size.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesInfo {
    pub key: String,
    pub unit: String,
    pub kind: SeriesKind,
    pub step_s: i64,
    pub count: i64,
    pub first_ts: i64,
    pub last_ts: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> crate::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> crate::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> crate::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        let version: Option<i64> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok());
        match version {
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(v) => {
                return Err(crate::Error::Config(format!(
                    "store schema version {v} is not supported (expected {SCHEMA_VERSION})"
                )));
            }
        }
        Ok(Self { conn })
    }

    /// Get or create a series; the unit and kind of an existing series must match.
    pub fn ensure_series(&self, key: &str, unit: &str, kind: SeriesKind) -> crate::Result<i64> {
        let existing: Option<(i64, String, String)> = self
            .conn
            .query_row(
                "SELECT id, unit, kind FROM series WHERE key = ?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match existing {
            Some((id, u, k)) if u == unit && k == kind.as_str() => Ok(id),
            Some((_, u, k)) => Err(crate::Error::Config(format!(
                "series '{key}' exists as {k} [{u}], not {} [{unit}]",
                kind.as_str()
            ))),
            None => {
                self.conn.execute(
                    "INSERT INTO series (key, unit, kind) VALUES (?1, ?2, ?3)",
                    params![key, unit, kind.as_str()],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
        }
    }

    fn series_id(&self, key: &str) -> crate::Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM series WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Insert or replace samples of an existing series. Returns rows written.
    pub fn put_samples(
        &mut self,
        key: &str,
        step_s: i64,
        samples: &[(i64, f64)],
    ) -> crate::Result<usize> {
        let id = self
            .series_id(key)?
            .ok_or_else(|| crate::Error::Config(format!("unknown series '{key}'")))?;
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO samples (series_id, step_s, ts, value) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for &(ts, v) in samples.iter().filter(|(_, v)| v.is_finite()) {
                stmt.execute(params![id, step_s, ts, v])?;
            }
        }
        tx.commit()?;
        Ok(samples.iter().filter(|(_, v)| v.is_finite()).count())
    }

    /// Samples with `from <= ts < to`, ordered by time.
    pub fn samples(
        &self,
        key: &str,
        step_s: i64,
        from: i64,
        to: i64,
    ) -> crate::Result<Vec<(i64, f64)>> {
        let Some(id) = self.series_id(key)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts, value FROM samples WHERE series_id = ?1 AND step_s = ?2 AND ts >= ?3 AND ts < ?4 ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![id, step_s, from, to], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn put_forecasts(
        &mut self,
        source: &str,
        var: &str,
        points: &[ForecastPoint],
    ) -> crate::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO forecasts (source, var, valid_ts, issued_ts, value) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for p in points.iter().filter(|p| p.value.is_finite()) {
                stmt.execute(params![source, var, p.valid_ts, p.issued_ts, p.value])?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// For each `valid_ts` in `[from, to)`, the value from the most recent
    /// forecast issued at or before `asof`. This is what was known at `asof`.
    pub fn forecasts_asof(
        &self,
        source: &str,
        var: &str,
        from: i64,
        to: i64,
        asof: i64,
    ) -> crate::Result<Vec<(i64, f64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.valid_ts, f.value FROM forecasts f
             WHERE f.source = ?1 AND f.var = ?2 AND f.valid_ts >= ?3 AND f.valid_ts < ?4
               AND f.issued_ts = (SELECT MAX(g.issued_ts) FROM forecasts g
                                  WHERE g.source = f.source AND g.var = f.var
                                    AND g.valid_ts = f.valid_ts AND g.issued_ts <= ?5)
             ORDER BY f.valid_ts",
        )?;
        let rows = stmt.query_map(params![source, var, from, to, asof], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn add_change_point(&self, model: &str, ts: i64, note: &str) -> crate::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO change_points (model, ts, note) VALUES (?1, ?2, ?3)",
            params![model, ts, note],
        )?;
        Ok(())
    }

    /// The most recent change point for `model` at or before `asof`, if any.
    pub fn last_change_point(&self, model: &str, asof: i64) -> crate::Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MAX(ts) FROM change_points WHERE model = ?1 AND ts <= ?2",
            params![model, asof],
            |r| r.get(0),
        )?)
    }

    pub fn summary(&self) -> crate::Result<Vec<SeriesInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.key, s.unit, s.kind, x.step_s, COUNT(*), MIN(x.ts), MAX(x.ts)
             FROM series s JOIN samples x ON x.series_id = s.id
             GROUP BY s.id, x.step_s ORDER BY s.key, x.step_s",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SeriesInfo {
                key: r.get(0)?,
                unit: r.get(1)?,
                kind: if r.get::<_, String>(2)? == "sum" {
                    SeriesKind::Sum
                } else {
                    SeriesKind::Mean
                },
                step_s: r.get(3)?,
                count: r.get(4)?,
                first_ts: r.get(5)?,
                last_ts: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_roundtrip_and_upsert() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_series("pv.power", "W", SeriesKind::Mean).unwrap();
        s.put_samples("pv.power", 3600, &[(0, 1.0), (3600, 2.0), (7200, f64::NAN)])
            .unwrap();
        s.put_samples("pv.power", 3600, &[(3600, 5.0)]).unwrap();
        assert_eq!(
            s.samples("pv.power", 3600, 0, 10_000).unwrap(),
            vec![(0, 1.0), (3600, 5.0)]
        );
        assert!(s.samples("pv.power", 900, 0, 10_000).unwrap().is_empty());
        assert!(s.samples("missing", 3600, 0, 10).unwrap().is_empty());
    }

    #[test]
    fn series_kind_conflict_is_rejected() {
        let s = Store::open_in_memory().unwrap();
        s.ensure_series("x", "W", SeriesKind::Mean).unwrap();
        assert!(s.ensure_series("x", "Wh", SeriesKind::Sum).is_err());
        assert!(s.ensure_series("x", "W", SeriesKind::Mean).is_ok());
    }

    #[test]
    fn forecasts_asof_picks_latest_issue_before_cutoff() {
        let mut s = Store::open_in_memory().unwrap();
        let p = |valid_ts, issued_ts, value| ForecastPoint {
            valid_ts,
            issued_ts,
            value,
        };
        s.put_forecasts(
            "om",
            "ghi",
            &[
                p(100, 0, 1.0),
                p(100, 50, 2.0),
                p(100, 90, 3.0),
                p(200, 10, 7.0),
            ],
        )
        .unwrap();
        assert_eq!(
            s.forecasts_asof("om", "ghi", 0, 300, 60).unwrap(),
            vec![(100, 2.0), (200, 7.0)]
        );
        assert_eq!(
            s.forecasts_asof("om", "ghi", 0, 300, 5).unwrap(),
            vec![(100, 1.0)]
        );
        assert!(
            s.forecasts_asof("om", "ghi", 0, 300, -1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn change_points() {
        let s = Store::open_in_memory().unwrap();
        s.add_change_point("solar", 1000, "new panels").unwrap();
        s.add_change_point("solar", 5000, "tree cut").unwrap();
        assert_eq!(s.last_change_point("solar", 4000).unwrap(), Some(1000));
        assert_eq!(s.last_change_point("solar", 999).unwrap(), None);
        assert_eq!(s.last_change_point("load", 9999).unwrap(), None);
    }
}
