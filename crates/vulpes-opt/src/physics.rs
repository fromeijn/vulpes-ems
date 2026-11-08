//! What one mode does during one slot: battery power, grid power, new state
//! of charge. Used by both the optimizer and the replay simulator.

use vulpes_core::{BatteryParams, Behaviour, GridLimits, ModeDef};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotOutcome {
    /// AC battery power, positive = charging (W).
    pub battery_w: f64,
    /// Grid power, positive = import (W).
    pub grid_w: f64,
    /// PV power not produced because of curtailment or the export limit (W).
    pub curtailed_w: f64,
    pub soc_wh: f64,
}

/// Apply `mode` for `dt_h` hours, given mean load and PV power (W).
pub fn step(
    mode: &ModeDef,
    load_w: f64,
    pv_w: f64,
    soc_wh: f64,
    bat: &BatteryParams,
    grid: &GridLimits,
    dt_h: f64,
) -> SlotOutcome {
    let pv = if mode.pv_curtail { 0.0 } else { pv_w.max(0.0) };
    let mut curtailed = pv_w.max(0.0) - pv;
    let net = load_w - pv; // positive = deficit
    let wanted = match mode.behaviour {
        Behaviour::GridSetpoint { w } => w - net,
        Behaviour::SelfConsume => -net,
        Behaviour::NoDischarge => (-net).max(0.0),
        Behaviour::Idle => 0.0,
        Behaviour::BatteryPower { w } => w,
    };
    // Power limits, then energy limits from the state of charge.
    let (min_wh, max_wh) = (bat.min_soc * bat.capacity_wh, bat.max_soc * bat.capacity_wh);
    let mut b = wanted.clamp(-bat.max_discharge_w, bat.max_charge_w);
    if b > 0.0 {
        b = b.min((max_wh - soc_wh).max(0.0) / (bat.charge_eff * dt_h));
    } else {
        b = b.max(-(soc_wh - min_wh).max(0.0) * bat.discharge_eff / dt_h);
    }
    let mut g = net + b;
    // Export above the limit is curtailed (inverters throttle PV first).
    if g < -grid.max_export_w {
        let excess = -grid.max_export_w - g;
        let cut = excess.min(pv);
        curtailed += cut;
        g += cut;
    }
    let soc = if b > 0.0 {
        soc_wh + b * dt_h * bat.charge_eff
    } else {
        soc_wh + b * dt_h / bat.discharge_eff
    };
    SlotOutcome {
        battery_w: b,
        grid_w: g,
        curtailed_w: curtailed,
        soc_wh: soc.clamp(min_wh, max_wh),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bat() -> BatteryParams {
        BatteryParams {
            capacity_wh: 10_000.0,
            max_charge_w: 3000.0,
            max_discharge_w: 3000.0,
            charge_eff: 0.9,
            discharge_eff: 0.9,
            min_soc: 0.1,
            max_soc: 1.0,
            wear_cost_per_kwh: 0.0,
        }
    }
    const GRID: GridLimits = GridLimits {
        max_import_w: 17_000.0,
        max_export_w: 17_000.0,
    };
    fn mode(b: Behaviour) -> ModeDef {
        ModeDef {
            name: "m".into(),
            behaviour: b,
            pv_curtail: false,
        }
    }

    #[test]
    fn grid_setpoint_charging_shrinks_with_load() {
        let m = mode(Behaviour::GridSetpoint { w: 3000.0 });
        let light = step(&m, 500.0, 0.0, 5000.0, &bat(), &GRID, 1.0);
        assert_eq!((light.battery_w, light.grid_w), (2500.0, 3000.0));
        // A 4 kW load: grid stays at 3 kW, so the battery discharges 1 kW.
        let heavy = step(&m, 4000.0, 0.0, 5000.0, &bat(), &GRID, 1.0);
        assert_eq!((heavy.battery_w, heavy.grid_w), (-1000.0, 3000.0));
    }

    #[test]
    fn self_consume_and_no_discharge() {
        let sc = step(
            &mode(Behaviour::SelfConsume),
            800.0,
            200.0,
            5000.0,
            &bat(),
            &GRID,
            1.0,
        );
        assert_eq!((sc.battery_w, sc.grid_w), (-600.0, 0.0));
        let nd = step(
            &mode(Behaviour::NoDischarge),
            800.0,
            200.0,
            5000.0,
            &bat(),
            &GRID,
            1.0,
        );
        assert_eq!((nd.battery_w, nd.grid_w), (0.0, 600.0));
        let nd_sun = step(
            &mode(Behaviour::NoDischarge),
            300.0,
            1300.0,
            5000.0,
            &bat(),
            &GRID,
            1.0,
        );
        assert_eq!((nd_sun.battery_w, nd_sun.grid_w), (1000.0, 0.0));
    }

    #[test]
    fn energy_limits_and_efficiency() {
        // Only 900 Wh of headroom: at 90% charge efficiency the battery takes 1000 W for 1 h.
        let full = step(
            &mode(Behaviour::BatteryPower { w: 3000.0 }),
            0.0,
            0.0,
            9100.0,
            &bat(),
            &GRID,
            1.0,
        );
        assert!((full.battery_w - 1000.0).abs() < 1e-9 && (full.soc_wh - 10_000.0).abs() < 1e-9);
        // Discharging 900 W for 1 h at 90% efficiency drains 1000 Wh.
        let d = step(
            &mode(Behaviour::BatteryPower { w: -900.0 }),
            900.0,
            0.0,
            5000.0,
            &bat(),
            &GRID,
            1.0,
        );
        assert!((d.soc_wh - 4000.0).abs() < 1e-9 && d.grid_w.abs() < 1e-9);
        // Empty at min SOC: no discharge.
        let e = step(
            &mode(Behaviour::SelfConsume),
            500.0,
            0.0,
            1000.0,
            &bat(),
            &GRID,
            1.0,
        );
        assert_eq!((e.battery_w, e.grid_w), (0.0, 500.0));
    }

    #[test]
    fn export_limit_and_curtailment() {
        let lim = GridLimits {
            max_import_w: 17_000.0,
            max_export_w: 800.0,
        };
        let r = step(&mode(Behaviour::Idle), 200.0, 3000.0, 5000.0, &bat(), &lim, 1.0);
        assert_eq!((r.grid_w, r.curtailed_w), (-800.0, 2000.0));
        let c = ModeDef {
            pv_curtail: true,
            ..mode(Behaviour::Idle)
        };
        let r = step(&c, 200.0, 3000.0, 5000.0, &bat(), &GRID, 1.0);
        assert_eq!((r.grid_w, r.curtailed_w), (200.0, 3000.0));
    }

    #[test]
    fn idle_leaves_the_battery_alone() {
        let lim = GridLimits {
            max_import_w: 17_000.0,
            max_export_w: 17_000.0,
        };
        let o = step(&mode(Behaviour::Idle), 800.0, 300.0, 5000.0, &bat(), &lim, 1.0);
        assert_eq!(
            (o.battery_w, o.grid_w, o.soc_wh, o.curtailed_w),
            (0.0, 500.0, 5000.0, 0.0)
        );
    }

    #[test]
    fn fixed_battery_power_is_clamped_to_the_limits() {
        let b = bat();
        let lim = GridLimits {
            max_import_w: 17_000.0,
            max_export_w: 17_000.0,
        };
        let up = step(
            &mode(Behaviour::BatteryPower { w: 1e6 }),
            0.0,
            0.0,
            5000.0,
            &b,
            &lim,
            0.25,
        );
        assert_eq!(up.battery_w, b.max_charge_w);
        let down = step(
            &mode(Behaviour::BatteryPower { w: -1e6 }),
            0.0,
            0.0,
            5000.0,
            &b,
            &lim,
            0.25,
        );
        assert_eq!(down.battery_w, -b.max_discharge_w);
        // The grid supplies or takes the battery power on top of the house balance.
        assert_eq!(down.grid_w, -b.max_discharge_w);
    }

    #[test]
    fn soc_stays_within_bounds_for_every_mode() {
        let b = bat();
        let lim = GridLimits {
            max_import_w: 17_000.0,
            max_export_w: 17_000.0,
        };
        let (lo, hi) = (b.min_soc * b.capacity_wh, b.max_soc * b.capacity_wh);
        let behaviours = [
            Behaviour::GridSetpoint { w: 5000.0 },
            Behaviour::GridSetpoint { w: -5000.0 },
            Behaviour::SelfConsume,
            Behaviour::NoDischarge,
            Behaviour::Idle,
            Behaviour::BatteryPower { w: 3000.0 },
            Behaviour::BatteryPower { w: -3000.0 },
        ];
        for beh in behaviours {
            for soc in [lo, lo + 1.0, 0.5 * (lo + hi), hi - 1.0, hi] {
                for (load, pv) in [(0.0, 0.0), (2000.0, 0.0), (300.0, 4000.0)] {
                    let o = step(&mode(beh), load, pv, soc, &b, &lim, 1.0);
                    assert!((lo..=hi).contains(&o.soc_wh), "{beh:?} soc {soc}: {o:?}");
                    // Energy balance: grid = load − used PV + battery.
                    let used_pv = pv - o.curtailed_w;
                    assert!(
                        (o.grid_w - (load - used_pv + o.battery_w)).abs() < 1e-9,
                        "{beh:?}: {o:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn shorter_slots_move_less_energy() {
        let b = bat();
        let lim = GridLimits {
            max_import_w: 17_000.0,
            max_export_w: 17_000.0,
        };
        let m = mode(Behaviour::BatteryPower { w: 2000.0 });
        let hour = step(&m, 0.0, 0.0, 5000.0, &b, &lim, 1.0);
        let quarter = step(&m, 0.0, 0.0, 5000.0, &b, &lim, 0.25);
        assert!(((hour.soc_wh - 5000.0) - 4.0 * (quarter.soc_wh - 5000.0)).abs() < 1e-9);
    }
}
