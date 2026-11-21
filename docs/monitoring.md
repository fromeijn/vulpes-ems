# Monitoring: health, history and performance

Vulpes judges itself inside Home Assistant: a Status sensor says whether it is working, and a set of sensors says how well. Everything is computed in Rust (`crates/vulpes-ha/src/health.rs`, `crates/vulpes-engine/src/history.rs` and `review.rs`) from what Home Assistant shows and from Vulpes's own history file. None of it changes what control does.

## The history file

Each config entry has a history file at `<config>/vulpes_ems/<entry_id>/vulpes.db`. It is a `vulpes-core` store, the same format as the CLI's `local/<house>/vulpes.db`, with the same series keys. A copy can be opened with the CLI tools (`vulpes-cli --store <copy> stats`); `eval-*` and `replay` also need archived weather, which `backfill-weather` adds.

Every 15 minutes, after control has run, Vulpes records:

| What | Where | Kept |
|---|---|---|
| The first plan of each hour as forecasts by issue time: hourly means of PV, load and import price, and whether the price was published, for 1 to 48 hours ahead | `forecasts` (source `vulpes`) | 35 days |
| The quarter hour's planned mode and prices, the plan's terminal value, whether the Active switch was on and the script ran the plan, and the state of the user's mode entity as a Vulpes mode | `plan_slots` | 400 days |
| The battery's state of charge | `soc`, 15 minutes | 400 days |
| Hourly energy from and to the grid, from PV, into and out of the battery; the house load; PV power; wholesale prices | `grid.import`, `grid.export`, `pv.total`, `battery.charge`, `battery.discharge`, `load.house`, `pv.total.power`, `price.wholesale.nl` | 400 days |
| The wind and solar indices at the price reference points, as forecast a day ahead (see `docs/price.md`) | `price.driver.wind`, `price.driver.solar` | 400 days |

The wholesale prices and the reference-point indices also serve as the price cache: at startup Vulpes reads them back, so it only fetches the days after them. They are written in full, but only hours that are new or changed.

A quarter hour can be recorded more than once: after a settings change, when Active is turned on, or when the `vulpes_ems.update_plan` action is called. Its slot record then shows the last plan's mode and prices; its state of charge stays the first reading, the one closest to the start of the quarter hour.

The hourly series of the last 48 hours are rewritten on each tick, so late or corrected recorder statistics are picked up. After each write the write-ahead log is checkpointed, so a backup of the `.db` file alone is complete. A failing write is logged and shows on the Status sensor; planning and control carry on.

Agreement with the user's mode entity ("Agreement with current mode today") is counted from the slot records, so it survives restarts. A slot counts when it had a plan and the entity a known state; it agrees when the entity showed the planned Vulpes mode (the solar-off and zero-export flags aside).

## The review

After recording, Vulpes reviews the last 30 local days. Days follow the configured time zone, so DST days have 23 or 25 hours. The measurements are hourly on UTC hours, so in a time zone with a 30 or 45-minute offset a day starts on the whole hour before local midnight.

The review covers complete past days only, so its figures can change only when a new hour of energy statistics arrives, the local date changes, or the settings change. The result is stored in the `meta` table with what it was computed from, and later ticks reuse it until one of these changes. In practice the review runs about once an hour, at the first tick after the hour's statistics land.

### Savings

For every complete day (every hour has the measured energy, including the battery's charge and discharge, and the load, every slot a price, and the SOC is known at both midnights):

- **Actual cost:** measured import at the hour's mean import price, minus measured export at the hour's mean export price, plus battery wear on the measured charge and discharge. Prices are the ones the plan had for each slot. A slot without a plan (a failed tick, Home Assistant down) takes, in this order: the hour's price from an earlier plan's forecasts when it was already published then (the hourly mean, with the export price derived from it through the tariffs), the wholesale price through the tariff, or the mean of the hour's other recorded slots. Only an hour with none of these makes the day incomplete.
- **Self-consumption baseline:** the battery only covering the house, simulated on the measured load and PV in 15-minute steps with the same slot physics as the optimizer (`vulpes_opt::step`), from the SOC measured at midnight.
- **Plan replayed:** the same simulation with each slot's planned mode. A slot without a plan, or with a mode that is no longer enabled, uses self-consumption (counted in a warning).
- **Energy left in the battery** at the end of the day differs between these. It is valued at the terminal value of the day's last plan (the expected future price after discharge losses): `saved = baseline − actual − (baseline end SOC − measured end SOC) × value`, and the same for the plan.

All three costs settle with `vulpes_opt::slot_cost`, the formula the optimizer and the CLI replay use.

"Saved last 7 days" is the actual figure: it counts whoever controlled the battery, and its `active_share_pct` attribute says how much of the time Vulpes did. "Plan would save" is the replayed plan, available in shadow mode too: it estimates what handing over control is worth. Once Active, a gap between the two points at control or model losses.

**Totals:** each newly complete day is added once to the running totals (in the store's `meta` table), so pruning and restarts don't change them. A day still incomplete 48 hours after it ended is skipped for good, so one gap doesn't hold up the days after it. A lost history file starts the totals over, with a new "since" date.

### Forecast quality

Per window (yesterday, 7 and 30 days), for PV (daylight hours) and load:

- **Day-ahead:** the newest forecast issued at least a day ahead. This is what the plan's big decisions rest on.
- **Latest:** the newest forecast issued at least an hour ahead.
- **Persistence:** the measured value of the same hour the day before, the naive baseline.

Each reports MAE, RMSE and bias in W; the sensor state is the day-ahead MAE as a percentage of the mean measured power. `skill` is 1 − RMSE(day-ahead) / RMSE(persistence) on the hours both have: negative means the forecast did worse than repeating yesterday.

Import prices are scored only for hours whose price was not yet published a day ahead, against the measured price and the same hour the day before.

### Control

- **Control followed the plan:** the share of quarter hours with the Active switch on in which the script ran the planned command (not the safe fallback) without error.
- **SOC tracking error:** for each hour in which control followed the plan in all four quarter hours, the planned modes are replayed from the measured SOC on the measured load and PV, and the result is compared with the measured SOC an hour later (percentage points). A persistent error points at a script that doesn't apply the modes, or battery settings (capacity, efficiencies, limits) that don't match the battery.

Figures need three complete (or scored) days in a window, and SOC tracking twelve hours; until then they are unknown.

## Health

The Status sensor is the worst state of these checks (`not_enough_data` counts as ok):

| Check | Degraded | Problem |
|---|---|---|
| `plan` | the last refresh failed | no successful plan for 30 minutes |
| `soc` | not reported for an hour while the plan moves the battery 10 points or more in the coming hour | not a state of charge (unavailable, out of range) |
| `prices` | no prices for tomorrow after 15:00 local time | no price for now |
| `energy_data` | the newest complete hour of energy statistics ended more than 3 hours ago | more than 24 hours ago |
| `wholesale` (EnergyZero only) | not fetched for 24 hours | – |
| `price_drivers` (EnergyZero only) | the wind and solar forecast at the price reference points not fetched for 24 hours: the cached forecast is used as far as it reaches, and unpublished prices beyond it fall back to the plain profile (see `docs/price.md`) | – |
| `forecast_inputs` | the engine had to guess (weather ending early, missing load profile, missing price forecast; the price fallback is expected without a wholesale source) | – |
| `control` (with a script) | – | the script failed, or the safe mode was sent |
| `history` | recording or the review failed | – |
| `load_forecast`, `solar_forecast` | day-ahead RMSE over 7 days worse than persistence | – |
| `soc_tracking` | above 5 points per hour over 7 days | – |
| `coverage` | PV planes without solar energy statistics (no load, no savings), or no battery charge/discharge statistics (no savings) | – |

After a start, the first plan runs once Home Assistant has started. Until it is done, for at most 30 minutes, the checks of what it fetches, records or sends (`plan`, `energy_data`, `wholesale`, `price_drivers`, `control`) are ok with the detail "waiting for the first plan".

A silent SOC sensor only counts while the plan expects the battery to move, because a full or empty battery can legitimately report nothing new for hours.

**Repairs:** a repair issue opens when the battery SOC has been unavailable for an hour, there has been no plan for an hour, the energy statistics have had no new data for a day, or the control script failed three times in a row while Active is on. It closes by itself when the cause is gone; the script issue also closes when Active is switched off.

## Diagnostics

The diagnostics download contains the settings with entity ids, the script and mode names redacted and the solar planes reduced to a count and total capacity; the plan's summary, inputs and warnings; control and calibration state; and the last 35 days of the history file (slot records, measured series, and the forecasts at the two leads the review scores). That includes meter data: share it only with people you trust.

## Known limitations

- Actuals are hourly statistics, so the actual cost uses the hour's mean price; differences between quarter-hour prices within an hour are not visible.
- Measured PV is after curtailment, so in hours the plan switched solar off, the baseline sees less PV than it could have had.
- Past days are reviewed with the current battery settings; after a settings change they are scored again with the new ones.
- With PV planes but no solar energy statistics, the house load cannot be derived: savings and load accuracy are left out, and the coverage check says why.
- Without the battery's charge and discharge statistics, the house load includes the battery's own power: a baseline simulated on it would count the battery twice, so the savings are left out, and the coverage check says why.
