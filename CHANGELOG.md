# Changelog

What changed for users of the Home Assistant integration, the `vulpes-ems` wheel and the CLI. Newest first.

## Unreleased

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
