# Changelog

What changed for users of the Home Assistant integration, the `vulpes-ems` wheel and the CLI. Newest first.

## Unreleased

### Added

- **CLI:** `replay` also runs the optimizer with the actual load, PV or prices in place of the forecast, and prints what a perfect forecast of each would save per year, with a 90 % interval. The table has a new "vs optim" column.
- **CLI:** `replay --compare-horizons 48,60` replays the optimizer with other planning horizons and prints what each saves per year against `--horizon-h`.

### Changed

- **The net metering switch is gone.** Export is always paid as `export a × wholesale + export b`. Setups that had the switch on now have Export a/b equal to Import a/b, so their plans are unchanged. When net metering ends for you (in the Netherlands on 1 January 2027), set Export a/b in the options under Tariff.
- **New setups default to Export a = 1.0** (was 1.21): without net metering, feed-in is usually paid at the market price without VAT. Existing setups keep their values.
- The Tariff step explains Export a and Export b.
- **Engine request:** `export` no longer has `net_metering`. The integration and the wheel must be updated together.
- **CLI:** a replay site file with `net_metering` under `export` is rejected. For net metering, set `export.a`/`export.b` to the `tariff` values.
- **CLI:** `replay` now gives each hour the PV forecast that had been issued by then. Hours more than a day ahead use the model from the new `--tail-model` flag (default `ecmwf_ifs025`) instead of the day-ahead forecast, which was not available yet. Before replaying, run `backfill-weather --lead-days 2 --models ecmwf_ifs025` (and `--lead-days 3`, `4` for horizons beyond 48 and 72 hours). Results differ slightly from earlier versions.

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
