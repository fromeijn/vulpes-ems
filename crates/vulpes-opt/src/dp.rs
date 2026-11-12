//! Dynamic programming over (slot × discretised state of charge × mode).
//!
//! Backward induction with linear interpolation of the value function between
//! SOC grid points. Costs: import − export revenue + battery wear; the energy
//! left at the end is credited at `terminal_value_per_kwh` (expected future
//! price × discharge efficiency), so there is no fixed end-SOC target.

use crate::physics::step;
use vulpes_core::{BatteryParams, Behaviour, GridLimits, ModeDef};

/// Forecasts and prices for one slot. Prices in currency/kWh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotInput {
    pub load_w: f64,
    pub pv_w: f64,
    pub import_price: f64,
    pub export_price: f64,
    /// Minimum SOC (Wh) at the end of this slot (reserve), if any.
    pub min_soc_wh: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Problem<'a> {
    pub slots: &'a [SlotInput],
    pub dt_h: f64,
    pub modes: &'a [ModeDef],
    pub battery: &'a BatteryParams,
    pub grid: &'a GridLimits,
    pub soc0_wh: f64,
    pub terminal_value_per_kwh: f64,
    /// SOC grid resolution (Wh).
    pub soc_step_wh: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanSlot {
    pub mode: usize,
    pub battery_w: f64,
    pub grid_w: f64,
    pub soc_wh: f64,
    pub cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub slots: Vec<PlanSlot>,
    /// Total cost including wear, excluding the terminal credit.
    pub cost: f64,
}

/// Cost of one slot outcome.
fn slot_cost(inp: &SlotInput, grid_w: f64, battery_w: f64, dt_h: f64, wear: f64) -> f64 {
    let kwh = grid_w * dt_h / 1000.0;
    let energy = if kwh >= 0.0 {
        kwh * inp.import_price
    } else {
        kwh * inp.export_price
    };
    energy + wear * battery_w.abs() * dt_h / 1000.0
}

/// Penalty per Wh below the reserve: large enough to dominate any price.
const RESERVE_PENALTY_PER_WH: f64 = 10.0;

/// Scores closer than this count as equal.
const TIE_EPS: f64 = 1e-9;

/// How much a mode intervenes. Among modes with an equal score the least
/// active one is planned (and among those the first in the list): a full
/// battery gives "charge" and "hold" the same outcome, but if the forecast
/// is off, the passive mode does less harm.
fn activity(mode: &ModeDef) -> u8 {
    let powered = matches!(
        mode.behaviour,
        Behaviour::GridSetpoint { .. } | Behaviour::BatteryPower { .. }
    );
    2 * u8::from(powered) + u8::from(mode.pv_curtail || mode.no_export)
}

pub fn optimize(p: &Problem<'_>) -> Plan {
    let b = p.battery;
    let (lo, hi) = (b.min_soc * b.capacity_wh, b.max_soc * b.capacity_wh);
    let n = (((hi - lo) / p.soc_step_wh).ceil() as usize).max(1) + 1;
    let level = |i: usize| (lo + i as f64 * (hi - lo) / (n - 1) as f64).min(hi);
    // Value at a continuous SOC by linear interpolation of the next slot's values.
    let interp = |v: &[f64], soc: f64| {
        let x = ((soc - lo) / (hi - lo) * (n - 1) as f64).clamp(0.0, (n - 1) as f64);
        let i = (x.floor() as usize).min(n - 2);
        let f = x - i as f64;
        v[i] * (1.0 - f) + v[i + 1] * f
    };
    let t_len = p.slots.len();
    let mut value = vec![vec![0.0; n]; t_len + 1];
    for (i, v) in value[t_len].iter_mut().enumerate() {
        *v = -(level(i) - lo) / 1000.0 * p.terminal_value_per_kwh;
    }
    let reserve_penalty = |inp: &SlotInput, soc: f64| {
        inp.min_soc_wh
            .map_or(0.0, |r| (r - soc).max(0.0) * RESERVE_PENALTY_PER_WH)
    };
    for t in (0..t_len).rev() {
        let inp = &p.slots[t];
        let (cur, next) = value.split_at_mut(t + 1);
        let next = &next[0];
        for (i, v) in cur[t].iter_mut().enumerate() {
            *v = p
                .modes
                .iter()
                .map(|mode| {
                    let o = step(mode, inp.load_w, inp.pv_w, level(i), b, p.grid, p.dt_h);
                    slot_cost(inp, o.grid_w, o.battery_w, p.dt_h, b.wear_cost_per_kwh)
                        + reserve_penalty(inp, o.soc_wh)
                        + interp(next, o.soc_wh)
                })
                .fold(f64::INFINITY, f64::min);
        }
    }
    // Forward pass from the actual (continuous) SOC, choosing the best mode
    // against the value function at each step.
    let mut soc = p.soc0_wh.clamp(lo, hi);
    let mut slots = Vec::with_capacity(t_len);
    let mut total = 0.0;
    for t in 0..t_len {
        let inp = &p.slots[t];
        let mut best: Option<(f64, usize, crate::SlotOutcome, f64)> = None;
        for (m, mode) in p.modes.iter().enumerate() {
            let o = step(mode, inp.load_w, inp.pv_w, soc, b, p.grid, p.dt_h);
            let c = slot_cost(inp, o.grid_w, o.battery_w, p.dt_h, b.wear_cost_per_kwh);
            let score = c + reserve_penalty(inp, o.soc_wh) + interp(&value[t + 1], o.soc_wh);
            let better = best.as_ref().is_none_or(|x| {
                score < x.0 - TIE_EPS || (score <= x.0 + TIE_EPS && activity(mode) < activity(&p.modes[x.1]))
            });
            if better {
                best = Some((score, m, o, c));
            }
        }
        let (_, m, o, c) = best.expect("at least one mode");
        total += c;
        soc = o.soc_wh;
        slots.push(PlanSlot {
            mode: m,
            battery_w: o.battery_w,
            grid_w: o.grid_w,
            soc_wh: o.soc_wh,
            cost: c,
        });
    }
    Plan { slots, cost: total }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vulpes_core::Behaviour;

    fn bat(wear: f64) -> BatteryParams {
        BatteryParams {
            capacity_wh: 10_000.0,
            max_charge_w: 3000.0,
            max_discharge_w: 3000.0,
            charge_eff: 0.95,
            discharge_eff: 0.95,
            min_soc: 0.1,
            max_soc: 1.0,
            wear_cost_per_kwh: wear,
        }
    }
    const GRID: GridLimits = GridLimits {
        max_import_w: 17_000.0,
        max_export_w: 17_000.0,
    };
    fn modes() -> Vec<ModeDef> {
        let m = |n: &str, b| ModeDef {
            name: n.into(),
            behaviour: b,
            pv_curtail: false,
            no_export: false,
        };
        vec![
            m("Charging", Behaviour::GridSetpoint { w: 3000.0 }),
            m("Grid", Behaviour::NoDischarge),
            m("Net Zero", Behaviour::SelfConsume),
            m("Discharging", Behaviour::GridSetpoint { w: -3000.0 }),
        ]
    }
    fn slot(load: f64, pv: f64, price: f64) -> SlotInput {
        SlotInput {
            load_w: load,
            pv_w: pv,
            import_price: price,
            export_price: price,
            min_soc_wh: None,
        }
    }
    fn solve(slots: &[SlotInput], soc0: f64, wear: f64, terminal: f64) -> Plan {
        let (b, m) = (bat(wear), modes());
        optimize(&Problem {
            slots,
            dt_h: 1.0,
            modes: &m,
            battery: &b,
            grid: &GRID,
            soc0_wh: soc0,
            terminal_value_per_kwh: terminal,
            soc_step_wh: 100.0,
        })
    }

    #[test]
    fn arbitrage_when_spread_pays() {
        // Cheap night, expensive evening: charge first, use it later.
        let mut s = vec![slot(300.0, 0.0, 0.10); 3];
        s.extend(vec![slot(1500.0, 0.0, 0.50); 3]);
        let plan = solve(&s, 1000.0, 0.02, 0.0);
        assert_eq!(
            plan.slots[0].mode, 0,
            "should charge in the cheap hours: {plan:?}"
        );
        assert!(
            plan.slots[3..].iter().all(|x| x.grid_w < 1.0),
            "evening should run from the battery"
        );
    }

    #[test]
    fn no_arbitrage_when_spread_is_below_losses_and_wear() {
        let mut s = vec![slot(300.0, 0.0, 0.30); 3];
        s.extend(vec![slot(1500.0, 0.0, 0.32); 3]);
        let plan = solve(&s, 1000.0, 0.02, 0.0);
        assert!(plan.slots[..3].iter().all(|x| x.mode != 0), "{plan:?}");
    }

    #[test]
    fn terminal_value_keeps_energy_for_later() {
        // Flat cheap prices, but energy is worth more after the horizon: do not dump it.
        let s = vec![slot(500.0, 0.0, 0.20); 4];
        let keep = solve(&s, 9000.0, 0.0, 0.40);
        assert!(keep.slots.iter().all(|x| x.battery_w >= -1e-6), "{keep:?}");
        let spend = solve(&s, 9000.0, 0.0, 0.0);
        assert!(spend.slots.iter().any(|x| x.battery_w < -1.0));
    }

    #[test]
    fn discharge_for_export_stays_within_the_export_limit() {
        // A high price and nothing after the horizon: sell as fast as allowed.
        let s = vec![slot(200.0, 0.0, 0.60); 3];
        let (b, m) = (bat(0.0), modes());
        let solve_with = |max_export_w: f64| {
            optimize(&Problem {
                slots: &s,
                dt_h: 1.0,
                modes: &m,
                battery: &b,
                grid: &GridLimits {
                    max_import_w: 17_000.0,
                    max_export_w,
                },
                soc0_wh: 9000.0,
                terminal_value_per_kwh: 0.0,
                soc_step_wh: 100.0,
            })
        };
        let free = solve_with(17_000.0);
        // The battery's 3 kW less the 200 W load.
        assert!((free.slots[0].grid_w + 2800.0).abs() < 1e-9, "{free:?}");
        let limited = solve_with(800.0);
        assert!(
            limited.slots.iter().all(|x| x.grid_w >= -800.0 - 1e-9),
            "{limited:?}"
        );
        assert!((limited.slots[0].battery_w + 1000.0).abs() < 1e-9, "{limited:?}");
        // Less can be sold, so the limited plan earns less.
        assert!(limited.cost > free.cost);
    }

    #[test]
    fn reserve_is_respected() {
        let mut s = vec![slot(1000.0, 0.0, 0.50); 4];
        for x in &mut s {
            x.min_soc_wh = Some(6000.0);
        }
        let plan = solve(&s, 8000.0, 0.0, 0.0);
        assert!(plan.slots.iter().all(|x| x.soc_wh >= 6000.0 - 1.0), "{plan:?}");
    }

    #[test]
    fn pre_empties_battery_before_sunny_day_with_cheap_midday() {
        // Full battery; expensive evening now, then a sunny low-price midday that refills it.
        let mut s = vec![slot(800.0, 0.0, 0.45); 3]; // evening: expensive
        s.extend(vec![slot(300.0, 0.0, 0.25); 6]); // night
        s.extend(vec![slot(400.0, 4000.0, 0.05); 5]); // sunny cheap midday: 18 kWh surplus
        let plan = solve(&s, 10_000.0, 0.01, 0.20);
        let exported_evening: f64 = plan.slots[..3].iter().map(|x| (-x.grid_w).max(0.0)).sum();
        assert!(
            exported_evening > 1000.0,
            "should export in the expensive evening: {plan:?}"
        );
        assert!(plan.slots.last().unwrap().soc_wh > 9000.0, "refilled by PV");
    }

    #[test]
    fn zero_export_is_chosen_only_when_export_costs_money() {
        let b = bat(0.0);
        let sc = modes()[2].clone();
        let m = vec![
            sc.clone(),
            ModeDef {
                name: "Net Zero:no_export".into(),
                no_export: true,
                ..sc
            },
        ];
        // Full battery, sunny: the surplus goes to the grid unless export is blocked.
        let plan = |export_price: f64| {
            let s = vec![
                SlotInput {
                    export_price,
                    ..slot(500.0, 3000.0, 0.20)
                };
                3
            ];
            optimize(&Problem {
                slots: &s,
                dt_h: 1.0,
                modes: &m,
                battery: &b,
                grid: &GRID,
                soc0_wh: 10_000.0,
                terminal_value_per_kwh: 0.0,
                soc_step_wh: 100.0,
            })
        };
        let negative = plan(-0.05);
        assert!(
            negative.slots.iter().all(|x| x.mode == 1 && x.grid_w == 0.0),
            "{negative:?}"
        );
        assert_eq!(negative.cost, 0.0);
        // Export paid at the (positive) import price: keep exporting.
        let paid = plan(0.20);
        assert!(
            paid.slots.iter().all(|x| x.mode == 0 && x.grid_w < 0.0),
            "{paid:?}"
        );
    }

    #[test]
    fn a_tie_goes_to_the_passive_mode() {
        // Full battery, no sun, flat prices, energy worth keeping: charging from
        // the grid and holding have the same outcome. Plan "hold" (index 1),
        // although "charge" comes first in the list.
        let s = vec![slot(500.0, 0.0, 0.20); 4];
        let plan = solve(&s, 10_000.0, 0.0, 0.40);
        assert!(
            plan.slots.iter().all(|x| x.mode == 1 && x.battery_w == 0.0),
            "{plan:?}"
        );
        // A real difference still wins over the preference: cheap now, dear later.
        let mut s = vec![slot(300.0, 0.0, 0.10); 3];
        s.extend(vec![slot(1500.0, 0.0, 0.50); 3]);
        assert_eq!(solve(&s, 1000.0, 0.02, 0.0).slots[0].mode, 0);
    }

    #[test]
    fn ties_prefer_plain_modes_and_then_list_order() {
        let plain = |b| ModeDef {
            name: "m".into(),
            behaviour: b,
            pv_curtail: false,
            no_export: false,
        };
        let variant = ModeDef {
            no_export: true,
            ..plain(Behaviour::SelfConsume)
        };
        assert!(activity(&plain(Behaviour::Idle)) < activity(&variant));
        assert!(activity(&variant) < activity(&plain(Behaviour::BatteryPower { w: 0.0 })));
        assert_eq!(
            activity(&plain(Behaviour::NoDischarge)),
            activity(&plain(Behaviour::SelfConsume))
        );
        // Night, empty battery: hold and self-consume both do nothing; the first listed wins.
        let b = bat(0.0);
        let m = vec![
            variant,
            plain(Behaviour::NoDischarge),
            plain(Behaviour::SelfConsume),
        ];
        let s = vec![slot(500.0, 0.0, 0.3); 3];
        let plan = optimize(&Problem {
            slots: &s,
            dt_h: 1.0,
            modes: &m,
            battery: &b,
            grid: &GRID,
            soc0_wh: 1000.0,
            terminal_value_per_kwh: 0.0,
            soc_step_wh: 100.0,
        });
        assert!(plan.slots.iter().all(|x| x.mode == 1), "{plan:?}");
    }

    #[test]
    fn empty_horizon_gives_an_empty_plan() {
        let plan = solve(&[], 5000.0, 0.0, 0.0);
        assert!(plan.slots.is_empty() && plan.cost == 0.0);
    }

    #[test]
    fn a_single_mode_is_always_chosen() {
        let (b, m) = (bat(0.0), vec![modes()[2].clone()]);
        let s = vec![slot(500.0, 0.0, 0.3); 4];
        let plan = optimize(&Problem {
            slots: &s,
            dt_h: 1.0,
            modes: &m,
            battery: &b,
            grid: &GRID,
            soc0_wh: 5000.0,
            terminal_value_per_kwh: 0.0,
            soc_step_wh: 100.0,
        });
        assert!(plan.slots.iter().all(|x| x.mode == 0));
    }

    #[test]
    fn plan_cost_is_the_sum_of_slot_costs() {
        let mut s = vec![slot(300.0, 0.0, 0.10); 3];
        s.extend(vec![slot(1500.0, 2000.0, 0.50); 3]);
        let plan = solve(&s, 3000.0, 0.02, 0.2);
        let sum: f64 = plan.slots.iter().map(|x| x.cost).sum();
        assert!((plan.cost - sum).abs() < 1e-9);
    }

    #[test]
    fn start_soc_outside_the_limits_is_clamped() {
        let b = bat(0.0);
        let s = vec![slot(0.0, 0.0, 0.3); 2];
        for soc0 in [-500.0, 20_000.0] {
            let plan = solve(&s, soc0, 0.0, 0.0);
            assert!(
                plan.slots
                    .iter()
                    .all(|x| (b.min_soc * b.capacity_wh..=b.capacity_wh).contains(&x.soc_wh))
            );
        }
    }

    #[test]
    fn quarter_hour_slots_scale_costs() {
        let s = vec![slot(1000.0, 0.0, 0.40); 4];
        let (b, m) = (bat(0.0), modes());
        let idle_only = vec![m[1].clone()]; // no discharge: the grid supplies the load
        let p = optimize(&Problem {
            slots: &s,
            dt_h: 0.25,
            modes: &idle_only,
            battery: &b,
            grid: &GRID,
            soc0_wh: 5000.0,
            terminal_value_per_kwh: 0.0,
            soc_step_wh: 100.0,
        });
        // 1 kW for one hour in four quarters at 0.40/kWh.
        assert!((p.cost - 0.40).abs() < 1e-9, "{}", p.cost);
    }
}
