//! vulpes-cli: data import, backfill and (later) backtesting tools.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate};
use clap::{Parser, Subcommand};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use vulpes_core::{ForecastPoint, SeriesKind, Store};
use vulpes_sources::{Location, energyzero, open_meteo};

mod entsoe;
mod load;
mod price;
mod replay;
mod series;
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
        /// Variables to fetch (default: the PV/temperature set).
        #[arg(long, value_delimiter = ',')]
        vars: Vec<String>,
        /// Label for a reference point; stored as `<source>:<model>@<label>`.
        #[arg(long)]
        label: Option<String>,
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
    /// Calibrate a string-inverter system (what the Home Assistant integration
    /// runs): fits each plane to its string's DC power, or scales all planes to
    /// the total, and scores the result on held-out weeks. Fetches weather.
    CalibrateSystem {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
        /// JSON list of planes as entered (azimuth, tilt, pdc0, ac_max, inverter).
        #[arg(long)]
        planes: PathBuf,
        /// Per plane, in order: the series with its string's DC power; `-` for none.
        #[arg(long, value_delimiter = ',')]
        strings: Vec<String>,
        /// Series with the total PV output (hourly energy or mean power).
        #[arg(long)]
        total: Option<String>,
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "knmi_harmonie_arome_netherlands,icon_d2"
        )]
        models: Vec<String>,
        /// Write the proposed planes (JSON) here.
        #[arg(long)]
        out: Option<PathBuf>,
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
    /// Fit the consumer tariff `all_in = a · wholesale + b` per calendar year.
    FitTariff {
        /// `<cost_series>:<energy_series>` pairs (EUR and kWh per hour), e.g. import and export.
        #[arg(long = "pair", required = true)]
        pairs: Vec<String>,
        #[arg(long, default_value = PRICE_KEY)]
        wholesale: String,
        /// Ignore steps with less energy (kWh); rounded supplier costs make them noisy.
        #[arg(long, default_value_t = 0.0)]
        min_energy: f64,
    },
    /// Evaluate day-ahead wholesale price forecasts (issued 10:00 UTC the day before).
    EvalPrice {
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
        #[arg(long, default_value = "Europe/Amsterdam")]
        tz: chrono_tz::Tz,
        /// Weather sources with `wind_speed_100m` at wind reference points.
        #[arg(long, value_delimiter = ',', required = true)]
        wind: Vec<String>,
        /// Weather sources with `shortwave_radiation` at solar reference points.
        #[arg(long, value_delimiter = ',', required = true)]
        solar: Vec<String>,
        #[arg(long, default_value = PRICE_KEY)]
        wholesale: String,
    },
    /// Replay history hour by hour, comparing control strategies on actual data.
    Replay {
        #[arg(long, default_value = "local/site.json")]
        site: PathBuf,
        #[arg(long, default_value = "local/solar-planes.json")]
        planes: PathBuf,
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
        #[arg(long, default_value = "Europe/Amsterdam")]
        tz: chrono_tz::Tz,
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        #[arg(long, default_value_t = 36)]
        horizon_h: usize,
        #[arg(long, default_value = PRICE_KEY)]
        wholesale: String,
        /// Scenario: override export pricing with `<a>,<b>` (export = a·wholesale + b).
        #[arg(long, allow_hyphen_values = true)]
        export: Option<String>,
    },
    /// Backfill ENTSO-E day-ahead prices and wind/solar/load forecasts for a zone.
    /// Token: ENTSOE_TOKEN or local/entsoe.token.
    BackfillEntsoe {
        /// Bidding zone: NL or DE-LU.
        #[arg(long, default_value = "NL")]
        zone: String,
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
            vars,
            label,
        } => backfill_weather(
            &mut store,
            WeatherBackfill {
                loc: Location { lat, lon },
                from,
                to,
                models,
                lead_days,
                vars,
                label,
            },
        ),
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
        Cmd::CalibrateSystem {
            lat,
            lon,
            from,
            to,
            planes,
            strings,
            total,
            models,
            out,
        } => {
            let text =
                std::fs::read_to_string(&planes).with_context(|| format!("reading {}", planes.display()))?;
            let planes: Vec<vulpes_solar::PlaneParams> = serde_json::from_str(&text)?;
            let inputs = solar::SystemInputs {
                lat,
                lon,
                from,
                to,
                strings: &strings,
                total: total.as_deref(),
                models,
            };
            let req = solar::system_request(&store, planes, &inputs)?;
            let resp = vulpes_engine::calibrate(&req)?;
            for line in solar::system_report(&req.planes, &resp) {
                println!("{line}");
            }
            if let Some(out) = out {
                std::fs::write(&out, serde_json::to_string_pretty(&resp.planes)?)?;
                println!("written to {}", out.display());
            }
            Ok(())
        }
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
        } => load::evaluate(
            &store,
            from,
            to,
            tz,
            parse_country(&country)?,
            &temp_forecast,
            &temp_history,
        ),
        Cmd::FitTariff {
            pairs,
            wholesale,
            min_energy,
        } => {
            let pairs = pairs.iter().map(|p| parse_pair(p)).collect::<Result<Vec<_>>>()?;
            price::fit_tariff(&store, &pairs, &wholesale, min_energy)
        }
        Cmd::EvalPrice {
            from,
            to,
            tz,
            wind,
            solar,
            wholesale,
        } => price::evaluate(
            &store,
            from,
            to,
            tz,
            &price::PricePoints { wind, solar },
            &wholesale,
        ),
        Cmd::Replay {
            site,
            planes,
            from,
            to,
            tz,
            lat,
            lon,
            horizon_h,
            wholesale,
            export,
        } => replay::run(
            &store,
            &replay::ReplayArgs {
                site: &site,
                planes: &planes,
                from,
                to,
                tz,
                lat,
                lon,
                horizon_h,
                wholesale: &wholesale,
                export_override: export.as_deref().map(parse_export).transpose()?,
            },
        ),
        Cmd::BackfillEntsoe { zone, from, to } => entsoe::backfill(&mut store, &zone, from, to),
        Cmd::Stats => stats(&store),
    }
}

const PRICE_KEY: &str = "price.wholesale.nl";

fn parse_country(c: &str) -> Result<vulpes_load::Country> {
    Ok(match c {
        "nl" => vulpes_load::Country::Netherlands,
        "none" => vulpes_load::Country::None,
        c => bail!("unknown country '{c}' (supported: none, nl)"),
    })
}

/// `--pair <cost_series>:<energy_series>`.
fn parse_pair(p: &str) -> Result<(String, String)> {
    match p.split_once(':') {
        Some((c, e)) if !c.is_empty() && !e.is_empty() => Ok((c.to_string(), e.to_string())),
        _ => bail!("--pair must be <cost_series>:<energy_series>, got '{p}'"),
    }
}

/// `--export <a>,<b>`: export = a · wholesale + b.
fn parse_export(e: &str) -> Result<(f64, f64)> {
    let (a, b) = e.split_once(',').context("--export must be <a>,<b>")?;
    let num = |s: &str| {
        s.trim()
            .parse::<f64>()
            .with_context(|| format!("--export: '{}' is not a number", s.trim()))
    };
    Ok((num(a)?, num(b)?))
}

/// Split `[from, to]` into consecutive inclusive chunks of at most `days` days.
pub(crate) fn date_chunks(from: NaiveDate, to: NaiveDate, days: i64) -> Vec<(NaiveDate, NaiveDate)> {
    let mut out = Vec::new();
    let mut start = from;
    while start <= to {
        let end = (start + chrono::Duration::days(days - 1)).min(to);
        out.push((start, end));
        start = end + chrono::Duration::days(1);
    }
    out
}

pub(crate) fn read_map(path: &Path) -> Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_map(&text, &path.display().to_string())
}

/// Lines of `<vulpes_key> <statistic_id>`; `#` starts a comment.
fn parse_map(text: &str, name: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(k), Some(s), None) => out.push((k.to_string(), s.to_string())),
            _ => bail!("{name}:{}: expected `<vulpes_key> <statistic_id>`", i + 1),
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
    for line in import_statistics(store, &ha, &read_map(map)?)? {
        println!("{line}");
    }
    Ok(())
}

/// Copy hourly statistics from a Home Assistant recorder database: series
/// with a cumulative `sum` become hourly amounts, others hourly means.
/// Returns one report line per mapped statistic.
fn import_statistics(store: &mut Store, ha: &Connection, map: &[(String, String)]) -> Result<Vec<String>> {
    let mut report = Vec::new();
    for (key, statistic_id) in map {
        let meta: Option<(i64, Option<String>, bool)> = ha
            .query_row(
                "SELECT id, unit_of_measurement, has_sum FROM statistics_meta WHERE statistic_id = ?1",
                [&statistic_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        let Some((meta_id, unit, has_sum)) = meta else {
            report.push(format!("{key}: statistic not found, skipped"));
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
        store.ensure_series(key, &unit, kind)?;
        let n = store.put_samples(key, 3600, &samples)?;
        report.push(format!("{key}: {n} hourly samples ({kind:?}, {unit})"));
    }
    Ok(report)
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

struct WeatherBackfill {
    loc: Location,
    from: NaiveDate,
    to: NaiveDate,
    models: Vec<String>,
    lead_days: Option<u8>,
    vars: Vec<String>,
    /// Appended to the source as `@<label>`, to keep reference points apart from the site.
    label: Option<String>,
}

fn backfill_weather(store: &mut Store, req: WeatherBackfill) -> Result<()> {
    let WeatherBackfill {
        loc,
        from,
        to,
        models,
        lead_days,
        vars,
        label,
    } = req;
    if lead_days.is_some_and(|d| !(1..=7).contains(&d)) {
        bail!("--lead-days must be between 1 and 7");
    }
    let models: Vec<&str> = models.iter().map(String::as_str).collect();
    let vars: Vec<&str> = if vars.is_empty() {
        open_meteo::DEFAULT_VARS.to_vec()
    } else {
        vars.iter().map(String::as_str).collect()
    };
    for (start, end) in date_chunks(from, to, 31) {
        let series = open_meteo::fetch(&open_meteo::Request {
            location: loc,
            models: &models,
            vars: &vars,
            range: match lead_days {
                None => open_meteo::Range::Historical { start, end },
                Some(lead_days) => open_meteo::Range::PreviousRuns {
                    start,
                    end,
                    lead_days,
                },
            },
        })?;
        let lead_s = lead_days.map_or(0, |d| i64::from(d) * 86_400);
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
            n += store.put_forecasts(
                &weather_source(lead_days, &s.model, label.as_deref()),
                &s.var,
                &pts,
            )?;
        }
        println!("{start}..{end}: {n} forecast values");
    }
    Ok(())
}

/// Store source name: `open-meteo-archive:<model>` or `open-meteo-day<N>:<model>`,
/// plus `@<label>` for reference points.
fn weather_source(lead_days: Option<u8>, model: &str, label: Option<&str>) -> String {
    let prefix = match lead_days {
        None => "open-meteo-archive".to_string(),
        Some(d) => format!("open-meteo-day{d}"),
    };
    let suffix = label.map(|l| format!("@{l}")).unwrap_or_default();
    format!("{prefix}:{model}{suffix}")
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
    use clap::CommandFactory;
    use std::collections::HashMap;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "vulpes-cli",
            "replay",
            "--from",
            "2026-10-01",
            "--to",
            "2026-10-14",
            "--lat",
            "52",
            "--lon",
            "5",
            "--export",
            "1.21,0",
        ])
        .unwrap();
        assert!(matches!(cli.cmd, Cmd::Replay { horizon_h: 36, .. }));
        assert!(Cli::try_parse_from(["vulpes-cli", "replay", "--from", "2026-10-01"]).is_err());
        assert!(Cli::try_parse_from(["vulpes-cli", "stats"]).is_ok());
    }

    #[test]
    fn argument_parsers() {
        assert!(matches!(
            parse_country("nl").unwrap(),
            vulpes_load::Country::Netherlands
        ));
        assert!(matches!(
            parse_country("none").unwrap(),
            vulpes_load::Country::None
        ));
        assert!(parse_country("be").is_err());
        assert_eq!(
            parse_pair("cost.p1:grid.import.p1").unwrap(),
            ("cost.p1".into(), "grid.import.p1".into())
        );
        assert!(parse_pair("cost.p1").is_err() && parse_pair(":x").is_err() && parse_pair("x:").is_err());
        assert_eq!(parse_export("1.21, -0.0248").unwrap(), (1.21, -0.0248));
        assert!(parse_export("1.21").is_err());
        let err = parse_export("1.21,abc").unwrap_err().to_string();
        assert!(err.contains("'abc' is not a number"), "{err}");
    }

    #[test]
    fn date_chunks_cover_the_range_without_overlap() {
        let c = date_chunks(date("2026-01-01"), date("2026-03-05"), 31);
        assert_eq!(
            c,
            vec![
                (date("2026-01-01"), date("2026-01-31")),
                (date("2026-02-01"), date("2026-03-03")),
                (date("2026-03-04"), date("2026-03-05")),
            ]
        );
        assert_eq!(date_chunks(date("2026-01-01"), date("2026-01-01"), 31).len(), 1);
        assert!(date_chunks(date("2026-01-02"), date("2026-01-01"), 31).is_empty());
    }

    #[test]
    fn map_file_parsing() {
        let text = "# comment\nload.house sensor.a  # trailing\n\n  pv.total sensor.b\n";
        assert_eq!(
            parse_map(text, "map.txt").unwrap(),
            vec![
                ("load.house".into(), "sensor.a".into()),
                ("pv.total".into(), "sensor.b".into())
            ]
        );
        let err = parse_map("ok sensor.a\nonly-one\n", "map.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("map.txt:2"), "{err}");
        assert!(parse_map("a b c", "m").is_err());
        assert!(parse_map("", "m").unwrap().is_empty());
    }

    #[test]
    fn weather_source_names() {
        assert_eq!(
            weather_source(None, "icon_d2", None),
            "open-meteo-archive:icon_d2"
        );
        assert_eq!(
            weather_source(Some(1), "icon_d2", None),
            "open-meteo-day1:icon_d2"
        );
        assert_eq!(
            weather_source(Some(1), "icon_d2", Some("nl-central")),
            "open-meteo-day1:icon_d2@nl-central"
        );
    }

    /// A recorder database with Home Assistant's statistics tables.
    fn ha_db() -> Connection {
        let ha = Connection::open_in_memory().unwrap();
        ha.execute_batch(
            "CREATE TABLE statistics_meta (id INTEGER PRIMARY KEY, statistic_id TEXT, unit_of_measurement TEXT, has_sum BOOLEAN);
             CREATE TABLE statistics (metadata_id INTEGER, start_ts FLOAT, mean FLOAT, state FLOAT, sum FLOAT);
             INSERT INTO statistics_meta VALUES (1, 'sensor.energy', 'kWh', 1), (2, 'sensor.power', 'W', 0), (3, 'sensor.temp', '°C', 0);
             INSERT INTO statistics VALUES
               (1, 0.0, NULL, 5.0, 10.0), (1, 3600.0, NULL, 6.0, 12.5), (1, 7200.0, NULL, 7.0, 13.0),
               (2, 0.0, 300.0, 280.0, NULL), (2, 3600.0, NULL, 450.0, NULL), (2, 7200.0, NULL, NULL, NULL),
               (3, 0.0, 18.5, NULL, NULL);",
        )
        .unwrap();
        ha
    }

    #[test]
    fn import_converts_sums_to_amounts_and_falls_back_to_state() {
        let mut store = Store::open_in_memory().unwrap();
        let map = vec![
            ("grid".to_string(), "sensor.energy".to_string()),
            ("power".to_string(), "sensor.power".to_string()),
            ("missing".to_string(), "sensor.nope".to_string()),
        ];
        let report = import_statistics(&mut store, &ha_db(), &map).unwrap();
        assert_eq!(report.len(), 3);
        assert!(report[2].contains("not found"), "{report:?}");
        assert_eq!(
            store.samples("grid", 3600, 0, 10_000).unwrap(),
            vec![(3600, 2.5), (7200, 0.5)]
        );
        // Mean where recorded, else the state; hours with neither are dropped.
        assert_eq!(
            store.samples("power", 3600, 0, 10_000).unwrap(),
            vec![(0, 300.0), (3600, 450.0)]
        );
        let units: HashMap<String, String> = store
            .summary()
            .unwrap()
            .into_iter()
            .map(|s| (s.key, s.unit))
            .collect();
        assert_eq!((units["grid"].as_str(), units["power"].as_str()), ("kWh", "W"));
    }

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
