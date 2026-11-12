//! Replay history hour by hour: each strategy decides with only the
//! information available at that moment, then the decision is executed
//! against actual load and PV and settled at actual prices.

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, TimeZone};
use chrono_tz::Tz;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use vulpes_core::{BatteryParams, GridLimits, ModeDef, Store};
use vulpes_load::{Country, ProfileForecaster, ProfileOptions};
use vulpes_opt::{Problem, SlotInput, optimize, step};
use vulpes_price::LinearTariff;

/// Export pricing: `a · wholesale + b`. Unknown keys are an error, so a site
/// file with the former `net_metering` switch doesn't replay another scenario
/// unnoticed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    a: f64,
    b: f64,
}

#[derive(Deserialize)]
struct Rules {
    charge_below: f64,
    grid_below: f64,
    net_zero_below: f64,
    charge_max_soc: f64,
    discharge_min_soc: f64,
    modes: HashMap<String, String>,
}

#[derive(Deserialize)]
pub(crate) struct Site {
    battery: BatteryParams,
    grid: GridLimits,
    modes: Vec<ModeDef>,
    tariff: LinearTariff,
    export: Export,
    rules: Option<Rules>,
}

impl Site {
    fn import_price(&self, wholesale: f64) -> f64 {
        self.tariff.price(wholesale)
    }
    fn export_price(&self, wholesale: f64) -> f64 {
        self.export.a * wholesale + self.export.b
    }
    /// The limits `step` relies on: a negative grid limit, for example, would
    /// silently block all charging.
    fn validate(&self) -> Result<()> {
        self.battery.validate()?;
        self.grid.validate()?;
        if !(self.export.a.is_finite() && self.export.b.is_finite()) {
            bail!("export: a and b must be numbers");
        }
        Ok(())
    }
    fn mode_index(&self, name: &str) -> Result<usize> {
        self.modes
            .iter()
            .position(|m| m.name == name)
            .with_context(|| format!("unknown mode '{name}'"))
    }
}

pub(crate) struct ReplayArgs<'a> {
    pub site: &'a Path,
    pub planes: &'a Path,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub tz: Tz,
    pub lat: f64,
    pub lon: f64,
    pub horizon_h: usize,
    /// Further horizons (hours) to compare the optimizer on.
    pub compare_horizons: &'a [usize],
    /// Weather model for PV forecasts issued two or more days ahead.
    pub tail_model: &'a str,
    pub wholesale: &'a str,
    pub export_override: Option<(f64, f64)>,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Tally {
    cost: f64,
    import_kwh: f64,
    export_kwh: f64,
    throughput_kwh: f64,
    mode_hours: Vec<usize>,
}

impl Tally {
    /// Settle one hour with grid and battery power in W (grid positive = import).
    fn settle(&mut self, grid_w: f64, battery_w: f64, import: f64, export: f64, wear: f64) {
        let kwh = grid_w / 1000.0;
        let throughput = battery_w.abs() / 1000.0;
        self.cost += if kwh >= 0.0 { kwh * import } else { kwh * export } + wear * throughput;
        self.import_kwh += kwh.max(0.0);
        self.export_kwh += (-kwh).max(0.0);
        self.throughput_kwh += throughput;
    }
}

/// Which forecast an oracle strategy replaces with the actual values.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Oracle {
    Load,
    Pv,
    Price,
}

/// How a replayed strategy picks the mode for the hour.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Strategy {
    SelfConsume,
    /// The site's threshold rules; self-consumption when it has none.
    Rules,
    /// The optimizer on the forecasts available at that hour.
    Optimizer,
    /// The optimizer with one forecast replaced by the actual values.
    Oracle(Oracle),
    /// The optimizer on the actual values of the whole horizon.
    Perfect,
}

/// Every strategy, in the order of the report.
const ALL: [Strategy; 7] = [
    Strategy::SelfConsume,
    Strategy::Rules,
    Strategy::Optimizer,
    Strategy::Oracle(Oracle::Load),
    Strategy::Oracle(Oracle::Pv),
    Strategy::Oracle(Oracle::Price),
    Strategy::Perfect,
];

impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Strategy::SelfConsume => "self-consumption only",
            Strategy::Rules => "threshold rules (current)",
            Strategy::Optimizer => "vulpes optimizer",
            Strategy::Oracle(Oracle::Load) => "optimizer + actual load",
            Strategy::Oracle(Oracle::Pv) => "optimizer + actual PV",
            Strategy::Oracle(Oracle::Price) => "optimizer + actual prices",
            Strategy::Perfect => "perfect foresight",
        }
    }

    /// What this strategy knows that the optimizer has to forecast; `None`
    /// for the strategies that know nothing more.
    fn foresight(self) -> Option<&'static str> {
        match self {
            Strategy::Oracle(Oracle::Load) => Some("load"),
            Strategy::Oracle(Oracle::Pv) => Some("PV"),
            Strategy::Oracle(Oracle::Price) => Some("prices"),
            Strategy::Perfect => Some("all three"),
            Strategy::SelfConsume | Strategy::Rules | Strategy::Optimizer => None,
        }
    }
}

/// The forecast slot with one input taken from the actual slot.
fn oracle_slot(fc: &SlotInput, actual: &SlotInput, which: Oracle) -> SlotInput {
    match which {
        Oracle::Load => SlotInput {
            load_w: actual.load_w,
            ..*fc
        },
        Oracle::Pv => SlotInput {
            pv_w: actual.pv_w,
            ..*fc
        },
        Oracle::Price => SlotInput {
            import_price: actual.import_price,
            export_price: actual.export_price,
            ..*fc
        },
    }
}

/// Hourly history the replay runs on, keyed by hour start (Unix seconds).
struct ReplayData {
    /// Actual house load excluding the battery (W).
    load: HashMap<i64, f64>,
    /// Actual PV (W).
    pv: HashMap<i64, f64>,
    /// Actual wholesale price (currency/kWh).
    price: HashMap<i64, f64>,
    /// PV forecast (W) by lead: entry `n - 1` holds the forecast issued `n`
    /// days before each hour.
    pv_fc: Vec<HashMap<i64, f64>>,
}

/// The freshest forecast lead (days, at least 1) that was already issued at
/// `t` for hour `h`: a forecast with lead `n` is issued `n` days before `h`.
fn lead_days(t: i64, h: i64) -> usize {
    ((h - t).max(1) as usize).div_ceil(86_400)
}

/// PV forecast for hour `h` as known at `t`; `None` when that lead is missing.
fn pv_forecast(pv_fc: &[HashMap<i64, f64>], t: i64, h: i64) -> Option<f64> {
    pv_fc.get(lead_days(t, h) - 1)?.get(&h).copied()
}

struct Report {
    /// The strategies replayed; `tallies` and each row of `daily` follow this order.
    strategies: Vec<Strategy>,
    hours: usize,
    /// Replayed hours that were planned on fewer than the horizon's hours,
    /// because a forecast was missing further ahead.
    short_hours: usize,
    tallies: Vec<Tally>,
    /// Cost per local day of which every hour was replayed, in order: one
    /// value per strategy. The yearly figures are scaled from these days.
    daily: Vec<Vec<f64>>,
    /// Local days with only some of their hours replayed; not in `daily`.
    partial_days: usize,
}

impl Report {
    fn index(&self, s: Strategy) -> Result<usize> {
        self.strategies
            .iter()
            .position(|x| *x == s)
            .with_context(|| format!("'{}' was not replayed", s.name()))
    }

    fn tally(&self, s: Strategy) -> Result<&Tally> {
        Ok(&self.tallies[self.index(s)?])
    }

    /// Per replayed day, what strategy `a` cost more than strategy `b`.
    fn daily_saving(&self, a: Strategy, b: Strategy) -> Result<Vec<f64>> {
        let (a, b) = (self.index(a)?, self.index(b)?);
        Ok(self.daily.iter().map(|d| d[a] - d[b]).collect())
    }
}

/// Yearly saving implied by daily savings: `(estimate, low, high)`, with a
/// 90 % interval from resampling blocks of `block` consecutive days. Blocks
/// rather than days, because consecutive days are correlated. The resampling
/// is seeded, so the same input gives the same interval. `None` without data.
fn yearly_saving(daily: &[f64], block: usize, draws: usize) -> Option<(f64, f64, f64)> {
    if daily.is_empty() || block == 0 || draws == 0 {
        return None;
    }
    let blocks: Vec<(f64, f64)> = daily
        .chunks(block)
        .map(|c| (c.iter().sum(), c.len() as f64))
        .collect();
    let per_year = |sum: f64, days: f64| sum / days * 365.0;
    // SplitMix64: small, seeded and good enough for resampling.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut samples: Vec<f64> = (0..draws)
        .map(|_| {
            let (mut sum, mut days) = (0.0, 0.0);
            for _ in 0..blocks.len() {
                let (s, d) = blocks[(next() % blocks.len() as u64) as usize];
                sum += s;
                days += d;
            }
            per_year(sum, days)
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    Some((
        per_year(daily.iter().sum(), daily.len() as f64),
        at(0.05),
        at(0.95),
    ))
}

/// Local midnight starting date `d`.
fn midnight(tz: Tz, d: NaiveDate) -> Result<i64> {
    Ok(tz
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).context("date")?)
        .earliest()
        .context("tz")?
        .timestamp())
}

/// When the day-ahead price for hour `h` becomes known: 12:00 UTC the day
/// before its local day.
fn published_at(tz: Tz, h: i64) -> i64 {
    let day = tz
        .timestamp_opt(h, 0)
        .single()
        .map(|t| t.date_naive())
        .unwrap_or_default();
    midnight(tz, day).unwrap_or(h) - 12 * 3600
}

/// Value of stored energy after the horizon: median import price × discharge efficiency.
fn terminal_value(slots: &[SlotInput], discharge_eff: f64) -> f64 {
    let mut v: Vec<f64> = slots.iter().map(|x| x.import_price).collect();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2] * discharge_eff
}

/// The threshold rules: `[charge, grid, net_zero, discharge]` mode indices.
fn rule_choice(r: &Rules, idx: [usize; 4], import: f64, soc_frac: f64) -> usize {
    let [charge, grid, net_zero, discharge] = idx;
    if import < r.charge_below && soc_frac < r.charge_max_soc {
        charge
    } else if import < r.grid_below {
        grid
    } else if import < r.net_zero_below {
        net_zero
    } else if soc_frac > r.discharge_min_soc {
        discharge
    } else {
        net_zero
    }
}

fn sorted(m: &HashMap<i64, f64>) -> Vec<(i64, f64)> {
    let mut v: Vec<_> = m.iter().map(|(&t, &x)| (t, x)).collect();
    v.sort_by_key(|x| x.0);
    v
}

/// Hours in local day `d`: 23 or 25 when the clock changes.
fn day_hours(tz: Tz, d: NaiveDate) -> Result<usize> {
    let next = d.succ_opt().context("date")?;
    Ok(((midnight(tz, next)? - midnight(tz, d)?) / 3600) as usize)
}

/// Run the given strategies hour by hour over `[start, end)`. Each decision
/// only uses data available at that hour; the decision is then executed
/// against the actual load and PV and settled at the actual price.
fn simulate(
    site: &Site,
    data: &ReplayData,
    tz: Tz,
    start: i64,
    end: i64,
    horizon_h: usize,
    strategies: &[Strategy],
) -> Result<Report> {
    site.validate()?;
    let (load, pv, price) = (&data.load, &data.pv, &data.price);
    let (load_hist, price_hist) = (sorted(load), sorted(price));
    let rules = match &site.rules {
        Some(r) => Some([
            site.mode_index(r.modes.get("charge").context("rules.modes.charge missing")?)?,
            site.mode_index(r.modes.get("grid").context("rules.modes.grid missing")?)?,
            site.mode_index(r.modes.get("net_zero").context("rules.modes.net_zero missing")?)?,
            site.mode_index(
                r.modes
                    .get("discharge")
                    .context("rules.modes.discharge missing")?,
            )?,
        ]),
        None => None,
    };
    let net_zero = match rules {
        Some(r) => r[2],
        None => site
            .modes
            .iter()
            .position(|m| m.behaviour == vulpes_core::Behaviour::SelfConsume)
            .context("no self_consume mode for the self-consumption baseline")?,
    };
    let b = &site.battery;
    let mut tallies = vec![
        Tally {
            mode_hours: vec![0; site.modes.len()],
            ..Default::default()
        };
        strategies.len()
    ];
    let mut soc = vec![0.5 * b.capacity_wh; strategies.len()];
    // The actual values of the horizon are only needed by the strategies that see them.
    let foresight = strategies.iter().any(|s| s.foresight().is_some());
    let plan_mode = |s: &[SlotInput], soc: f64| {
        optimize(&Problem {
            slots: s,
            dt_h: 1.0,
            modes: &site.modes,
            battery: b,
            grid: &site.grid,
            soc0_wh: soc,
            terminal_value_per_kwh: terminal_value(s, b.discharge_eff),
            soc_step_wh: 100.0,
        })
        .slots[0]
            .mode
    };

    let mut day_cache: Option<(NaiveDate, ProfileForecaster, ProfileForecaster)> = None;
    let (mut hours, mut short_hours) = (0usize, 0usize);
    // Per local day with a replayed hour: the day, its replayed hours and the cost per strategy.
    let mut days: Vec<(NaiveDate, usize, Vec<f64>)> = Vec::new();
    let mut t = start;
    while t < end {
        let (Some(&l_act), Some(&pv_act), Some(&w_act)) = (load.get(&t), pv.get(&t), price.get(&t)) else {
            t += 3600;
            continue;
        };
        // Forecasters are refit once per local day on data before its midnight.
        let today = tz.timestamp_opt(t, 0).single().context("tz")?.date_naive();
        if day_cache.as_ref().is_none_or(|c| c.0 != today) {
            let cut = midnight(tz, today)?;
            let before = |h: &[(i64, f64)]| -> Vec<(i64, f64)> {
                h.iter().copied().filter(|&(x, _)| x < cut).collect()
            };
            let price_opt = ProfileOptions {
                lookback_days: 28,
                half_life_days: 7.0,
                ..Default::default()
            };
            day_cache = Some((
                today,
                ProfileForecaster::new(
                    &before(&load_hist),
                    tz,
                    Country::Netherlands,
                    ProfileOptions::default(),
                ),
                ProfileForecaster::new(&before(&price_hist), tz, Country::None, price_opt),
            ));
        }
        let (_, load_fc, price_fc) = day_cache.as_ref().expect("cache set");
        // Forecast inputs as known at t, and the actual values for the perfect-foresight bound.
        let mut fc = Vec::new();
        let mut perfect = Vec::new();
        for h in (0..horizon_h as i64).map(|k| t + k * 3600) {
            let w = if published_at(tz, h) <= t {
                price.get(&h).copied()
            } else {
                price_fc.forecast(h)
            };
            let (Some(w), Some(l), Some(p)) = (w, load_fc.forecast(h), pv_forecast(&data.pv_fc, t, h)) else {
                break;
            };
            fc.push(SlotInput {
                load_w: l,
                pv_w: p,
                import_price: site.import_price(w),
                export_price: site.export_price(w),
                min_soc_wh: None,
            });
            if !foresight {
                continue;
            }
            if let (Some(&la), Some(&pa), Some(&wa)) = (load.get(&h), pv.get(&h), price.get(&h)) {
                perfect.push(SlotInput {
                    load_w: la,
                    pv_w: pa,
                    import_price: site.import_price(wa),
                    export_price: site.export_price(wa),
                    min_soc_wh: None,
                });
            }
        }
        if fc.is_empty() {
            t += 3600;
            continue;
        }
        let (imp, exp) = (site.import_price(w_act), site.export_price(w_act));
        // Without actuals for the whole horizon, the oracles and perfect
        // foresight decide on the forecasts.
        let complete = perfect.len() == fc.len();
        let choice: Vec<usize> = strategies
            .iter()
            .zip(&soc)
            .map(|(s, &soc)| match *s {
                Strategy::SelfConsume => net_zero,
                Strategy::Rules => match (rules, &site.rules) {
                    (Some(idx), Some(r)) => rule_choice(r, idx, imp, soc / b.capacity_wh),
                    _ => net_zero,
                },
                Strategy::Oracle(which) if complete => {
                    let slots: Vec<SlotInput> = fc
                        .iter()
                        .zip(&perfect)
                        .map(|(f, a)| oracle_slot(f, a, which))
                        .collect();
                    plan_mode(&slots, soc)
                }
                Strategy::Perfect if complete => plan_mode(&perfect, soc),
                Strategy::Optimizer | Strategy::Oracle(_) | Strategy::Perfect => plan_mode(&fc, soc),
            })
            .collect();
        if days.last().is_none_or(|d| d.0 != today) {
            days.push((today, 0, vec![0.0; strategies.len()]));
        }
        let (_, day_hours, day_costs) = days.last_mut().expect("day pushed");
        *day_hours += 1;
        for (k, &m) in choice.iter().enumerate() {
            let o = step(&site.modes[m], l_act, pv_act, soc[k], b, &site.grid, 1.0);
            soc[k] = o.soc_wh;
            let before = tallies[k].cost;
            tallies[k].settle(o.grid_w, o.battery_w, imp, exp, b.wear_cost_per_kwh);
            tallies[k].mode_hours[m] += 1;
            day_costs[k] += tallies[k].cost - before;
        }
        hours += 1;
        short_hours += usize::from(fc.len() < horizon_h);
        t += 3600;
    }
    if hours == 0 {
        bail!("no hours with complete data in range");
    }
    // A day with skipped hours has a cost for only part of it, so it cannot
    // be scaled to a year together with whole days.
    let mut daily = Vec::new();
    for (day, replayed, costs) in &days {
        if *replayed == day_hours(tz, *day)? {
            daily.push(costs.clone());
        }
    }
    Ok(Report {
        strategies: strategies.to_vec(),
        hours,
        short_hours,
        tallies,
        partial_days: days.len() - daily.len(),
        daily,
    })
}

pub(crate) fn run(store: &Store, args: &ReplayArgs<'_>) -> Result<()> {
    let mut site: Site = serde_json::from_str(&std::fs::read_to_string(args.site)?)
        .with_context(|| format!("parsing {}", args.site.display()))?;
    if let Some((a, b)) = args.export_override {
        site.export = Export { a, b };
    }
    let tz = args.tz;
    let all = |k: &str| -> Result<HashMap<i64, f64>> {
        Ok(store.samples(k, 3600, 0, i64::MAX / 2)?.into_iter().collect())
    };
    let (start, end) = (
        midnight(tz, args.from)?,
        midnight(tz, args.to + chrono::Duration::days(1))?,
    );
    // PV forecast per lead. One day ahead: the mean of the plant forecast from
    // both short-range weather models. Further ahead those models have no
    // archived runs, so the tail model is used.
    let planes = crate::solar::load_planes(args.planes, false)?;
    let longest = args
        .compare_horizons
        .iter()
        .copied()
        .fold(args.horizon_h, usize::max);
    let span = (start - 86_400, end + (longest as i64 + 24) * 3600);
    let plant = |source: &str| -> Result<HashMap<i64, f64>> {
        let data = crate::solar::load_inputs(store, source, args.lat, args.lon, span.0, span.1)?;
        Ok(crate::solar::plant_hourly(&planes, &data))
    };
    let models = ["knmi_harmonie_arome_netherlands", "icon_d2"];
    let mut day1: HashMap<i64, f64> = HashMap::new();
    for m in models {
        for (h, w) in plant(&format!("open-meteo-day1:{m}"))? {
            *day1.entry(h).or_default() += w / models.len() as f64;
        }
    }
    let mut pv_fc = vec![day1];
    for lead in 2..=longest.div_ceil(24).max(1) {
        let source = format!("open-meteo-day{lead}:{}", args.tail_model);
        pv_fc.push(plant(&source).with_context(|| {
            format!(
                "a {longest} h horizon needs PV weather {lead} days ahead: \
                 run backfill-weather --lead-days {lead} --models {}",
                args.tail_model
            )
        })?);
    }
    let data = ReplayData {
        load: all(crate::load::LOAD_KEY)?,
        pv: all("pv.total.power")?,
        price: all(args.wholesale)?,
        pv_fc,
    };
    let report = simulate(&site, &data, tz, start, end, args.horizon_h, &ALL)?;
    let Report {
        hours,
        tallies,
        daily,
        ..
    } = &report;
    let short_warning = |r: &Report, horizon_h: usize| {
        if r.short_hours > 0 {
            println!(
                "warning: {} of {} hours were planned on less than {horizon_h} h, because a forecast was \
                 missing further ahead; for PV, run backfill-weather --lead-days <N> --models {}",
                r.short_hours, r.hours, args.tail_model
            );
        }
    };
    let b = &site.battery;
    let export_desc = format!("export = {:.3} × wholesale {:+.3}", site.export.a, site.export.b);
    println!(
        "{hours} hours replayed; horizon {} h; {export_desc}; costs include battery wear",
        args.horizon_h
    );
    short_warning(&report, args.horizon_h);
    println!(
        "{:28} {:>10} {:>10} {:>10} {:>10} {:>11} {:>9}  mode hours",
        "strategy", "cost €", "vs self", "vs optim", "import kWh", "export kWh", "cycles"
    );
    let (base, opt) = (
        report.tally(Strategy::SelfConsume)?.cost,
        report.tally(Strategy::Optimizer)?.cost,
    );
    for (name, tl) in report.strategies.iter().map(|s| s.name()).zip(tallies) {
        let modes: Vec<String> = site
            .modes
            .iter()
            .zip(&tl.mode_hours)
            .map(|(m, h)| format!("{}:{h}", m.name))
            .collect();
        println!(
            "{name:28} {:10.2} {:+10.2} {:+10.2} {:10.0} {:11.0} {:9.1}  {}",
            tl.cost,
            tl.cost - base,
            tl.cost - opt,
            tl.import_kwh,
            tl.export_kwh,
            tl.throughput_kwh / 2.0 / (b.capacity_wh / 1000.0),
            modes.join(" ")
        );
    }
    let left_out = match report.partial_days {
        0 => String::new(),
        1 => " (1 partial day left out)".into(),
        n => format!(" ({n} partial days left out)"),
    };
    println!(
        "\n{} complete days replayed{left_out}. What a perfect forecast would save per year \
         (90 % interval, resampled by week):",
        daily.len()
    );
    for &s in &report.strategies {
        let Some(name) = s.foresight() else {
            continue;
        };
        let saving = report.daily_saving(Strategy::Optimizer, s)?;
        if let Some((est, lo, hi)) = yearly_saving(&saving, 7, 2000) {
            println!("  {name:10} {est:8.2}  ({lo:.2} to {hi:.2})");
        }
    }
    if !args.compare_horizons.is_empty() {
        println!(
            "\nOptimizer by horizon: cost, and saving per year against {} h (90 % interval):",
            args.horizon_h
        );
        println!("  {:3} h {:10.2}", args.horizon_h, opt);
        for &h in args.compare_horizons {
            // Only the optimizer is compared, so only the optimizer is replayed.
            let other = simulate(&site, &data, tz, start, end, h, &[Strategy::Optimizer])?;
            let (est, lo, hi) = horizon_saving(&report, &other)?;
            println!(
                "  {h:3} h {:10.2} {est:8.2}  ({lo:.2} to {hi:.2})",
                other.tally(Strategy::Optimizer)?.cost
            );
            short_warning(&other, h);
        }
    }
    Ok(())
}

/// What the optimizer saves per year in `other` against `base`, two replays
/// of the same days: `(estimate, low, high)`.
fn horizon_saving(base: &Report, other: &Report) -> Result<(f64, f64, f64)> {
    if base.daily.len() != other.daily.len() {
        bail!(
            "the horizons replayed different days ({} and {})",
            base.daily.len(),
            other.daily.len()
        );
    }
    let (i, j) = (
        base.index(Strategy::Optimizer)?,
        other.index(Strategy::Optimizer)?,
    );
    let saving: Vec<f64> = base
        .daily
        .iter()
        .zip(&other.daily)
        .map(|(a, b)| a[i] - b[j])
        .collect();
    yearly_saving(&saving, 7, 2000).context("no complete days replayed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use vulpes_core::Behaviour;

    const TZ: Tz = chrono_tz::Europe::Amsterdam;
    const OPT: Strategy = Strategy::Optimizer;
    const PRICE_ORACLE: Strategy = Strategy::Oracle(Oracle::Price);

    fn tally(r: &Report, s: Strategy) -> &Tally {
        r.tally(s).unwrap()
    }

    fn site(rules: bool) -> Site {
        let m = |n: &str, b| ModeDef {
            name: n.into(),
            behaviour: b,
            pv_curtail: false,
            no_export: false,
        };
        Site {
            battery: BatteryParams {
                capacity_wh: 10_000.0,
                max_charge_w: 3000.0,
                max_discharge_w: 3000.0,
                charge_eff: 0.95,
                discharge_eff: 0.95,
                min_soc: 0.1,
                max_soc: 1.0,
                wear_cost_per_kwh: 0.01,
            },
            grid: GridLimits {
                max_import_w: 17_000.0,
                max_export_w: 17_000.0,
            },
            modes: vec![
                m("charge", Behaviour::GridSetpoint { w: 3000.0 }),
                m("hold", Behaviour::NoDischarge),
                m("self", Behaviour::SelfConsume),
                m("discharge", Behaviour::GridSetpoint { w: -3000.0 }),
            ],
            tariff: LinearTariff { a: 1.0, b: 0.0 },
            export: Export { a: 1.0, b: 0.0 },
            rules: rules.then(|| Rules {
                charge_below: 0.15,
                grid_below: 0.25,
                net_zero_below: 0.45,
                charge_max_soc: 0.6,
                discharge_min_soc: 0.3,
                modes: [
                    ("charge", "charge"),
                    ("grid", "hold"),
                    ("net_zero", "self"),
                    ("discharge", "discharge"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            }),
        }
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    /// `days` days from 1 Oct: constant 500 W load, no PV, wholesale 0.10
    /// from 00–06 local time and 0.40 otherwise.
    fn data(days: u32) -> ReplayData {
        let (start, end) = (
            midnight(TZ, day(1)).unwrap(),
            midnight(TZ, day(1 + days)).unwrap(),
        );
        let hours: Vec<i64> = (start..end).step_by(3600).collect();
        let price = |h: i64| {
            let local = TZ.timestamp_opt(h, 0).unwrap();
            if chrono::Timelike::hour(&local) < 6 {
                0.10
            } else {
                0.40
            }
        };
        ReplayData {
            load: hours.iter().map(|&h| (h, 500.0)).collect(),
            pv: hours.iter().map(|&h| (h, 0.0)).collect(),
            price: hours.iter().map(|&h| (h, price(h))).collect(),
            pv_fc: vec![hours.iter().map(|&h| (h, 0.0)).collect(); 4],
        }
    }

    #[test]
    fn tally_settles_import_export_and_wear() {
        let mut t = Tally::default();
        t.settle(2000.0, 1000.0, 0.30, 0.10, 0.02); // import 2 kWh, 1 kWh through the battery
        assert!((t.cost - (0.60 + 0.02)).abs() < 1e-12);
        t.settle(-1000.0, -500.0, 0.30, 0.10, 0.02); // export 1 kWh
        assert!((t.cost - (0.62 - 0.10 + 0.01)).abs() < 1e-12);
        assert_eq!((t.import_kwh, t.export_kwh, t.throughput_kwh), (2.0, 1.0, 1.5));
    }

    #[test]
    fn day_ahead_prices_are_published_at_noon_utc_the_day_before() {
        // 2026-11-08 15:00 UTC is local 16:00 on 8 Nov (UTC+1); local midnight
        // is 7 Nov 23:00 UTC, so the price was known from 7 Nov 11:00 UTC.
        let h = 1_794_150_000; // 2026-11-08 15:00 UTC
        assert_eq!(published_at(TZ, h), 1_794_049_200); // 2026-11-07 11:00 UTC
        // 23:30 UTC on 8 Nov is already 9 Nov locally.
        assert_eq!(published_at(TZ, 1_794_180_600), 1_794_049_200 + 86_400);
    }

    #[test]
    fn terminal_value_is_the_median_import_price_after_losses() {
        let s = |p: f64| SlotInput {
            load_w: 0.0,
            pv_w: 0.0,
            import_price: p,
            export_price: p,
            min_soc_wh: None,
        };
        let v = [s(0.4), s(0.1), s(0.3)];
        assert!((terminal_value(&v, 0.9) - 0.27).abs() < 1e-12);
    }

    #[test]
    fn threshold_rules_cover_every_branch() {
        let site = site(true);
        let r = site.rules.as_ref().unwrap();
        let idx = [0, 1, 2, 3];
        assert_eq!(rule_choice(r, idx, 0.10, 0.5), 0, "cheap and room to charge");
        assert_eq!(rule_choice(r, idx, 0.10, 0.7), 1, "cheap but already charged");
        assert_eq!(rule_choice(r, idx, 0.20, 0.2), 1, "below the grid threshold");
        assert_eq!(rule_choice(r, idx, 0.30, 0.2), 2, "below the net-zero threshold");
        assert_eq!(rule_choice(r, idx, 0.50, 0.5), 3, "expensive and energy to spare");
        assert_eq!(
            rule_choice(r, idx, 0.50, 0.2),
            2,
            "expensive but at the discharge floor"
        );
    }

    #[test]
    fn site_prices() {
        let mut s = site(false);
        s.tariff = LinearTariff { a: 1.21, b: 0.13 };
        s.export = Export { a: 1.1, b: -0.02 };
        assert!((s.import_price(0.1) - 0.251).abs() < 1e-12);
        assert!((s.export_price(0.1) - 0.09).abs() < 1e-12);
        // Net metering: the export tariff equals the import tariff.
        s.export = Export { a: 1.21, b: 0.13 };
        assert_eq!(s.export_price(0.1), s.import_price(0.1));
        assert!(s.mode_index("hold").is_ok() && s.mode_index("nope").is_err());
    }

    #[test]
    fn export_rejects_the_former_net_metering_switch() {
        let ok: Export = serde_json::from_str(r#"{"a": 1.0, "b": -0.02}"#).unwrap();
        assert_eq!((ok.a, ok.b), (1.0, -0.02));
        let err = serde_json::from_str::<Export>(r#"{"net_metering": true, "a": 1.0, "b": 0.0}"#)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("net_metering"), "{err}");
    }

    #[test]
    fn optimizer_exploits_the_daily_spread() {
        let site = site(true);
        let (start, end) = (midnight(TZ, day(15)).unwrap(), midnight(TZ, day(22)).unwrap());
        let r = simulate(&site, &data(25), TZ, start, end, 36, &ALL).unwrap();
        assert_eq!(r.hours, 7 * 24);
        let [own, rules, opt, perfect] =
            [Strategy::SelfConsume, Strategy::Rules, OPT, Strategy::Perfect].map(|s| tally(&r, s).cost);
        assert!(opt < own - 1.0, "optimizer {opt} vs self-consumption {own}");
        assert!(opt <= rules + 1e-9, "optimizer {opt} vs rules {rules}");
        // Load, PV and prices repeat exactly, so the forecasts are perfect too.
        assert!(
            (opt - perfect).abs() < 0.05,
            "optimizer {opt} vs perfect {perfect}"
        );
        assert!(tally(&r, OPT).mode_hours[0] > 0, "charges in the cheap hours");
        // With perfect forecasts, knowing one actual input changes nothing.
        let oracles: Vec<_> = r
            .strategies
            .iter()
            .zip(&r.tallies)
            .filter(|(s, _)| matches!(s, Strategy::Oracle(_)))
            .collect();
        assert_eq!(oracles.len(), 3);
        for (s, t) in oracles {
            assert!((t.cost - opt).abs() < 0.05, "{}: {} vs {opt}", s.name(), t.cost);
        }
        // One row of costs per local day, adding up to the totals.
        assert_eq!(r.daily.len(), 7);
        for (k, t) in r.tallies.iter().enumerate() {
            let sum: f64 = r.daily.iter().map(|d| d[k]).sum();
            assert!(
                (sum - t.cost).abs() < 1e-9,
                "{}: {sum} vs {}",
                r.strategies[k].name(),
                t.cost
            );
        }
        assert!(
            r.tallies
                .iter()
                .all(|t| t.mode_hours.iter().sum::<usize>() == r.hours)
        );
    }

    #[test]
    fn unpublished_prices_do_not_leak_into_earlier_decisions() {
        let site = site(true);
        let base = data(25);
        // A price spike on 20 Oct evening, published 19 Oct 12:00 UTC.
        let spike_hour = midnight(TZ, day(20)).unwrap() + 19 * 3600;
        let mut spiked = data(25);
        spiked.price.insert(spike_hour, 5.0);
        let published = published_at(TZ, spike_hour);
        let start = midnight(TZ, day(16)).unwrap();
        let before = |d: &ReplayData| simulate(&site, d, TZ, start, published, 36, &ALL).unwrap();
        let (a, b) = (before(&base), before(&spiked));
        assert_eq!(
            tally(&a, OPT),
            tally(&b, OPT),
            "optimizer saw the spike before publication"
        );
        // Once published, the optimizer reacts to it (perfect foresight always could).
        let after = |d: &ReplayData| simulate(&site, d, TZ, start, spike_hour + 3600, 36, &ALL).unwrap();
        assert_ne!(tally(&after(&base), OPT), tally(&after(&spiked), OPT));
    }

    #[test]
    fn oracle_slot_swaps_only_its_own_input() {
        let fc = SlotInput {
            load_w: 1.0,
            pv_w: 2.0,
            import_price: 3.0,
            export_price: 4.0,
            min_soc_wh: Some(5.0),
        };
        let actual = SlotInput {
            load_w: 10.0,
            pv_w: 20.0,
            import_price: 30.0,
            export_price: 40.0,
            min_soc_wh: None,
        };
        let load = SlotInput { load_w: 10.0, ..fc };
        let pv = SlotInput { pv_w: 20.0, ..fc };
        let price = SlotInput {
            import_price: 30.0,
            export_price: 40.0,
            ..fc
        };
        assert_eq!(oracle_slot(&fc, &actual, Oracle::Load), load);
        assert_eq!(oracle_slot(&fc, &actual, Oracle::Pv), pv);
        assert_eq!(oracle_slot(&fc, &actual, Oracle::Price), price);
    }

    /// Hour of the day in local time.
    fn local_hour(h: i64) -> u32 {
        chrono::Timelike::hour(&TZ.timestamp_opt(h, 0).unwrap())
    }

    #[test]
    fn load_oracle_gains_when_the_load_forecast_is_wrong() {
        // Export earns nothing, and there is an evening peak. The history has
        // a small load, so the forecast expects the battery to last all day.
        // From 15 Oct the load is ten times higher: the battery then has to be
        // saved for the peak, which only the load oracle knows.
        let mut site = site(false);
        site.export = Export { a: 0.0, b: 0.0 };
        let mut d = data(25);
        let start = midnight(TZ, day(15)).unwrap();
        for (&h, p) in d.price.iter_mut() {
            if (17..21).contains(&local_hour(h)) {
                *p = 0.60;
            }
        }
        for (&h, l) in d.load.iter_mut() {
            *l = if h < start { 100.0 } else { 1000.0 };
        }
        let r = simulate(&site, &d, TZ, start, start + 7 * 86_400, 36, &ALL).unwrap();
        let cost = |s: Strategy| tally(&r, s).cost;
        let (opt, load) = (cost(OPT), cost(Strategy::Oracle(Oracle::Load)));
        assert!(load < opt - 1.0, "load oracle {load} vs optimizer {opt}");
        assert!(
            cost(Strategy::Perfect) <= load + 0.05,
            "perfect {} vs {load}",
            cost(Strategy::Perfect)
        );
        // PV is forecast exactly (zero), so its oracle decides like the optimizer.
        assert_eq!(tally(&r, Strategy::Oracle(Oracle::Pv)), tally(&r, OPT));
        // Prices repeat, so the price oracle has nothing to add.
        assert!((cost(PRICE_ORACLE) - opt).abs() < 0.05);
    }

    #[test]
    fn price_oracle_sees_prices_before_they_are_published() {
        // Mornings pay best, so the battery is normally sold empty then, and
        // buying it back in the afternoon doesn't pay. On 20 Oct the night
        // pays more than the morning instead of being cheap; that is published
        // on 19 Oct 12:00 UTC, after the morning of the 19th.
        let site = site(false);
        let mornings = |mut d: ReplayData| {
            for (&h, p) in d.price.iter_mut() {
                match local_hour(h) {
                    0..6 => {}
                    6..12 => *p = 0.50,
                    _ => *p = 0.48,
                }
            }
            d
        };
        let base = mornings(data(25));
        let mut changed = mornings(data(25));
        let night = midnight(TZ, day(20)).unwrap();
        for h in (night..night + 6 * 3600).step_by(3600) {
            changed.price.insert(h, 0.60);
        }
        let (start, published) = (midnight(TZ, day(16)).unwrap(), published_at(TZ, night));
        let before = |d: &ReplayData| simulate(&site, d, TZ, start, published, 36, &ALL).unwrap();
        let (a, b) = (before(&base), before(&changed));
        assert_eq!(tally(&a, OPT), tally(&b, OPT), "optimizer saw it early");
        assert_ne!(
            tally(&a, PRICE_ORACLE),
            tally(&b, PRICE_ORACLE),
            "the price oracle should already keep the battery for the better-paid night"
        );
    }

    #[test]
    fn oracles_fall_back_to_forecasts_without_actuals_in_the_horizon() {
        let site = site(true);
        let mut d = data(25);
        let start = midnight(TZ, day(15)).unwrap();
        // An hour after the replayed day, but inside every decision's horizon.
        d.load.remove(&(start + 30 * 3600));
        let r = simulate(&site, &d, TZ, start, start + 24 * 3600, 36, &ALL).unwrap();
        assert_eq!(r.hours, 24);
        for (s, t) in r.strategies.iter().zip(&r.tallies) {
            if s.foresight().is_some() {
                assert_eq!(t, tally(&r, OPT), "{}", s.name());
            }
        }
    }

    #[test]
    fn forecast_lead_is_the_freshest_one_already_issued() {
        let (t, day) = (1_794_150_000, 86_400);
        assert_eq!(lead_days(t, t), 1, "the current hour");
        assert_eq!(lead_days(t, t - 3600), 1, "never below one day");
        assert_eq!(lead_days(t, t + day - 3600), 1);
        assert_eq!(lead_days(t, t + day), 1, "issued exactly now");
        assert_eq!(
            lead_days(t, t + day + 3600),
            2,
            "the day-ahead run for it is not out yet"
        );
        assert_eq!(lead_days(t, t + 2 * day), 2);
        assert_eq!(lead_days(t, t + 83 * 3600), 4);
        // Each lead has its own forecast; a missing lead or hour gives none.
        let h = t + day + 3600;
        let fc = vec![
            HashMap::from([(h, 100.0), (t, 10.0)]),
            HashMap::from([(h, 200.0)]),
        ];
        assert_eq!(pv_forecast(&fc, t, t), Some(10.0));
        assert_eq!(pv_forecast(&fc, t, h), Some(200.0));
        assert_eq!(pv_forecast(&fc, t + 3600, h), Some(100.0), "a day ahead by now");
        assert_eq!(pv_forecast(&fc, t, t + 3600), None);
        assert_eq!(pv_forecast(&fc, t, t + 2 * day + 3600), None);
        assert_eq!(pv_forecast(&[], t, t), None);
    }

    #[test]
    fn day_ahead_pv_forecasts_do_not_leak_into_earlier_decisions() {
        // Export earns nothing, so sun at midday replaces charging at night.
        // The day-ahead forecast for 20 Oct shows that sun; the forecasts
        // issued earlier show none.
        let mut site = site(false);
        site.export = Export { a: 0.0, b: 0.0 };
        let base = data(25);
        let mut sunny = data(25);
        let noon = midnight(TZ, day(20)).unwrap() + 12 * 3600;
        for h in (noon..noon + 3 * 3600).step_by(3600) {
            sunny.pv_fc[0].insert(h, 5000.0);
        }
        let start = midnight(TZ, day(16)).unwrap();
        let until = |d: &ReplayData, end: i64| simulate(&site, d, TZ, start, end, 36, &ALL).unwrap();
        let issued = noon - 86_400;
        assert_eq!(
            tally(&until(&base, issued), OPT),
            tally(&until(&sunny, issued), OPT),
            "optimizer saw the day-ahead forecast before it was issued"
        );
        assert_ne!(tally(&until(&base, noon), OPT), tally(&until(&sunny, noon), OPT));
    }

    #[test]
    fn horizon_stops_where_the_pv_forecast_lead_is_missing() {
        let site = site(true);
        let (start, end) = (midnight(TZ, day(15)).unwrap(), midnight(TZ, day(18)).unwrap());
        let mut one_lead = data(25);
        one_lead.pv_fc.truncate(1);
        // With only day-ahead forecasts, 36 h plans on the 25 hours they cover.
        let short = simulate(&site, &one_lead, TZ, start, end, 36, &ALL).unwrap();
        let full = simulate(&site, &data(25), TZ, start, end, 25, &ALL).unwrap();
        assert_eq!(short.tallies, full.tallies);
        assert_eq!(short.hours, 72);
        // Every hour was planned on less than the 36 hours asked for, and the report says so.
        assert_eq!((short.short_hours, full.short_hours), (72, 0));
    }

    #[test]
    fn strategies_have_distinct_names_and_are_found_by_kind() {
        let names: std::collections::HashSet<_> = ALL.iter().map(|s| s.name()).collect();
        assert_eq!(names.len(), ALL.len());
        let labels: Vec<_> = ALL.iter().filter_map(|s| s.foresight()).collect();
        assert_eq!(labels, ["load", "PV", "prices", "all three"]);
        let site = site(true);
        let start = midnight(TZ, day(15)).unwrap();
        let r = simulate(&site, &data(25), TZ, start, start + 86_400, 36, &ALL).unwrap();
        for (k, &s) in ALL.iter().enumerate() {
            assert_eq!(r.index(s).unwrap(), k);
        }
        let only = simulate(&site, &data(25), TZ, start, start + 86_400, 36, &[OPT]).unwrap();
        let err = only.tally(Strategy::Perfect).unwrap_err().to_string();
        assert!(err.contains("perfect foresight"), "{err}");
        assert!(only.daily_saving(OPT, Strategy::Rules).is_err());
    }

    #[test]
    fn a_single_strategy_replays_like_it_does_among_the_others() {
        let site = site(true);
        let (start, end) = (midnight(TZ, day(15)).unwrap(), midnight(TZ, day(18)).unwrap());
        let all = simulate(&site, &data(25), TZ, start, end, 36, &ALL).unwrap();
        for s in [OPT, Strategy::Rules, Strategy::Perfect, PRICE_ORACLE] {
            let one = simulate(&site, &data(25), TZ, start, end, 36, &[s]).unwrap();
            assert_eq!(one.tallies.len(), 1);
            assert_eq!(tally(&one, s), tally(&all, s), "{}", s.name());
            assert_eq!(one.daily_saving(s, s).unwrap(), vec![0.0; 3]);
            let k = all.index(s).unwrap();
            let daily: Vec<f64> = all.daily.iter().map(|d| d[k]).collect();
            let alone: Vec<f64> = one.daily.iter().map(|d| d[0]).collect();
            assert_eq!(alone, daily, "{}", s.name());
            assert_eq!((one.hours, one.short_hours), (all.hours, all.short_hours));
        }
    }

    #[test]
    fn day_length_follows_the_clock_changes() {
        assert_eq!(day_hours(TZ, day(15)).unwrap(), 24);
        assert_eq!(day_hours(TZ, day(25)).unwrap(), 25);
        let spring = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        assert_eq!(day_hours(TZ, spring).unwrap(), 23);
    }

    #[test]
    fn only_whole_days_count_for_the_yearly_figures() {
        let site = site(true);
        let start = midnight(TZ, day(15)).unwrap();
        // From noon: half a day, then a whole one.
        let r = simulate(
            &site,
            &data(25),
            TZ,
            start + 12 * 3600,
            start + 48 * 3600,
            36,
            &ALL,
        )
        .unwrap();
        assert_eq!((r.hours, r.daily.len(), r.partial_days), (36, 1, 1));
        // The totals still cover every replayed hour.
        let whole_day: f64 = r.daily[0][r.index(OPT).unwrap()];
        assert!((whole_day - tally(&r, OPT).cost).abs() > 1e-6);
        // A day with a skipped hour in the middle is partial too.
        let mut gap = data(25);
        gap.load.remove(&(start + 30 * 3600));
        let r = simulate(&site, &gap, TZ, start, start + 72 * 3600, 36, &ALL).unwrap();
        assert_eq!((r.hours, r.daily.len(), r.partial_days), (71, 2, 1));
        // The 25-hour day of the clock change is a whole day.
        let (from, to) = (midnight(TZ, day(25)).unwrap(), midnight(TZ, day(26)).unwrap());
        let r = simulate(&site, &data(27), TZ, from, to, 36, &ALL).unwrap();
        assert_eq!((r.hours, r.daily.len(), r.partial_days), (25, 1, 0));
    }

    #[test]
    fn horizon_saving_compares_the_optimizer_day_by_day() {
        let report = |cost: f64, days: usize| Report {
            strategies: vec![OPT],
            hours: days * 24,
            short_hours: 0,
            tallies: Vec::new(),
            daily: vec![vec![cost]; days],
            partial_days: 0,
        };
        let (est, lo, hi) = horizon_saving(&report(1.0, 30), &report(0.9, 30)).unwrap();
        for v in [est, lo, hi] {
            assert!((v - 36.5).abs() < 1e-9, "{v}");
        }
        let (worse, ..) = horizon_saving(&report(0.9, 30), &report(1.0, 30)).unwrap();
        assert!((worse + 36.5).abs() < 1e-9);
        let err = horizon_saving(&report(1.0, 30), &report(1.0, 29)).unwrap_err();
        assert!(err.to_string().contains("different days"), "{err}");
        assert!(horizon_saving(&report(1.0, 0), &report(1.0, 0)).is_err());
    }

    #[test]
    fn yearly_saving_scales_to_a_year_with_an_interval() {
        assert_eq!(yearly_saving(&[], 7, 100), None);
        assert_eq!(yearly_saving(&[1.0], 0, 100), None);
        assert_eq!(yearly_saving(&[1.0], 7, 0), None);
        // A constant saving has no spread, also with a short last block.
        let (est, lo, hi) = yearly_saving(&[0.1; 30], 7, 200).unwrap();
        for v in [est, lo, hi] {
            assert!((v - 36.5).abs() < 1e-9, "{v}");
        }
        // A single day is its own estimate.
        assert_eq!(yearly_saving(&[2.0], 7, 50), Some((730.0, 730.0, 730.0)));
        // Weeks alternating between 0 and 1 per day: the estimate is the mean,
        // the interval lies around it and inside the possible range.
        let weeks: Vec<f64> = (0..140).map(|d| ((d / 7) % 2) as f64).collect();
        let (est, lo, hi) = yearly_saving(&weeks, 7, 2000).unwrap();
        assert!((est - 182.5).abs() < 1e-9);
        assert!(
            0.0 <= lo && lo < est && est < hi && hi <= 365.0,
            "{lo} {est} {hi}"
        );
        assert!(hi - lo > 30.0, "interval too narrow: {lo} to {hi}");
        // Negative savings keep their sign, and the result is reproducible.
        let neg: Vec<f64> = weeks.iter().map(|x| -x).collect();
        let (est_n, lo_n, hi_n) = yearly_saving(&neg, 7, 2000).unwrap();
        assert!((est_n + 182.5).abs() < 1e-9 && lo_n < est_n && est_n < hi_n && hi_n <= 0.0);
        assert_eq!(yearly_saving(&weeks, 7, 2000), Some((est, lo, hi)));
    }

    #[test]
    fn hours_with_missing_data_are_skipped() {
        let site = site(true);
        let mut d = data(25);
        let start = midnight(TZ, day(15)).unwrap();
        d.load.remove(&(start + 5 * 3600));
        d.pv.remove(&(start + 6 * 3600));
        d.price.remove(&(start + 7 * 3600));
        let r = simulate(&site, &d, TZ, start, start + 24 * 3600, 36, &ALL).unwrap();
        assert_eq!(r.hours, 21);
        // The day is incomplete, so it gives no daily cost to scale to a year.
        assert_eq!((r.daily.len(), r.partial_days), (0, 1));
        let empty = ReplayData {
            load: HashMap::new(),
            pv: HashMap::new(),
            price: HashMap::new(),
            pv_fc: Vec::new(),
        };
        let err = simulate(&site, &empty, TZ, start, start + 86_400, 36, &ALL)
            .err()
            .unwrap();
        assert!(err.to_string().contains("no hours with complete data"));
    }

    #[test]
    fn configuration_errors() {
        let start = midnight(TZ, day(15)).unwrap();
        let mut s = site(true);
        s.rules
            .as_mut()
            .unwrap()
            .modes
            .insert("grid".into(), "missing".into());
        assert!(simulate(&s, &data(16), TZ, start, start + 3600, 36, &ALL).is_err());
        let mut s = site(true);
        s.rules.as_mut().unwrap().modes.remove("discharge");
        assert!(simulate(&s, &data(16), TZ, start, start + 3600, 36, &ALL).is_err());
        // Without rules, the baseline needs a self-consume mode.
        let mut s = site(false);
        s.modes.retain(|m| m.behaviour != Behaviour::SelfConsume);
        assert!(simulate(&s, &data(16), TZ, start, start + 3600, 36, &ALL).is_err());
        // Limits that `step` would otherwise apply without complaint.
        let invalid = |change: fn(&mut Site)| {
            let mut s = site(true);
            change(&mut s);
            let err = simulate(&s, &data(16), TZ, start, start + 3600, 36, &ALL).err();
            err.expect("invalid site").to_string()
        };
        assert!(invalid(|s| s.grid.max_import_w = -1.0).contains("grid limits"));
        assert!(invalid(|s| s.grid.max_export_w = -17_000.0).contains("grid limits"));
        assert!(invalid(|s| s.grid.max_import_w = f64::NAN).contains("grid limits"));
        assert!(invalid(|s| s.battery.capacity_wh = 0.0).contains("battery"));
        assert!(invalid(|s| s.export.a = f64::NAN).contains("export"));
        assert!(invalid(|s| s.export.b = f64::INFINITY).contains("export"));
    }
}
