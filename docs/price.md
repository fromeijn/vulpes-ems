# Price forecasting

Prices are handled in two layers:
1. a **wholesale** day-ahead forecast, which is market-wide;
2. a **consumer tariff** on top, which is supplier- and country-specific.

Published day-ahead prices always take precedence. Forecasts only fill the hours that aren't published yet: the next day before the auction results, and further ahead.

## Tariff fit

`vulpes-cli fit-tariff` fits `all_in = a · wholesale + b` per calendar year. Energy taxes usually change on 1 January.

The fit uses **total cost per hour**:

`cost = a · (E · wholesale) + b · E`

Each hour is weighted by its energy E. Fitting per-kWh prices instead is dominated by low-consumption hours, where suppliers' rounded hourly costs blow up the per-kWh error. `--min-energy` can additionally drop such hours.

How to read the coefficients:
- `a` is usually the VAT factor (for example 1.21);
- `b` is (energy tax + supplier markup) × VAT.

Fitting import and export compensation separately shows whether export is paid at the import price (net metering) or not.

Export has its own `a` and `b`. Under net metering they equal the import values. Without it, export is typically paid at the wholesale price without VAT (`a` close to 1), minus any feed-in fee per kWh (a negative `b`). The planner gets the wholesale price of a published slot back from its import price. A fixed import price (`a` = 0) doesn't allow that, so it only goes with a fixed export price (export `a` = 0, the price in `b`).

## Wholesale forecast

`vulpes-cli eval-price` issues forecasts at 10:00 UTC on the day before, before the auction. Only prices up to the end of the issue day are known at that point.

| Method | How it works |
|---|---|
| Copy previous day | What simple setups do while tomorrow's prices are unknown. |
| Profile | Same local hour on recent days: 28-day look-back, 7-day half-life. |
| Profile + wind/solar regression | Weighted ridge regression of past profile errors on how tomorrow's wind and solar differ from the profile's reference days. Recency-weighted with a 30-day half-life and refitted daily on past data only. |

**Interim drivers.** Until ENTSO-E fundamentals (generation and load forecasts per bidding zone) are wired in, the drivers are day-ahead Open-Meteo forecasts at public reference points:

| Driver | Reference points | Transform |
|---|---|---|
| Wind | North Sea off IJmuiden (52.6 N, 3.9 E) and Schleswig-Holstein (54.3 N, 9.5 E) | 100 m wind through a normalised turbine power curve (cubic from 3 to 12 m/s) |
| Solar | Central NL (52.1 N, 5.2 E) and central Germany (51.0 N, 10.5 E) | GHI |

**Scoring:** besides MAE and RMSE, it reports what matters for storage:
- the within-day rank correlation;
- how many of the day's actual cheapest and priciest 4 hours the forecast also picks.

**Results** over about a year of NL prices, compared with copying the previous day:
- the regression reduced MAE by about 26 % and RMSE by about 27 %;
- the within-day rank correlation went from about 0.79 to 0.89.

For picking the very cheapest hours, the plain profile scored slightly better. The oracle replay (see [optimizer.md](optimizer.md#replay)) shows what a perfect forecast of the not yet published prices would save. As long as that is small, the regression stays out of the engine and the plain profile is used.
