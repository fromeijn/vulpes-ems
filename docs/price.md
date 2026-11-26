# Price forecasting

Prices are handled in two layers:
1. a **wholesale** day-ahead forecast, which is market-wide;
2. a **consumer tariff** on top, which is supplier- and country-specific.

Published day-ahead prices always take precedence. Forecasts only fill the hours that aren't published yet: the next day before the auction results, and further ahead. With the 84-hour horizon, that is up to three and a half days of each plan.

## Published prices

In Home Assistant the published prices come from the user's price sensor, as all-in prices. Where the sensor has no price for a slot (it is unavailable, or its source has a gap), the published wholesale price from EnergyZero fills in, through the tariff (`a · wholesale + b`, export `export a · wholesale + export b`). Such a slot counts as known, like a sensor price; the plan's `inputs.published_price_slots` counts them, and the health check `prices` is degraded meanwhile. Only with EnergyZero as the wholesale source.

The **billing interval** setting says what one price covers in the contract:
- **per 15 minutes** (the default, as the day-ahead market since October 2025): the sensor's prices are used as they come, and the fallback fetches EnergyZero's quarter-hour prices (`public.api.energyzero.nl/public/v1/prices`, one request for today and tomorrow), with the hourly prices for what they don't cover;
- **per hour**: the sensor's prices shorter than an hour are averaged per hour, as hourly contracts bill the mean of the four quarter-hour prices, and the fallback uses the hourly wholesale prices Vulpes already keeps.

The **Current import price** and **Current export price** sensors show the known price of the current slot, from either source, so they can serve as the price entity in Home Assistant's Energy dashboard. They are unknown while the price for now is only forecast.

Tomorrow's prices are fetched from 13:00 Amsterdam time every 30 minutes until they are there, otherwise every 6 hours. Prices not published yet are forecast per hour in both cases.

## Tariff fit

`vulpes-cli fit-tariff` fits `all_in = a · wholesale + b` per calendar year. Energy taxes usually change on 1 January.

The fit uses **total cost per hour**:

`cost = a · (E · wholesale) + b · E`

Each hour is weighted by its energy E. Fitting per-kWh prices instead is dominated by low-consumption hours, where suppliers' rounded hourly costs blow up the per-kWh error. `--min-energy` can additionally drop such hours.

How to read the coefficients:
- `a` is usually the VAT factor (for example 1.21);
- `b` is (energy tax + supplier markup) × VAT.

Fitting import and export compensation separately shows whether export is paid at the import price (net metering) or not.

In Home Assistant, the health check `tariff` compares the Tariff settings with the tariff the price sensor itself follows (see [monitoring.md](monitoring.md#health)).

Export has its own `a` and `b`. Under net metering they equal the import values. Without it, export is typically paid at the wholesale price without VAT (`a` close to 1), minus any feed-in fee per kWh (a negative `b`). The planner gets the wholesale price of a published slot back from its import price. A fixed import price (`a` = 0) doesn't allow that, so it only goes with a fixed export price (export `a` = 0, the price in `b`).

## Wholesale forecast

The engine and `vulpes-cli eval-price` use the same code (`vulpes_price::wholesale`).

**Profile.** The same local hour on recent days of the same type (workday, Saturday, Sunday): 28-day look-back, 7-day half-life. Every future day of a type gets the same curve.

**Wind/solar correction.** A ridge regression explains the profile's error per hour by two features:
- how the hour's wind-power index differs from the same weighted mean over the profile's reference days;
- the same for solar.

Windy or sunny hours after calm, dull reference days are cheaper than the profile says, and the other way round. Details:
- The rows are recency-weighted (30-day half-life), λ = 10⁻³.
- The regression needs 14 days of rows.
- It trains on day-ahead residuals: for each past day, the profile of the days before it, against that day's prices.
- Live, it trains on every complete day of the wholesale history that has the profile's full look-back behind it.

**Anchor to the latest known prices.** The profile is a mean over four weeks, so its level lags when the market has moved in the last days. Without a correction, that shows as a step where the published prices end, and the planner would charge or discharge around a step that isn't there. The forecast's error in the last known hour says how large the step is:
- the forecast of each later hour gets that error added, fading with a time constant of 12 hours (`ANCHOR_HOURS`, `ANCHOR_FADE_H`);
- the first forecast hour so continues at the level of the last published one, and a day later 13 % of the offset is left;
- it applies to the profile and to the corrected forecast alike, and has nothing to train;
- when the wholesale history ends before the published prices do (a failed fetch), the fade counts from the history's end, so an old error has no effect.

The plan's `price_forecast.anchor_offset` is what the anchor adds to the first forecast hour, before the tariff.

**Drivers.** Open-Meteo forecasts at public reference points, one model (`ecmwf_ifs025`) for training and forecasting:

| Driver | Reference points | Transform |
|---|---|---|
| Wind | North Sea off IJmuiden (52.6 N, 3.9 E) and Schleswig-Holstein (54.3 N, 9.5 E) | 100 m wind through a normalised turbine power curve (cubic from 3 to 12 m/s), mean of the points |
| Solar | Central NL (52.1 N, 5.2 E) and central Germany (51.0 N, 10.5 E) | GHI in kW/m², mean of the points |

- The training history uses the day-ahead forecasts of the previous-runs API (`_previous_day1`), never the archive of actual weather, so training sees forecasts as good as the live ones.
- The day being forecast uses the latest forecast.
- An hour without a driver value gets the profile.
- ENTSO-E generation forecasts would be better drivers for the next day, but they don't reach the second to fourth day.

**Self-check.** Each planning run scores both methods on the last 14 days, each day forecast with a model fitted on the days before it. The correction is used only when its mean absolute error was lower. The plan's `price_forecast` says which method was used, with both errors.

**In Home Assistant**, only with EnergyZero as the wholesale source, because the reference points are for the Dutch market:
- the wholesale history (90 days) every 6 hours, only from the last known day on, and every 30 minutes from 13:00 Amsterdam time until tomorrow's prices are in;
- the reference-point history once a day, only from the last known day on;
- the reference-point forecast every 6 hours.

Both histories are kept in the history file (`price.wholesale.nl`, `price.driver.wind`, `price.driver.solar`), so after a restart only the missing days are fetched; a new install fetches the full 90 days once. All four reference points come in one Open-Meteo request per range.

A failed fetch keeps the cached data. The health check `price_drivers` reports a forecast older than 24 hours.

### Evaluation

`vulpes-cli eval-price` issues forecasts at 10:00 UTC, `--lead-days` days before the day (default 1), before that day's auction. Only prices up to the end of the issue day are known at that point.

| Method | How it works |
|---|---|
| Copy last known day | What simple setups do while tomorrow's prices are unknown. |
| Profile | As above. |
| Profile + wind/solar regression | As above, with the drivers as forecast `--lead-days` ahead for the day being forecast. |
| Profile + anchor, profile + wind/solar regression + anchor | The two above with the anchor on their error in the last known hour, the last hour of the issue day. |

`--weeks even` or `--weeks odd` scores only the days in those ISO weeks, to choose a setting on one half and check it on the other. The regression still learns from every past day.

**Scoring:** besides MAE and RMSE, it reports what matters for storage:
- the within-day rank correlation;
- how many of the day's actual cheapest and priciest 4 hours the forecast also picks.

**Results** over about a year of NL prices (late 2025 to late 2026), ECMWF drivers:

| Lead | Regression MAE vs profile | Rank correlation, profile → regression |
|---|---|---|
| 1 day | −28 % | 0.87 → 0.89 |
| 2 days | −26 % | 0.87 → 0.89 |
| 3 days | −23 % | 0.87 → 0.87 |
| 4 days | −20 % | 0.87 → 0.86 |

- For picking the very cheapest 4 hours, the plain profile scores slightly better, by up to 0.2 of an hour per day (at 4 days ahead).
- ICON-D2 drivers give similar results a day ahead, but ICON-D2 has no archive beyond one day ahead.
- The anchor lowers the day-ahead MAE by a further 9 % with the regression and by 14 % for the profile alone, with the same rank correlation and the same hits on the cheapest and priciest hours. Two days ahead it gains 1 %, beyond that nothing. Its two settings were chosen on the even ISO weeks; the odd weeks gave the same gain.

**What it is worth.** The replay ([optimizer.md](optimizer.md#replay)) runs the optimizer with each price forecast and settles at actual prices. On a test household with export at the wholesale price, the 84-hour plan with the correction cost about the same as with the profile alone, within the replay's interval. A perfect forecast of the unpublished prices also saved little there. The anchor made no difference in cost there either. The battery cycles mostly within the day, which the published prices already show. The correction matters more where the battery shifts energy across days: when export earns little, or with a large battery and a small daily load.
