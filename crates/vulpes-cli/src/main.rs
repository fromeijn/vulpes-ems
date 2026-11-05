//! vulpes-cli: data import, backfill and (later) backtesting tools.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use vulpes_core::{ForecastPoint, SeriesKind, Store};
use vulpes_sources::{Location, energyzero, open_meteo};

mod load;
mod solar;

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
        /// Fetch forecasts as issued this many days before the valid time (1..=7)
        /// instead of the short-lead archive. Stored as `open-meteo-day<N>:<model>`.
        #[arg(long)]
        lead_days: Option<u8>,
    },
    /// Backfill hourly NL day-ahead prices from EnergyZero.
    BackfillPrices {
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
    },
    /// Calibrate per-panel PV planes (azimuth, tilt, capacity) from history.
    CalibrateSolar {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
        /// Panel list: one `<serial> <name>` per line; actuals are read from series `pv.inv.<serial>`.
        #[arg(long, default_value = "local/panels.txt")]
        panels: PathBuf,
        /// Open-Meteo model whose archive drives the fit.
        #[arg(long, default_value = "knmi_harmonie_arome_netherlands")]
        model: String,
        #[arg(long, default_value = "local/solar-planes.json")]
        out: PathBuf,
        /// Use only days in even or odd ISO weeks (hold the other half out for evaluation).
        #[arg(long, value_enum, default_value_t = solar::Weeks::All)]
        weeks: solar::Weeks,
        /// Also learn a per-panel shading map (experimental: on hourly data it did not
        /// improve held-out day-ahead accuracy, see docs/solar.md).
        #[arg(long)]
        shading: bool,
        /// Pseudo-hours pulling each shading bin toward "unshaded"; higher = more evidence needed.
        #[arg(long, default_value_t = 10.0)]
        shading_prior: f64,
    },
    /// Evaluate daily PV forecasts against actuals and the Solar Forecast ML benchmark.
    EvalSolar {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
        #[arg(long, default_value = "local/solar-planes.json")]
        planes: PathBuf,
        /// Weather lead time in days (source `open-meteo-day<N>`); 0 = short-lead archive.
        #[arg(long, default_value_t = 1)]
        lead_days: u8,
        #[arg(long, value_enum, default_value_t = solar::Weeks::All)]
        weeks: solar::Weeks,
        /// Ignore learned shading maps.
        #[arg(long)]
        no_shading: bool,
    },
    /// Reconstruct hourly house load (excluding the battery) into series `load.house`.
    BuildLoad {
        #[arg(long)]
        from: NaiveDate,
        /// Grid import energy series (kWh/h), highest priority first.
        #[arg(long, value_delimiter = ',', required = true)]
        grid_import: Vec<String>,
        /// Grid export energy series (kWh/h), same order as --grid-import.
        #[arg(long, value_delimiter = ',', required = true)]
        grid_export: Vec<String>,
        /// PV power series (W, hourly mean).
        #[arg(long)]
        pv: String,
        /// Battery charge / discharge energy series (kWh/h), AC side.
        #[arg(long)]
        battery_in: Option<String>,
        #[arg(long)]
        battery_out: Option<String>,
        /// Battery power series (W) used where the energy counters are missing.
        #[arg(long)]
        battery_power: Option<String>,
        /// +1 if --battery-power is positive when charging, -1 if positive when discharging.
        #[arg(long, default_value_t = 1.0, allow_hyphen_values = true)]
        battery_power_sign: f64,
    },
    /// Evaluate day-ahead load forecasts (issued 12:00 UTC the day before).
    EvalLoad {
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
        #[arg(long, default_value = "Europe/Amsterdam")]
        tz: chrono_tz::Tz,
        /// Public-holiday calendar: none or nl.
        #[arg(long, default_value = "nl")]
        country: String,
        /// Weather source for the target day's temperature (day-ahead forecast).
        #[arg(long, default_value = "open-meteo-day1:knmi_harmonie_arome_netherlands")]
        temp_forecast: String,
        /// Weather source for reference days' temperature.
        #[arg(long, default_value = "open-meteo-archive:knmi_harmonie_arome_netherlands")]
        temp_history: String,
    },
    /// Show what the store contains.
    Stats,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(dir) = cli.store.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut store = Store::open(&cli.store).with_context(|| format!("opening {}", cli.store.display()))?;
    match cli.cmd {
        Cmd::ImportHa { ha_db, map } => import_ha(&mut store, &ha_db, &map),
        Cmd::BackfillWeather {
            lat,
            lon,
            from,
            to,
            models,
            lead_days,
        } => backfill_weather(&mut store, Location { lat, lon }, from, to, &models, lead_days),
        Cmd::BackfillPrices { from, to } => {
            store.ensure_series(PRICE_KEY, "EUR/kWh", SeriesKind::Mean)?;
            let prices = energyzero::fetch_hourly(from, to)?;
            let n = store.put_samples(PRICE_KEY, 3600, &prices)?;
            println!("{PRICE_KEY}: {n} hourly prices");
            Ok(())
        }
        Cmd::CalibrateSolar {
            lat,
            lon,
            from,
            to,
            panels,
            model,
            out,
            weeks,
            shading,
            shading_prior,
        } => {
            let shading_prior = shading.then_some(shading_prior);
            solar::calibrate(
                &store,
                lat,
                lon,
                from,
                to,
                &panels,
                &model,
                &out,
                weeks,
                shading_prior,
            )
        }
        Cmd::EvalSolar {
            lat,
            lon,
            from,
            to,
            planes,
            lead_days,
            weeks,
            no_shading,
        } => solar::evaluate(&store, lat, lon, from, to, &planes, lead_days, weeks, !no_shading),
        Cmd::BuildLoad {
            from,
            grid_import,
            grid_export,
            pv,
            battery_in,
            battery_out,
            battery_power,
            battery_power_sign,
        } => {
            let src = load::LoadSources {
                grid_import,
                grid_export,
                energy_kwh: true,
                pv,
                battery_in,
                battery_out,
                battery_power,
                battery_power_sign,
            };
            load::build(&mut store, &src, from)
        }
        Cmd::EvalLoad {
            from,
            to,
            tz,
            country,
            temp_forecast,
            temp_history,
        } => {
            let country = match country.as_str() {
                "nl" => vulpes_load::Country::Netherlands,
                "none" => vulpes_load::Country::None,
                c => bail!("unknown country '{c}' (supported: none, nl)"),
            };
            load::evaluate(&store, from, to, tz, country, &temp_forecast, &temp_history)
        }
        Cmd::Stats => stats(&store),
    }
}

const PRICE_KEY: &str = "price.wholesale.nl";

pub(crate) fn read_map(path: &Path) -> Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
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
            // Fall back to the hourly state snapshot when no mean was recorded
            // (happens when an integration changes its sensor's state class).
            .prepare("SELECT CAST(start_ts AS INTEGER), COALESCE(mean, state), sum FROM statistics WHERE metadata_id = ?1 ORDER BY start_ts")?
            .query_map([meta_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let (kind, samples) = if has_sum {
            (SeriesKind::Sum, hourly_deltas(&rows))
        } else {
            (
                SeriesKind::Mean,
                rows.iter().filter_map(|&(t, m, _)| m.map(|m| (t, m))).collect(),
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
    lead_days: Option<u8>,
) -> Result<()> {
    if lead_days.is_some_and(|d| !(1..=7).contains(&d)) {
        bail!("--lead-days must be between 1 and 7");
    }
    let models: Vec<&str> = models.iter().map(String::as_str).collect();
    let mut start = from;
    while start <= to {
        let end = (start + chrono::Duration::days(30)).min(to);
        let series = open_meteo::fetch(&open_meteo::Request {
            location: loc,
            models: &models,
            vars: open_meteo::DEFAULT_VARS,
            range: match lead_days {
                None => open_meteo::Range::Historical { start, end },
                Some(lead_days) => open_meteo::Range::PreviousRuns {
                    start,
                    end,
                    lead_days,
                },
            },
        })?;
        let (prefix, lead_s) = match lead_days {
            None => ("open-meteo-archive".to_string(), 0),
            Some(d) => (format!("open-meteo-day{d}"), i64::from(d) * 86_400),
        };
        let mut n = 0;
        for s in series {
            // The short-lead archive is stitched from the freshest run per hour,
            // so it is marked as issued at the valid time.
            let pts: Vec<_> = s
                .points
                .iter()
                .map(|&(t, v)| ForecastPoint {
                    valid_ts: t,
                    issued_ts: t - lead_s,
                    value: v,
                })
                .collect();
            n += store.put_forecasts(&format!("{prefix}:{}", s.model), &s.var, &pts)?;
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
