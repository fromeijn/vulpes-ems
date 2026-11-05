//! vulpes-cli: data import, backfill and (later) backtesting tools.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use vulpes_core::{ForecastPoint, SeriesKind, Store};
use vulpes_sources::{Location, energyzero, open_meteo};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// vulpes store (SQLite) to read/write.
    #[arg(long, global = true, default_value = "local/vulpes.db")]
    store: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import hourly long-term statistics from a Home Assistant database (opened read-only).
    ImportHa {
        /// Path to home-assistant_v2.db.
        #[arg(long)]
        ha_db: PathBuf,
        /// Mapping file: one `<vulpes_key> <statistic_id>` per line, `#` comments.
        #[arg(long, default_value = "local/ha-map.txt")]
        map: PathBuf,
    },
    /// Backfill archived Open-Meteo 15-minute forecasts.
    BackfillWeather {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "knmi_harmonie_arome_netherlands,icon_d2"
        )]
        models: Vec<String>,
    },
    /// Backfill hourly NL day-ahead prices from EnergyZero.
    BackfillPrices {
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
    },
    /// Show what the store contains.
    Stats,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(dir) = cli.store.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut store =
        Store::open(&cli.store).with_context(|| format!("opening {}", cli.store.display()))?;
    match cli.cmd {
        Cmd::ImportHa { ha_db, map } => import_ha(&mut store, &ha_db, &map),
        Cmd::BackfillWeather {
            lat,
            lon,
            from,
            to,
            models,
        } => backfill_weather(&mut store, Location { lat, lon }, from, to, &models),
        Cmd::BackfillPrices { from, to } => {
            store.ensure_series(PRICE_KEY, "EUR/kWh", SeriesKind::Mean)?;
            let prices = energyzero::fetch_hourly(from, to)?;
            let n = store.put_samples(PRICE_KEY, 3600, &prices)?;
            println!("{PRICE_KEY}: {n} hourly prices");
            Ok(())
        }
        Cmd::Stats => stats(&store),
    }
}

const PRICE_KEY: &str = "price.wholesale.nl";

fn read_map(path: &Path) -> Result<Vec<(String, String)>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(k), Some(s), None) => out.push((k.to_string(), s.to_string())),
            _ => bail!(
                "{}:{}: expected `<vulpes_key> <statistic_id>`",
                path.display(),
                i + 1
            ),
        }
    }
    Ok(out)
}

fn import_ha(store: &mut Store, ha_db: &Path, map: &Path) -> Result<()> {
    let ha = Connection::open_with_flags(
        ha_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", ha_db.display()))?;
    for (key, statistic_id) in read_map(map)? {
        let meta: Option<(i64, Option<String>, bool)> = ha
            .query_row(
                "SELECT id, unit_of_measurement, has_sum FROM statistics_meta WHERE statistic_id = ?1",
                [&statistic_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        let Some((meta_id, unit, has_sum)) = meta else {
            eprintln!("{key}: statistic not found, skipped");
            continue;
        };
        let unit = unit.unwrap_or_default();
        let rows: Vec<(i64, Option<f64>, Option<f64>)> = ha
            .prepare("SELECT CAST(start_ts AS INTEGER), mean, sum FROM statistics WHERE metadata_id = ?1 ORDER BY start_ts")?
            .query_map([meta_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let (kind, samples) = if has_sum {
            (SeriesKind::Sum, hourly_deltas(&rows))
        } else {
            (
                SeriesKind::Mean,
                rows.iter()
                    .filter_map(|&(t, m, _)| m.map(|m| (t, m)))
                    .collect(),
            )
        };
        store.ensure_series(&key, &unit, kind)?;
        let n = store.put_samples(&key, 3600, &samples)?;
        println!("{key}: {n} hourly samples ({kind:?}, {unit})");
    }
    Ok(())
}

/// HA stores a cumulative `sum` per hour; the amount in an hour is the
/// difference with the previous hour. Only consecutive hours are used, so gaps
/// never produce a spike. HA already corrects `sum` for meter resets, and
/// negative deltas are real (e.g. cost during negative-price hours).
fn hourly_deltas(rows: &[(i64, Option<f64>, Option<f64>)]) -> Vec<(i64, f64)> {
    rows.windows(2)
        .filter_map(|w| match (w[0], w[1]) {
            ((t0, _, Some(s0)), (t1, _, Some(s1))) if t1 - t0 == 3600 => Some((t1, s1 - s0)),
            _ => None,
        })
        .collect()
}

fn backfill_weather(
    store: &mut Store,
    loc: Location,
    from: NaiveDate,
    to: NaiveDate,
    models: &[String],
) -> Result<()> {
    let models: Vec<&str> = models.iter().map(String::as_str).collect();
    let mut start = from;
    while start <= to {
        let end = (start + chrono::Duration::days(30)).min(to);
        let series = open_meteo::fetch(&open_meteo::Request {
            location: loc,
            models: &models,
            vars: open_meteo::DEFAULT_VARS,
            range: open_meteo::Range::Historical { start, end },
        })?;
        let mut n = 0;
        for s in series {
            // Archived forecasts are stitched from the freshest run per hour, so
            // they are marked as issued at the valid time ("short lead").
            let pts: Vec<_> = s
                .points
                .iter()
                .map(|&(t, v)| ForecastPoint {
                    valid_ts: t,
                    issued_ts: t,
                    value: v,
                })
                .collect();
            n += store.put_forecasts(&format!("open-meteo-archive:{}", s.model), &s.var, &pts)?;
        }
        println!("{start}..{end}: {n} forecast values");
        start = end + chrono::Duration::days(1);
    }
    Ok(())
}

fn stats(store: &Store) -> Result<()> {
    let day = |t: i64| {
        DateTime::from_timestamp(t, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default()
    };
    for s in store.summary()? {
        println!(
            "{:28} {:8} {:5?} step={:5}s n={:7} {}..{}",
            s.key,
            s.unit,
            s.kind,
            s.step_s,
            s.count,
            day(s.first_ts),
            day(s.last_ts)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_skip_gaps_keep_negatives() {
        let rows = [
            (0, None, Some(10.0)),
            (3600, None, Some(12.0)),
            (7200, None, Some(15.0)),
            (14400, None, Some(20.0)), // gap: 10800 missing
            (18000, None, Some(19.5)), // negative hour (e.g. cost at negative prices)
            (21600, None, None),       // missing sum
            (25200, None, Some(21.0)),
        ];
        assert_eq!(
            hourly_deltas(&rows),
            vec![(3600, 2.0), (7200, 3.0), (18000, -0.5)]
        );
    }
}
