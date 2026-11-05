# Optimizer

## Modes as behaviours

The optimizer reasons about physical behaviours. The Home Assistant integration maps each user-defined mode to concrete actions.

| Behaviour | Effect (AC side, grid positive = import) |
|---|---|
| `grid_setpoint { w }` | Grid held at `w`; the battery takes `w + pv − load` (Victron ESS-style setpoint) |
| `self_consume` | Grid held at 0 |
| `no_discharge` | The battery may charge from PV surplus but never discharges |
| `idle` | Battery at 0 |
| `battery_power { w }` | Fixed battery power |

Any mode can add `pv_curtail`.

The distinction matters. With a grid setpoint, a heavy load *reduces* charging and can even discharge the battery, because grid power is held fixed. A battery-power model gets this wrong.

## Slot physics

`vulpes_opt::step` applies one mode for one slot, in this order:
1. power limits;
2. state-of-charge limits, with charge and discharge efficiencies;
3. the grid export limit, where excess PV is curtailed.

The optimizer and the replay simulator use this same function.

## Dynamic programming

The optimizer does backward induction over slot × SOC grid × mode, interpolating the value function linearly between SOC levels. The cost of each slot is:

`import kWh × import price − export kWh × export price + wear × |battery kWh|`

**End of horizon:** no end-SOC target is imposed. Energy left in the battery is credited at a terminal value per kWh: the median import price over the horizon × discharge efficiency.

**Reserve:** an optional minimum SOC per slot is enforced with a large penalty.

## Replay

`vulpes-cli replay` steps through history hour by hour. At each hour, every strategy decides with only the information available then:
- **PV:** day-ahead forecast;
- **load:** profile forecast from data up to that day;
- **prices:** published prices where they would have been out, the profile forecast otherwise.

The decision is then executed against actual load and PV, and settled at actual prices.

Strategies compared:
- self-consumption only;
- user-configured threshold rules;
- the optimizer;
- a perfect-foresight upper bound (the same optimizer fed actual future values).

`--export a,b` replays a scenario where export is paid `a × wholesale + b`, for example after net metering ends.

**Known limitation:** under the Dutch net-metering scheme ("saldering"), export only earns the import price up to the annual import volume. The replay doesn't model that cap yet, so net-metering results that export more than they import are optimistic.
