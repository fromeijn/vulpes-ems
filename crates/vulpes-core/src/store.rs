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
CREATE TABLE IF NOT EXISTS plan_slots (
    ts             INTEGER PRIMARY KEY,
    mode           TEXT,
    import_price   REAL,
    export_price   REAL,
    terminal_value REAL,
    active         INTEGER NOT NULL DEFAULT 0,
    followed       INTEGER NOT NULL DEFAULT 0,
    user_mode      TEXT
);
";

/// Keys of series that the CLI tools and the integration's history share.
pub mod keys {
    /// House load excluding the battery (W, hourly mean).
    pub const LOAD_HOUSE: &str = "load.house";
    /// Total AC PV power (W, hourly mean).
    pub const PV_TOTAL_POWER: &str = "pv.total.power";
    /// Day-ahead wholesale price for the Netherlands (EUR/kWh, hourly mean).
    pub const WHOLESALE_NL: &str = "price.wholesale.nl";
}

/// What happened in one 15-minute slot: the planned mode and prices as the
/// plan had them, whether Vulpes was in control and followed the plan, and
/// the state of the user's own mode entity (for agreement).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlanSlotRecord {
    pub ts: i64,
    /// Full engine mode name (`hold:no_export`), if a plan was available.
    pub mode: Option<String>,
    pub import_price: Option<f64>,
    pub export_price: Option<f64>,
    /// Value per kWh stored at the end of the plan (currency/kWh).
    pub terminal_value: Option<f64>,
    /// The Active switch was on (a control script ran).
    pub active: bool,
    /// The script ran the planned command (not the safe fallback) without error.
    pub followed: bool,
    /// The Vulpes mode the user's mode entity showed, if one is mapped.
    pub user_mode: Option<String>,
}

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
        // The integration and a copy-run CLI may touch the file at the same time.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
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
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| {
                r.get::<_, String>(0)
            })
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
            .query_row("SELECT id, unit, kind FROM series WHERE key = ?1", [key], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
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
    pub fn put_samples(&mut self, key: &str, step_s: i64, samples: &[(i64, f64)]) -> crate::Result<usize> {
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
    pub fn samples(&self, key: &str, step_s: i64, from: i64, to: i64) -> crate::Result<Vec<(i64, f64)>> {
        let Some(id) = self.series_id(key)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts, value FROM samples WHERE series_id = ?1 AND step_s = ?2 AND ts >= ?3 AND ts < ?4 ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![id, step_s, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
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

    /// For each `valid_ts` in `[from, to)`, the value from the most recent
    /// forecast issued at least `lead_s` seconds before it: what was known
    /// `lead_s` ahead. A `valid_ts` without such a forecast is left out.
    pub fn forecasts_at_lead(
        &self,
        source: &str,
        var: &str,
        from: i64,
        to: i64,
        lead_s: i64,
    ) -> crate::Result<Vec<(i64, f64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.valid_ts, f.value FROM forecasts f
             WHERE f.source = ?1 AND f.var = ?2 AND f.valid_ts >= ?3 AND f.valid_ts < ?4
               AND f.issued_ts = (SELECT MAX(g.issued_ts) FROM forecasts g
                                  WHERE g.source = f.source AND g.var = f.var
                                    AND g.valid_ts = f.valid_ts AND g.issued_ts <= f.valid_ts - ?5)
             ORDER BY f.valid_ts",
        )?;
        let rows = stmt.query_map(params![source, var, from, to, lead_s], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Insert or replace the record of one slot.
    pub fn put_plan_slot(&self, r: &PlanSlotRecord) -> crate::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO plan_slots
                 (ts, mode, import_price, export_price, terminal_value, active, followed, user_mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                r.ts,
                r.mode,
                r.import_price.filter(|v| v.is_finite()),
                r.export_price.filter(|v| v.is_finite()),
                r.terminal_value.filter(|v| v.is_finite()),
                r.active,
                r.followed,
                r.user_mode
            ])?;
        Ok(())
    }

    /// Slot records with `from <= ts < to`, ordered by time.
    pub fn plan_slots(&self, from: i64, to: i64) -> crate::Result<Vec<PlanSlotRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts, mode, import_price, export_price, terminal_value, active, followed, user_mode
             FROM plan_slots WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![from, to], |r| {
            Ok(PlanSlotRecord {
                ts: r.get(0)?,
                mode: r.get(1)?,
                import_price: r.get(2)?,
                export_price: r.get(3)?,
                terminal_value: r.get(4)?,
                active: r.get(5)?,
                followed: r.get(6)?,
                user_mode: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A value from the key/value `meta` table.
    pub fn meta(&self, key: &str) -> crate::Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Set a value in the `meta` table. `schema_version` is managed by the store.
    pub fn set_meta(&self, key: &str, value: &str) -> crate::Result<()> {
        if key == "schema_version" {
            return Err(crate::Error::Config(
                "schema_version is managed by the store".into(),
            ));
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Delete forecasts valid before `before`. Returns rows deleted.
    pub fn prune_forecasts(&self, source: &str, before: i64) -> crate::Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM forecasts WHERE source = ?1 AND valid_ts < ?2",
            params![source, before],
        )?)
    }

    /// Delete samples of one series before `before`. Returns rows deleted.
    pub fn prune_samples(&self, key: &str, before: i64) -> crate::Result<usize> {
        let Some(id) = self.series_id(key)? else {
            return Ok(0);
        };
        Ok(self.conn.execute(
            "DELETE FROM samples WHERE series_id = ?1 AND ts < ?2",
            params![id, before],
        )?)
    }

    /// Delete slot records before `before`. Returns rows deleted.
    pub fn prune_plan_slots(&self, before: i64) -> crate::Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM plan_slots WHERE ts < ?1", [before])?)
    }

    /// Write the write-ahead log back into the database file, so a copy of
    /// the `.db` file alone (a backup) holds everything written so far.
    pub fn checkpoint(&self) -> crate::Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
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
            &[p(100, 0, 1.0), p(100, 50, 2.0), p(100, 90, 3.0), p(200, 10, 7.0)],
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
        assert!(s.forecasts_asof("om", "ghi", 0, 300, -1).unwrap().is_empty());
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

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vulpes-store-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("store.db")
    }

    #[test]
    fn data_persists_across_reopening_a_file() {
        let path = temp_db("persist");
        {
            let mut s = Store::open(&path).unwrap();
            s.ensure_series("x", "W", SeriesKind::Mean).unwrap();
            s.put_samples("x", 3600, &[(0, 1.0)]).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.samples("x", 3600, 0, 10).unwrap(), vec![(0, 1.0)]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let path = temp_db("schema");
        Store::open(&path).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute("UPDATE meta SET value = '99' WHERE key = 'schema_version'", [])
            .unwrap();
        let err = Store::open(&path)
            .err()
            .expect("version 99 must be rejected")
            .to_string();
        assert!(err.contains("schema version 99"), "{err}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn samples_edge_cases() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(
            s.put_samples("nope", 3600, &[(0, 1.0)]).is_err(),
            "unknown series"
        );
        assert!(s.samples("nope", 3600, 0, 10).unwrap().is_empty());
        s.ensure_series("x", "W", SeriesKind::Mean).unwrap();
        // Non-finite values are skipped and not counted.
        let n = s
            .put_samples(
                "x",
                3600,
                &[(0, 1.0), (3600, f64::NAN), (7200, f64::INFINITY), (10_800, 4.0)],
            )
            .unwrap();
        assert_eq!(n, 2);
        // The range is half-open, and steps are kept apart.
        s.put_samples("x", 900, &[(0, 9.0)]).unwrap();
        assert_eq!(s.samples("x", 3600, 0, 10_800).unwrap(), vec![(0, 1.0)]);
        assert_eq!(
            s.samples("x", 3600, 0, 10_801).unwrap(),
            vec![(0, 1.0), (10_800, 4.0)]
        );
        assert_eq!(s.samples("x", 900, 0, 10_801).unwrap(), vec![(0, 9.0)]);
        let info = s.summary().unwrap();
        assert_eq!(info.len(), 2, "one row per series and step");
    }

    #[test]
    fn forecasts_at_lead_picks_the_newest_issue_at_least_lead_before() {
        let mut s = Store::open_in_memory().unwrap();
        let p = |valid_ts, issued_ts, value| ForecastPoint {
            valid_ts,
            issued_ts,
            value,
        };
        let h = 3600;
        s.put_forecasts(
            "vulpes",
            "pv_w",
            &[
                // Hour 30: issued 30 h, 25 h and 1 h ahead.
                p(30 * h, 0, 1.0),
                p(30 * h, 5 * h, 2.0),
                p(30 * h, 29 * h, 3.0),
                // Hour 40: issued only 2 h ahead.
                p(40 * h, 38 * h, 9.0),
            ],
        )
        .unwrap();
        // Exactly 24 h ahead or more: the 25 h issue.
        assert_eq!(
            s.forecasts_at_lead("vulpes", "pv_w", 0, 50 * h, 24 * h).unwrap(),
            vec![(30 * h, 2.0)]
        );
        // 25 h ahead is still allowed (inclusive bound).
        assert_eq!(
            s.forecasts_at_lead("vulpes", "pv_w", 0, 50 * h, 25 * h).unwrap(),
            vec![(30 * h, 2.0)]
        );
        // 1 h ahead: the newest issue of both hours.
        assert_eq!(
            s.forecasts_at_lead("vulpes", "pv_w", 0, 50 * h, h).unwrap(),
            vec![(30 * h, 3.0), (40 * h, 9.0)]
        );
        // Half-open range, other variables and sources stay apart.
        assert!(
            s.forecasts_at_lead("vulpes", "pv_w", 0, 30 * h, h)
                .unwrap()
                .is_empty()
        );
        assert!(
            s.forecasts_at_lead("vulpes", "load_w", 0, 50 * h, h)
                .unwrap()
                .is_empty()
        );
        assert!(
            s.forecasts_at_lead("other", "pv_w", 0, 50 * h, h)
                .unwrap()
                .is_empty()
        );
        // Further ahead than any issue: nothing.
        assert!(
            s.forecasts_at_lead("vulpes", "pv_w", 0, 50 * h, 31 * h)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn plan_slots_roundtrip_upsert_and_prune() {
        let s = Store::open_in_memory().unwrap();
        let full = PlanSlotRecord {
            ts: 900,
            mode: Some("hold:no_export".into()),
            import_price: Some(0.25),
            export_price: Some(0.05),
            terminal_value: Some(0.2),
            active: true,
            followed: true,
            user_mode: Some("hold".into()),
        };
        // A slot without a plan and with a non-finite price stores nulls.
        let bare = PlanSlotRecord {
            ts: 0,
            import_price: Some(f64::NAN),
            ..Default::default()
        };
        s.put_plan_slot(&full).unwrap();
        s.put_plan_slot(&bare).unwrap();
        let got = s.plan_slots(0, 1800).unwrap();
        assert_eq!(
            got,
            vec![
                PlanSlotRecord {
                    import_price: None,
                    ..bare
                },
                full.clone()
            ]
        );
        // Replace by ts.
        s.put_plan_slot(&PlanSlotRecord {
            followed: false,
            ..full.clone()
        })
        .unwrap();
        assert!(!s.plan_slots(900, 901).unwrap()[0].followed);
        assert_eq!(s.prune_plan_slots(900).unwrap(), 1);
        assert_eq!(s.plan_slots(0, 1800).unwrap().len(), 1);
    }

    #[test]
    fn plan_slot_record_json_roundtrip() {
        let r = PlanSlotRecord {
            ts: 1,
            mode: Some("charge".into()),
            active: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<PlanSlotRecord>(&json).unwrap(), r);
    }

    #[test]
    fn meta_values_and_protected_schema_version() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.meta("review.since").unwrap(), None);
        s.set_meta("review.since", "2026-11-14").unwrap();
        s.set_meta("review.since", "2026-11-15").unwrap();
        assert_eq!(s.meta("review.since").unwrap().as_deref(), Some("2026-11-15"));
        assert!(s.set_meta("schema_version", "2").is_err());
        assert_eq!(s.meta("schema_version").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn pruning_keeps_newer_rows_and_other_series() {
        let mut s = Store::open_in_memory().unwrap();
        s.ensure_series("a", "W", SeriesKind::Mean).unwrap();
        s.ensure_series("b", "W", SeriesKind::Mean).unwrap();
        s.put_samples("a", 3600, &[(0, 1.0), (3600, 2.0)]).unwrap();
        s.put_samples("b", 3600, &[(0, 1.0)]).unwrap();
        assert_eq!(s.prune_samples("a", 3600).unwrap(), 1);
        assert_eq!(s.prune_samples("missing", 3600).unwrap(), 0);
        assert_eq!(s.samples("a", 3600, 0, 10_000).unwrap(), vec![(3600, 2.0)]);
        assert_eq!(s.samples("b", 3600, 0, 10_000).unwrap(), vec![(0, 1.0)]);
        let p = |valid_ts| ForecastPoint {
            valid_ts,
            issued_ts: 0,
            value: 1.0,
        };
        s.put_forecasts("vulpes", "x", &[p(0), p(7200)]).unwrap();
        s.put_forecasts("open-meteo", "x", &[p(0)]).unwrap();
        assert_eq!(s.prune_forecasts("vulpes", 3600).unwrap(), 1);
        assert_eq!(
            s.forecasts_asof("vulpes", "x", 0, 10_000, 0).unwrap(),
            vec![(7200, 1.0)]
        );
        assert_eq!(
            s.forecasts_asof("open-meteo", "x", 0, 10_000, 0).unwrap(),
            vec![(0, 1.0)]
        );
    }

    #[test]
    fn a_file_from_before_plan_slots_gains_the_table_and_keeps_version_1() {
        let path = temp_db("upgrade");
        {
            // A version-1 file as the 0.4.0 CLI wrote it: no plan_slots table.
            let mut s = Store::open(&path).unwrap();
            s.ensure_series("x", "W", SeriesKind::Mean).unwrap();
            s.put_samples("x", 3600, &[(0, 1.0)]).unwrap();
        }
        Connection::open(&path)
            .unwrap()
            .execute("DROP TABLE plan_slots", [])
            .unwrap();
        let s = Store::open(&path).unwrap();
        assert_eq!(s.samples("x", 3600, 0, 10).unwrap(), vec![(0, 1.0)]);
        assert!(s.plan_slots(0, 10).unwrap().is_empty());
        assert_eq!(s.meta("schema_version").unwrap().as_deref(), Some("1"));
        drop(s);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn checkpoint_leaves_a_complete_db_file() {
        let path = temp_db("checkpoint");
        let mut s = Store::open(&path).unwrap();
        s.ensure_series("x", "W", SeriesKind::Mean).unwrap();
        s.put_samples("x", 3600, &[(0, 1.0)]).unwrap();
        s.checkpoint().unwrap();
        // A copy of the .db file alone, without the -wal file, has the data.
        let copy = path.with_file_name("copy.db");
        std::fs::copy(&path, &copy).unwrap();
        let c = Store::open(&copy).unwrap();
        assert_eq!(c.samples("x", 3600, 0, 10).unwrap(), vec![(0, 1.0)]);
        drop((s, c));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn non_finite_forecasts_are_skipped() {
        let mut s = Store::open_in_memory().unwrap();
        let p = |v| ForecastPoint {
            valid_ts: 0,
            issued_ts: -10,
            value: v,
        };
        assert_eq!(s.put_forecasts("src", "t", &[p(1.0), p(f64::NAN)]).unwrap(), 1);
        assert_eq!(s.forecasts_asof("src", "t", 0, 1, 0).unwrap(), vec![(0, 1.0)]);
        assert!(
            s.forecasts_asof("src", "t", 0, 1, -11).unwrap().is_empty(),
            "not yet issued"
        );
        assert!(s.forecasts_asof("other", "t", 0, 1, 0).unwrap().is_empty());
    }
}
