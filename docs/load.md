# Load forecasting

## Target

House load excludes the battery:

`load = grid_import − grid_export + pv − battery_net_in`

The terms are energy per step. With hourly data, mean power in W equals energy in Wh.

`vulpes-cli build-load` builds this series:
- Grid series are given in priority order, for example a smart-meter reading first and the supplier's data second. That lets a history switch sources seamlessly.
- When the battery energy counters are missing, a battery power series fills in, with a configurable sign.
- Small negative values from sensor timing jitter are clipped to 0.

The Home Assistant integration builds the same series from hourly statistics. Two kinds of hours are left out, because a single one would distort the profile's mean for weeks:
- an hour where an energy counter went down (a meter reset or a corrected statistic);
- an hour with a load above what the grid connection, the battery and the solar panels can deliver together. The plan then carries a warning.

## Baseline: similar-day profile

Households follow the local clock and calendar, so everything works in local time, including DST.

**Day types** come from the local date: workday, Saturday, and Sunday. Public holidays count as Sunday. Dutch holidays are built in; Easter-based holidays use the computus.

**The forecast for a local hour** is a recency-weighted average of the same local hour on recent days of the same type:
- look back 56 days;
- weights halve every 21 days;
- fall back to all day types when fewer than 2 similar days exist;
- optionally blend in the same window last year.

The default is the weighted **mean**. A weighted median has a lower hourly MAE, but it ignores consumption spikes and so systematically under-forecasts energy. That matters for battery planning.

## Temperature-anomaly correction

A profile already reflects the recent temperature regime. It cannot anticipate a change, such as a heatwave starting tomorrow.

The correction regresses the profile's past errors on how the target hour's cooling and heating demand differs from the reference days the profile used:

- cooling degree-hours: `max(0, T − 18 °C)`
- heating degree-hours: `max(0, 12 °C − T)`

The fit is a non-negative ridge least squares. It is refitted daily on past days only, and the target day uses the day-ahead temperature forecast.

No appliance metering is needed. Temperature-driven loads such as air conditioners are captured through their effect on total load.

## Evaluation

`vulpes-cli eval-load` issues each forecast at 12:00 UTC on the day before. Only data available at that moment is used, including the data used to fit the correction.

Baselines:
- the last known day at the same hour (roughly what "naive" forecasters do day-ahead);
- the same hour last week.

On a test household, the profile with temperature correction reduced daily energy error by about 18 % and hourly RMSE by about 26 %, compared with the naive day-ahead method.

## Planned

- **AC heating submodel:** heating source chosen on temperature and the electricity/gas price ratio. This waits for enough heating-season data.
- **ML residual layer:** parked. In the oracle replay (see [optimizer.md](optimizer.md#replay)) on a test household, a perfect load forecast saved only about a tenth of what the optimizer already saves, so a residual model can gain little.
- **15-minute resolution:** from recorded live data.
