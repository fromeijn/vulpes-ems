# Changelog

What changed for users of the Home Assistant integration, the `vulpes-ems` wheel and the CLI. Newest first.

## Unreleased

### Added

- **Action `vulpes_ems.update_plan`:** plans at once, without waiting for the next quarter hour. It reads the prices and the state of charge as they are now and updates the plan and the sensors; while Active is on, it also runs the control script with the new plan. Use it in an automation to plan as soon as tomorrow's prices arrive (example in `docs/control.md`). The first call plans at once; calls in the 30 seconds after it are combined into one more run. Nothing to do if you don't need it; the integration and the wheel must be updated together.

### Fixed

- **Home Assistant no longer waits for Vulpes at startup.** Until now, setup waited for the first plan, and with it for 90 days of prices and of weather at the price reference points. On a network with broken IPv6 every request for that weather hung for about 40 seconds before it fell back to IPv4, which held up the start of Home Assistant, and your own automations, by minutes, and delayed the control script by about 3 minutes once a day. Now setup finishes at once, and the first plan follows in the background as soon as Home Assistant has started, when the SOC and price sensors of other integrations have their states. Connections give up on an address that doesn't answer after a few seconds and try the next, and every request is limited to 20 seconds (was 60). Nothing to do. If your network hands out IPv6 addresses but IPv6 doesn't work, fixing it, or turning IPv6 off in Home Assistant (Settings → System → Network), also helps other integrations.
- **Entities while the first plan runs, or when it fails:** the entities and the Status sensor now exist straight after a start. The plan sensors are unavailable until the first plan, and Status stays `ok` with the plan check "waiting for the first plan" (a problem after 30 minutes without one). When the first plan failed at startup, there used to be no entities at all. A first plan that fails is tried again at the next quarter hour, or when you call `vulpes_ems.update_plan`.
- **History file:** a quarter hour keeps its first state of charge reading. When a quarter hour was recorded a second time (after a settings change or when Active was turned on), the later reading replaced it. In the first quarter of an hour that shifted the reading the "SOC tracking error" sensor and the savings figures use.

### Changed

- **Active after a restart:** a restored Active switch waits for the first plan and applies it when it is ready, usually within seconds of Home Assistant having started, instead of being applied during setup. No safe command is sent before it, unless that plan fails. Meanwhile the Battery control sensor shows the new state `waiting` ("Waiting for the first plan"); automations that compare its state to the four existing ones may need it.
- **Price history in the history file:** the wholesale prices and the wind and solar at the price reference points are now kept in the history file (new series `price.driver.wind` and `price.driver.solar`). After a restart Vulpes only fetches the days after them. The first start after the update still fetches all 90 days once. The weather at the four reference points now takes one request per range to Open-Meteo instead of four.
- **Building from source needs Rust 1.89** (was 1.85), for the CLI and for a wheel you build yourself. The dependencies are updated, among them PyO3 0.30, rusqlite 0.43 and quick-xml 0.42. Nothing to do when you use the published wheels.

### Removed

- **Plan cost sensor:** the sensor is gone, and its entity is removed when the integration is updated. It showed the expected cost over the next 84 hours, a figure that could not be compared with a bill or a day and that shifted every time the plan moved on. The Saved last 7 days and Total saved sensors show what the plan is worth. Change dashboards and automations that use the Plan cost sensor. The figure is still in the engine response (`summary.cost`) and in the diagnostics.

## 0.6.0 - 2026-11-17

Prices that are not published yet now follow the wind and solar forecast instead of repeating the recent average, with EnergyZero as the wholesale source. The integration and the `vulpes-ems` wheel must be updated together, as always; the first refresh after the update fetches 90 days of wholesale prices and of weather at four public reference points.

### Changed

- **Prices that are not published yet now follow the wind and solar forecast.** Until now, each future day got the same price curve: the average of recent days. Now Vulpes also looks at the weather forecast at four public reference points: wind over the North Sea and northern Germany, and sun over the Netherlands and central Germany. A windy or sunny day is forecast cheaper than a calm, dull one. This matters most for the second to fourth day of the 84-hour plan. Vulpes checks the correction every planning run against the last 14 days, and uses the plain average when the correction did worse. Only with EnergyZero as the wholesale price source. Nothing to do; the integration and the wheel must be updated together.
- **Wholesale price history:** Vulpes now keeps 90 days instead of 35. It fetches all of them once after Home Assistant starts; the refresh every 6 hours then only fetches the newest days, one request to EnergyZero instead of two.

### Added

- **Health:** a new check, `price_drivers`, on the Status sensor: degraded when the wind and solar forecast has not been fetched for 24 hours (the cached forecast is used as far as it reaches; hours beyond it fall back to the plain average). The weather at the reference points is fetched from Open-Meteo: the history once a day and the forecast every 6 hours.
- **Diagnostics:** the plan's `price_forecast` (method used and the recent check).
- **Engine request:** new optional `price_drivers` (`past` and `ahead` rows of `[hour, wind index, solar index]`); `vulpes_ems.price_drivers` fetches them. **Engine response:** new `price_forecast` with the method used (`published`, `profile`, `regression` or `mean_known`), the number of forecast slots and the recent check.
- **CLI:** `eval-price --lead-days N` scores forecasts issued N days ahead. Use day-ahead sources (`open-meteo-day1:…`); the matching `open-meteo-day<N>:…` forecasts must be in the store (`backfill-weather --lead-days N`). The first row is now called "copy last known day".
- **CLI:** `replay --price-wind … --price-solar …` also replays the optimizer with the wind/solar price correction, and prints what it saves per year against the plain average. It needs the reference points' weather for each lead day of the horizon.

## 0.5.0 - 2026-11-15

The integration now judges itself: a Status sensor, repair issues and performance sensors, from a history file Vulpes keeps (see `docs/monitoring.md`). The performance figures appear once three complete days are recorded. All of the integration's logic moved into the Rust core; the integration and the `vulpes-ems` wheel must be updated together, as always. CLI stores from earlier versions keep working (a new table is added the first time a newer CLI opens them, and older CLIs ignore it).

### Added

- **History file:** Vulpes keeps its own history in `<config>/vulpes_ems/<entry id>/vulpes.db`: the forecasts as they were made (PV, load and price, up to 48 hours ahead), the planned mode and prices of each quarter hour, whether control followed the plan, the state of charge, and hourly energy, load and wholesale prices. Forecasts are kept 35 days, the rest 400 days. It is the same format as the CLI's store, so a copy works with `vulpes-cli eval-*` and `replay`. It is removed with the integration. The coming performance sensors are computed from it.
- **Status sensor:** "Status" is `ok`, `degraded` or `problem`, from checks on the plan (failing or old), the battery SOC sensor (unavailable, or silent while the plan moves the battery), the price sensor (no price for now, no prices for tomorrow after 15:00), the energy statistics (late or stopped), wholesale prices, what the engine had to guess, the control script, the history file, and the review (forecasts worse than "same hour yesterday", the battery not following the plan's physics). The attribute `checks` lists each one with the reason; `problems` lists the ones that aren't ok. It stays available when planning fails. Two diagnostic timestamps, "Last plan" and "Energy data up to", show how fresh the inputs are. Health only reports: it never changes what control does.
- **Repairs:** Vulpes raises a repair issue when the battery SOC has been unavailable for an hour, there has been no plan for an hour, the energy statistics have had no new data for a day, or the control script failed three times in a row while Active is on. The issue closes by itself once the cause is gone.
- **Performance sensors**, computed from the history file whenever new hourly energy statistics arrive (about once an hour). Each needs three complete days of history first. The three savings sensors also need the "Energy into the battery" and "Energy out of the battery" statistics in the settings: without them the Status sensor's `coverage` check is degraded and says so.
  - "Saved last 7 days": what electricity cost you against plain self-consumption (the battery only covering the house, simulated on your measured load and solar), corrected for the energy left in the battery. It counts whoever controlled the battery; attribute `active_share_pct` says how much of that time Vulpes did.
  - "Plan would save last 7 days": the same for Vulpes's plan replayed on your measured load and solar, also in shadow mode. It shows what switching on Active is worth; once Active, the gap to "Saved" shows control or model losses.
  - "Total saved": the running total of the first figure (state class `total`, so long-term statistics keep it). It is unknown until the first complete day is counted.
  - The savings sensors are in Home Assistant's currency, the one the tariffs are entered in. A quarter hour without a plan (a failed planning run, a restart) takes the price an earlier plan had for that hour, so one gap doesn't lose the day.
  - Diagnostic: "Solar forecast error" and "Load forecast error" (day-ahead error as % of the mean, with bias, the latest forecast and a "same hour yesterday" baseline in the attributes), and, with a control script, "Control followed the plan" and "SOC tracking error" (how far the battery's state of charge moved from what the plan's physics expected).
- **Diagnostics download** (device page → Download diagnostics): the settings with entity ids and names redacted, the plan's inputs and warnings, control and calibration state, and the last 35 days of the history file. That includes your meter data; share it only with people you trust.
- **Solar forecast per day:** four new sensors, "PV forecast today" (the whole day, including the hours that have passed), "PV forecast rest of today", "PV forecast tomorrow" and "PV forecast day after tomorrow", in kWh. A sensor is unknown when the weather forecast doesn't cover that day's daylight. The engine response has them as `summary.pv_kwh_today`, `pv_kwh_today_remaining`, `pv_kwh_tomorrow` and `pv_kwh_day_after`.
- **Solar forecast in the Energy dashboard:** Vulpes can now be the solar forecast of Home Assistant's Energy dashboard. Choose "Vulpes EMS" as the forecast of your solar production under Settings → Dashboards → Energy → Solar panels. It covers the whole of today and the days the plan reaches, per hour, as what the panels could produce before curtailment. The hours of today that have passed follow the latest weather, so they are not the forecast as it was made; past days have no forecast line. The engine response has it as `pv_hourly`, a list of `[hour_start, Wh]`.

### Changed

- **Agreement with your mode entity survives restarts:** "Agreement with current mode today" is counted from the history file, so a restart or settings change no longer resets it to empty for the rest of the day.
- **Hold or self-consume on a solar surplus is now a choice, not a coincidence.** When the forecast has more solar than load, both modes do the same, and the plan used to say hold. If the house then used more than forecast, hold imported that power, even at a high price with a full battery. The plan now takes the mode that is cheaper when the load forecast is off: self-consume when the stored energy is worth less than importing now, hold when it is worth more. You will see self-consume instead of hold in more daytime slots. Nothing to do; the integration and the wheel must be updated together.
- **Faster forecast lookups in the store:** finding the forecast that was known at a given time now reads the forecasts once instead of once per stored issue, with the same results. The hourly review of the history file takes about a fifth of the CPU time, and the CLI commands that read weather forecasts from a large store (load, price and solar fitting and evaluation) are faster. Nothing to do.
- **The 64-bit ARM wheels are tested before release.** The wheels for Raspberry Pi 4 and 5 and Home Assistant Green and Yellow (aarch64), and the glibc (manylinux) wheels, were built but never installed in CI; all four wheels now run the test suite on their own architecture. 32-bit ARM is not supported. `docs/optimizer.md` and `docs/solar.md` now give run times: a planning run stays under about a second on a Raspberry Pi 4-class board, and a solar calibration with string sensors takes one to two minutes there.

### Fixed

- **Plan cost currency:** the Plan cost sensor is now in Home Assistant's currency instead of always EUR. Nothing changes for users whose currency is EUR.
- **Removing or reloading the integration during a planning run** no longer lets that run carry on: it could send the plan to the control script after the safe mode was sent, recreate the history file of a removed entry, or leave a repair issue open until the next restart.
- **Price times without a time zone** in the price sensor's attributes are read in Home Assistant's time zone. Before, they were read in the system time zone of the machine or container, which can differ (Docker containers often run in UTC). Price sensors that include the offset, as most do, are not affected.
- **Agreement while Active:** with the Active switch on and a mode entity that the control script sets, every mode change counted as a disagreement, because the plan was compared with the mode of the previous quarter hour. The comparison now happens after the script has run, so "Current mode matches plan" and "Agreement with current mode today" show whether the script applied the plan.

## 0.4.0 - 2026-11-12

### Added

- **CLI:** `replay` also runs the optimizer with the actual load, PV or prices in place of the forecast, and prints what a perfect forecast of each would save per year, with a 90 % interval. The table has a new "vs optim" column.
- **CLI:** `replay --compare-horizons 48,60` replays the optimizer with other planning horizons and prints what each saves per year against `--horizon-h`.

### Changed

- **Vulpes now plans 84 hours ahead** (was 36). In a replay of a year of history with the forecasts that were available at the time, a longer horizon gave lower costs. The `plan` attribute of the Planned mode sensor now has 336 slots instead of 144, so a dashboard chart of the plan gets longer. The Plan cost sensor is the expected cost over those 84 hours, so its value is more than twice as high as before: adjust automations or thresholds that use it.
- **Solar forecast beyond two days:** slots that the two short-range weather models don't reach use ECMWF IFS from Open-Meteo, in the same request.
- **Weather that stops early:** when the weather forecast does not reach the whole horizon, for example because ECMWF is unavailable, the plan ends where the weather ends and the Planned mode sensor shows a warning. Planning fails only when the weather covers less than 18 hours.
- **Engine request:** new optional `weather_tail_model` (default `"ecmwf_ifs025"`, `null` for none), and `horizon_h` now defaults to 84. The integration and the wheel must be updated together.
- **CLI:** `replay --horizon-h` defaults to 84, like the integration.
- **The net metering switch is gone.** Export is always paid as `export a × wholesale + export b`. Setups that had the switch on now have Export a/b equal to Import a/b, so their plans are unchanged. When net metering ends for you (in the Netherlands on 1 January 2027), set Export a/b in the options under Tariff.
- **New setups default to Export a = 1.0** (was 1.21): without net metering, feed-in is usually paid at the market price without VAT. Existing setups keep their values.
- The Tariff step explains Export a and Export b. With Import a at 0 (a fixed price), it asks for Export a at 0 too, with the fixed export price in Export b.
- **Engine request:** `export` no longer has `net_metering`. The integration and the wheel must be updated together.
- **Engine request:** stricter checks. A request is now rejected when it has an unknown key at the top level or in `location`, `export`, `known_prices` or `weather`, a `horizon_h` above 240, a `tariff.a` of 0 with an `export.a` other than 0, negative grid limits, or two modes with the same name. The integration never sends these, except Import a = 0 with another Export a: set Export a to 0 in the options under Tariff. Check your own scripts that call `vulpes_ems.plan`.
- **Engine response:** the plan can have fewer slots than the horizon asks for, when the weather stops early (see above). Before, the slots without weather were planned without solar power, and the request failed when more than a quarter had none.
- **CLI:** a replay site file with `net_metering` under `export` is rejected. For net metering, set `export.a`/`export.b` to the `tariff` values.
- **CLI:** `replay` now gives each hour the PV forecast that had been issued by then. Hours more than a day ahead use the model from the new `--tail-model` flag (default `ecmwf_ifs025`) instead of the day-ahead forecast, which was not available yet. Before replaying, run `backfill-weather --lead-days 2 --models ecmwf_ifs025` (and `--lead-days 3`, `4` for horizons beyond 48 and 72 hours). Results differ slightly from earlier versions.
- **CLI:** `replay` scales its yearly figures from complete days only, and prints how many partial days it left out. It warns when hours were planned on a shorter horizon because a forecast was missing, and rejects a site file with invalid grid limits. `--compare-horizons` is faster: it replays only the optimizer.

### Fixed

- **Grid limits:** a plan no longer discharges the battery above the maximum grid export, or charges it above the maximum grid import. Before, only solar power was limited, so a plan could count on export or import that the connection does not allow. Plans change only for setups where a grid limit is lower than the battery power plus the load or solar power. Nothing to set.
- **Engine:** a request with a planning horizon shorter than one slot is rejected with an error instead of crashing the engine. Should the engine still crash, Python now gets a `ValueError` ("internal error: …") like for any other failure.

## 0.3.1 - 2026-11-10

### Fixed

- Load forecast: hours with an impossible load (meter glitches) are ignored, with a warning.
- Optimizer: when modes score the same, the plan takes the least active one.

## 0.3.0 - 2026-11-08

### Added

- Solar setup through a form: arrays on a shared inverter, and calibration from Home Assistant history.
- Active control through a user script, with a safe fallback command.
- Zero-export variants of Hold, Self-consume and Idle.
- EPEX Spot price lists as a price source.
- Dutch translation.

### Changed

- Setup flow: sources are validated against live states and statistics, the options are a menu, and inputs are checked.

### Fixed

- Calibration: clipping detection, and the status after a new proposal.
- Control: releases the inverter on unload, shows script errors, and tolerates a state of charge above 100 %.

## 0.2.0 - 2026-11-07

### Changed

- The integration has its own modes (charge, hold, self-consume, idle, discharge) instead of user-named ones. Existing setups are migrated.

### Fixed

- Setup forms on Home Assistant 2026.11.

## 0.1.0 - 2026-11-07

- First release: solar, load and price forecasts, the optimizer, history replay, and a shadow-mode Home Assistant integration.
