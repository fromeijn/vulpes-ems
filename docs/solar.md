# Solar forecasting

## Physics baseline

The baseline has no ML. Per plane (with microinverters, per panel):

1. **Sun position:** PSA algorithm (Blanco-Muriel et al., 2001).
2. **Plane-of-array irradiance** from forecast GHI/DNI/DHI:
   - beam: DNI × cos(angle of incidence) × ASHRAE incidence-angle modifier;
   - sky diffuse: Perez 1990 (all-sites composite coefficients);
   - ground reflection: albedo 0.2.
3. **Cell temperature:** Faiman model (u0 = 25, u1 = 6.84), from forecast air temperature and 10 m wind.
4. **Power:** PVWatts DC (γ = −0.4 %/°C), then inverter efficiency, then clipping at the inverter's AC limit.

The model runs on 15-minute weather. Open-Meteo radiation values are averages over the *preceding* 15 minutes, so they are shifted to the start of their interval. Hourly results are means of four sub-slots.

The building blocks are tested against pvlib reference values (`crates/vulpes-solar/tests/pvlib_reference.rs`):
- solar position: < 0.02° from NREL SPA;
- Perez, extraterrestrial irradiance, IAM and Faiman: exact to rounding.

## Calibration

`vulpes-cli calibrate-solar` fits azimuth, tilt and effective DC capacity per panel. It uses measured hourly power and the archived short-lead weather forecasts.

- **Hour selection:** only hours the forecast called mostly clear (GHI ≥ 65 % of a clear-sky estimate) with the sun above 10°.
- **Grid search:** azimuth every 5°, tilt every 5°, refined to 1°. For each geometry, capacity has a closed-form least-squares solution, because the model is linear in capacity below the inverter limit.
- **Trimmed refit:** the worst 20 % of hours are dropped and the fit is repeated. Those are mostly cloud timing errors in the weather input.
- **Group fits:** panels known to share a geometry share azimuth and tilt but keep their own capacity. A fixed azimuth offset supports tent ("east-west") pairs, for example 180° for exactly opposite panels. Constraining shared geometry stops partial shading from distorting individual fits.
- **Inverter limit:** each panel's AC limit is taken from its 99.5th-percentile hourly power. Panels that never reach their limit (for example north-east facing) inherit the highest limit seen.

## Validation

- **Split:** calibrate on even ISO weeks, evaluate on odd weeks, so evaluated days are never seen by the fit.
- **Honest weather:** evaluation uses *day-ahead* forecasts (Open-Meteo previous-runs API, issued about 24 h before each slot), never the short-lead archive the calibration uses.
- **Scores:** daily energy (MAE/RMSE/bias, compared against a benchmark series when available) and hourly power, with skill measured against persistence (same hour yesterday).
- **Ensembles:** averaging the forecasts driven by two weather models (KNMI Harmonie and ICON-D2) beat either model alone.

## Findings on shading maps (experimental, off by default)

`--shading` learns a beam-transmission factor per sun-position bin (10° azimuth × 5° elevation). In each bin it takes the median of hourly ratios, shrunk toward "unshaded".

On hourly data the maps did **not** improve held-out day-ahead accuracy, and they slightly increased negative bias. Most bins flagged as shaded sat at low sun elevations (10–15°). Real obstructions from low winter sun can't be told apart from weather-model artefacts there (hazy "clear" winter hours). In well-populated bins the maps mainly absorbed a general model bias, which does not transfer to day-ahead weather.

Planned revisit, once both of these are available:
- 15-minute actuals, since hourly means smear the sun over ~15° of azimuth;
- a separate learned bias layer, so shading no longer has to absorb bias.
