# Optimizer

## Modes as behaviours

The optimizer reasons about physical behaviours. The Home Assistant integration offers a fixed set of modes and translates each enabled one into a behaviour:

| Integration mode | Behaviour |
|---|---|
| Charge from grid | `grid_setpoint { +charge power }` or `battery_power { +charge power }` |
| Hold | `no_discharge` |
| Self-consume | `self_consume` |
| Idle | `idle` |
| Discharge to grid | `grid_setpoint { −discharge power }` or `battery_power { −discharge power }` |

"How power is set" chooses between grid setpoint and battery power. If solar can be switched off, each mode also gets a variant with `pv_curtail`. If export can be blocked (zero export), Hold, Self-consume and Idle also get a variant with `no_export`.

| Behaviour | Effect (AC side, grid positive = import) |
|---|---|
| `grid_setpoint { w }` | Grid held at `w`; the battery takes `w + pv − load` (Victron ESS-style setpoint) |
| `self_consume` | Grid held at 0 |
| `no_discharge` | The battery may charge from PV surplus but never discharges |
| `idle` | Battery at 0 |
| `battery_power { w }` | Fixed battery power |

Any mode can add `pv_curtail` (no PV at all) or `no_export` (export limit 0: PV still covers the load and charges the battery, and only the surplus beyond that is curtailed). Zero export only pays when exporting costs money: when the export price is negative.

The distinction matters. With a grid setpoint, a heavy load *reduces* charging and can even discharge the battery, because grid power is held fixed. A battery-power model gets this wrong.

## Slot physics

`vulpes_opt::step` applies one mode for one slot, in this order:
1. power limits;
2. state-of-charge limits, with charge and discharge efficiencies;
3. the grid import limit: the battery charges less. A load that exceeds the limit on its own is left as it is;
4. the grid export limit (0 for a `no_export` mode): excess PV is curtailed first, and if that is not enough the battery discharges less.

The optimizer and the replay simulator use this same function.

## Dynamic programming

The optimizer does backward induction over slot × SOC grid × mode, interpolating the value function linearly between SOC levels. The cost of each slot is:

`import kWh × import price − export kWh × export price + wear × |battery kWh|`

**Ties:** when modes score the same in a slot, the plan takes the least active one: a mode without a power setpoint before charge or discharge, and a plain mode before its `pv_curtail` or `no_export` variant. A full battery, for example, gives "charge" and "hold" the same outcome, and the plan says "hold".

**Charge can mean discharge:** with a grid setpoint, "charge" in a slot with a forecast load above the setpoint is a partial discharge. The optimizer uses it that way when it pays.

**Horizon:** 84 hours. The length was chosen with the replay's horizon comparison (see [Replay](#replay)), which gives each lead time the forecast that was available then. When the weather forecast stops earlier, the plan ends where the weather ends, with a warning: slots without a solar forecast are not planned. The weather must reach three quarters of the first day (or of a shorter horizon), with at most a quarter of the covered slots missing; otherwise planning fails.

**End of horizon:** no end-SOC target is imposed. Energy left in the battery is credited at a terminal value per kWh: the median import price over the horizon × discharge efficiency.

**Reserve:** an optional minimum SOC per slot is enforced with a large penalty.

## Replay

`vulpes-cli replay` steps through history hour by hour. At each hour, every strategy decides with only the information available then:
- **PV:** for each hour ahead, the freshest forecast that had been issued by then. Up to a day ahead that is the mean of the two short-range weather models; further ahead it is the longer-range model given with `--tail-model`, because the short-range models have no archived runs that far back. Live, the short-range models still cover part of the second day, so the replay is slightly pessimistic there;
- **load:** profile forecast from data up to that day;
- **prices:** published prices where they would have been out, the profile forecast otherwise.

The decision is then executed against actual load and PV, and settled at actual prices.

Strategies compared:
- self-consumption only;
- user-configured threshold rules;
- the optimizer;
- the optimizer with one forecast replaced by the actual values: load, PV, or the prices that were not yet published;
- a perfect-foresight upper bound (the same optimizer fed actual future values).

The three single-forecast variants show what a perfect forecast of that input would save, which is the most any improvement to that forecast can bring. Their savings interact, so they don't add up to the perfect-foresight gap. When actual values are missing somewhere in the horizon, these variants decide on the forecasts for that hour.

Below the table, the replay scales each saving to a year and gives a 90 % interval. Only local days of which every hour was replayed count for this; days with skipped hours are left out and their number is printed. The interval comes from resampling whole weeks of daily savings, because consecutive days are correlated. An interval that includes zero, or that overlaps another forecast's interval, means the history is too short to tell.

`--compare-horizons 48,60` replays the optimizer again with other planning horizons and prints what each saves per year against `--horizon-h`, with the same kind of interval. A horizon of more than 24 hours needs the tail model's forecasts in the store: `backfill-weather --lead-days <N> --models <tail model>` for each day of the horizon from the second. Where a forecast is missing for some hours, those hours are planned on a shorter horizon; the replay prints how many, because a comparison of horizons is only meaningful when that number is small.

Export is paid `a × wholesale + b`, from the site file. `--export a,b` replays another scenario, for example after net metering ends. Net metering itself is an export tariff equal to the import tariff.

**Known limitation:** under the Dutch net-metering scheme ("saldering"), export only earns the import price up to the annual import volume. The replay doesn't model that cap yet, so net-metering results that export more than they import are optimistic.
