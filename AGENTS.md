# AGENTS.md

Guidance for AI coding agents (and humans) working on vulpes-ems.

## What this is

A home energy manager for Home Assistant. A Rust core does the forecasting (solar, load, prices) and a dynamic-programming optimizer that picks a battery mode per 15-minute slot. The core ships as a Python wheel (`vulpes-ems` on PyPI). A thin Home Assistant integration (`custom_components/vulpes_ems`) feeds it data and publishes the plan as entities.

## Layout

| Path | Contents |
|---|---|
| `crates/vulpes-core` | Time grid, mode/battery types, SQLite store |
| `crates/vulpes-sources` | Open-Meteo, EnergyZero and ENTSO-E clients |
| `crates/vulpes-solar` | Solar position, Perez transposition, PV model, calibration |
| `crates/vulpes-load` | Similar-day load profile, NL calendar, temperature correction |
| `crates/vulpes-price` | Tariff fit |
| `crates/vulpes-ml` | Weighted ridge regression, recency weights |
| `crates/vulpes-opt` | Slot physics (`step`) and DP optimizer |
| `crates/vulpes-engine` | JSON request → forecasts → plan, and solar calibration (what the integration calls) |
| `crates/vulpes-py` | PyO3 bindings, built with maturin (`vulpes_ems._core`) |
| `crates/vulpes-cli` | Backfill, calibration, evaluation and replay tools |
| `custom_components/vulpes_ems` | Home Assistant integration |
| `tests/` | Python tests for HA-independent integration code |
| `docs/` | Model documentation (solar, load, price, optimizer, control) |

## Commands

```sh
# the same checks as CI (rust.yml):
cargo fmt --all --check    # max_width = 110 (rustfmt.toml); `cargo fmt --all` to fix
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 -m venv .venv && .venv/bin/pip install pytest maturin ziglang
.venv/bin/python -m pytest -q tests
# wheel for Home Assistant (Alpine/musl):
.venv/bin/maturin build --release -m crates/vulpes-py/Cargo.toml \
  --target x86_64-unknown-linux-musl --zig --compatibility musllinux_1_2 -o dist
```

## Conventions

- **Rust edition 2024**, small crates, no async. HTTP uses `ureq` with ring + webpki-roots. Don't use reqwest/rustls defaults: aws-lc-sys breaks the zig/musl cross-builds.
- **Wheels are abi3 (cp313+)**, built for manylinux_2_28 and musllinux_1_2 on x86_64 and aarch64. Home Assistant Container runs Python 3.14 on Alpine (musl).
- **Engine API is JSON in, JSON out** (`vulpes_engine::plan_json` and `calibrate_json`, exposed as `vulpes_ems.plan` and `vulpes_ems.calibrate_solar`). Python releases the GIL and calls it in the executor.
- **Units:** power in W, energy in Wh (kWh at the HA boundary), prices in currency/kWh, timestamps in Unix seconds UTC. 15-minute slots start on the quarter hour.
- **Signs:** battery power positive = charging; grid power positive = import.
- **Information-time discipline:** evaluation and replay code may only use data that was available at decision time. Day-ahead weather comes from the Open-Meteo previous-runs API (`_previous_day1`).
- **Integration modes are Vulpes's own** (`modes.py`): charge, hold, self_consume, idle, discharge, plus `:pv_off` variants and `:no_export` variants (hold, self_consume, idle). The engine only sees behaviours (`grid_setpoint`, `self_consume`, `no_discharge`, `idle`, `battery_power`) and the `pv_curtail`/`no_export` flags. A user's own mode entity is optional and only mapped for comparison.
- **Active control goes through one user script** (`control.py`, `docs/control.md`): while the Active switch is on, Vulpes runs `script.turn_on` with fixed variables every 15 minutes, and the safe command (self_consume, `fallback: true`) when there is no usable plan. The variable names and mode values are an API: change them only with a release note. Vulpes never calls inverter services directly.
- **Settings:** the options flow saves a complete copy of the settings, which replaces the setup data (`modes.settings`). Don't merge data and options, or cleared fields come back.
- **One physics function:** the optimizer and replay both use `vulpes_opt::step`. Change slot behaviour there, never in a copy.
- **Integration and engine move together:** the manifest pins `vulpes-ems==x.y.z` exactly. A change to the JSON request/response needs a new wheel *and* a new integration version, released together.
- **Never change entity `unique_id`s.** Users would lose entity history, customisations and dashboards.
- **No blocking work on HA's event loop:** calls into Rust and all network or recorder access run in the executor (`async_add_executor_job`, the recorder's own executor).
- Keep `modes.py`, `control.py`, `solar_setup.py` and `setup_checks.py` free of Home Assistant imports so they stay testable in `tests/`. Flow input checks go in `setup_checks.py`.
- **Solar setup:** the array form stores engine planes with `inverter: 0` plus integration-only keys (`power_entity`), stripped by `engine_planes()` before any engine call. `solar_setup` in the settings is `form` or `json`; setups from before 0.3.0 have neither and are treated as JSON. Calibration results live in `.storage/vulpes_ems.<entry_id>.calibration` and are only applied by the user, through the options flow.
- `strings.json` and `translations/en.json` must stay identical, and each options step has the same labels as its config step (`tests/test_strings.py`). The options flow is a menu; each section saves the full settings copy.
- **Translations:** every string change also updates `translations/nl.json`. It must have the same keys and `{placeholders}` as `en.json` (`tests/test_strings.py`). Mode values in script variables stay English: they are an API.
- **Licensing:** MIT OR Apache-2.0. No per-file license headers.

## Testing

**Unit-test everything in the Rust codebase.** Every module with logic has a `#[cfg(test)] mod tests` next to the code. Write tests for:
- normal cases;
- **edge cases:** empty or missing input, a single value, DST transitions (23/25-hour days), slot boundaries, SOC at its limits, zero/negative power and prices, NaN-free output;
- **error paths:** invalid config, unparsable API responses, singular fits.

Rules:
- **No network or real data in tests.** API clients test their parsers on small inline samples (for example XML/JSON snippets). Tests build their own synthetic inputs.
- **Bug fixes come with a regression test** that fails without the fix.
- **New or changed behaviour comes with tests in the same change.** Before committing, the checks under Commands must pass.
- **CLI commands:** put the logic in plain functions that take data and return results, and unit-test those (for example `replay::simulate`, `load::evaluate_days`, `solar::calibrate_panels`). The command function only loads data, calls them and prints. Keep `main.rs` thin.
- **Refactoring commands:** run the command on real local data before and after, and check that the output is identical.
- **Serde formats are part of the API:** test the JSON round-trip of engine requests/responses and of `Behaviour`/`ModeDef`, so the Python side doesn't silently break.
- **Physics against references:** solar changes must keep `crates/vulpes-solar/tests/pvlib_reference.rs` green (pvlib 0.16.1, solar constant 1366.1 W/m²). Regenerate reference values with pvlib, never by copying our own output.
- **Python:** HA-independent code (like `modes.py`) gets pytest tests in `tests/`.

Known coverage gaps to fill:
- **Network calls** (`open_meteo::fetch`, `entsoe::fetch`, `energyzero::fetch_hourly`): only the request building and response parsing are tested; there are no tests against live services by design.
- **`vulpes-py`** is tested from Python (`tests/test_bindings.py`), against the built module. PyO3 functions can't be unit-tested from Rust because the extension module doesn't link libpython.

## Model changes

- **Measure before adopting.** A new model, feature or ML correction only goes in if it beats the current baseline on held-out data:
  - split by even/odd ISO week;
  - use day-ahead weather (Open-Meteo previous-runs `_previous_day1`), never the archive of actual weather;
  - report MAE, RMSE, bias and a naive/persistence baseline (`vulpes-cli eval-*`).
- Keep the physics/statistical baseline working without training data. ML only corrects its residuals.
- Update `docs/*.md` when a model changes. Docs describe methods, never a specific house's figures.

## Home Assistant pitfalls (2026.11+)

- **Selectors are validated with `probatio`.** An invalid selector config makes the flow step fail with a bare HTTP 400 and **nothing is logged**. Known rules:
  - `NumberSelectorConfig` must not get `unit_of_measurement=None`; leave the key out.
  - `step` must be ≥ 0.001 or `"any"`.
- **Test config flows by clicking through them** (or via the REST flow API on a throwaway instance), not only with a pre-made config entry. A pre-made entry skips every form.
- **Updating a loaded integration needs a restart.** The Marketplace replaces HACS and can install new integrations without a restart, but not updates to one that's already loaded in the current run.
- `DeviceInfo` is a keyword-only dataclass.
- Use `DataUpdateCoordinator(config_entry=...)`. Don't put large data in recorded state attributes; use `_unrecorded_attributes`.
- Bump the config entry `VERSION` and add `async_migrate_entry` when the settings format changes.

## Testing against Home Assistant

- **Never test against someone's production Home Assistant.** Use a throwaway container on another port with a scratch config, for example `ghcr.io/home-assistant/home-assistant:<version>` with `-p 127.0.0.1:18124:8123`.
- A small fixture integration can inject states and statistics.
- Check the log for `vulpes_ems` warnings/errors and the entity states, then remove the container.

## Local data workflow (CLI)

The CLI works on a local SQLite store (gitignored `local/vulpes.db`):
1. **`import-ha`** reads statistics from a **copy** of a Home Assistant database. Never open the live database.
2. **`backfill-weather`, `backfill-prices` and `backfill-entsoe`** fetch the history the models need.
3. **`calibrate-solar`, `build-load` and `fit-tariff`** fit the models.
4. **`eval-solar`, `eval-load` and `eval-price`** score them.
5. **`replay`** compares strategies hour by hour over history.

Run `cargo run -p vulpes-cli -- --help` for the flags.

## CI

| Workflow | What it checks |
|---|---|
| `rust.yml` | `cargo test`, clippy, fmt |
| `ha.yml` | hassfest and HACS/Marketplace validation |
| `wheels.yml` | Builds the 4 wheels and smoke-tests on Alpine. On `v*` tags it also publishes to PyPI. |

All three must be green before a release.

## External APIs

- **Keep requests low:** Open-Meteo, EnergyZero and ENTSO-E are free services. Cache, and fetch only the history windows the models need.
- **User-Agent:** the generic `vulpes-ems/<version>`. Never put personal data (email, location) in headers or URLs beyond the rounded coordinates.
- **Respect rate limits,** and keep errors free of tokens.

## Known gaps and parked ideas

- `GridLimits::max_import_w` isn't applied in `vulpes_opt::step`; only the export limit is.
- **Hybrid inverters:** PV behind a DC-coupled battery is limited by the PV input, not the AC output. The form's inverter limit is a single number; a separate AC limit for (PV − battery charge) is not modelled.
- **Per-phase limits:** a single-phase inverter is limited by its phase fuse together with the house load on that phase. Not modelled; parked until needed.
- **HTTP timeout** is 60 s (`vulpes-sources/src/lib.rs`). A hanging API can delay the integration's first refresh, so it should probably be about 20 s.

## Release process

1. **Bump the version everywhere:**
   - `[workspace.package] version` in `Cargo.toml`, and the `version = "x.y.z", path = …` dependency entries in `crates/*/Cargo.toml`;
   - `custom_components/vulpes_ems/manifest.json`: both `version` and the `vulpes-ems==x.y.z` requirement.
2. **Check:** run fmt, clippy, the Rust tests and pytest (see Commands). Commit, push.
3. **Tag `vX.Y.Z` and push the tag.** `wheels.yml` builds the 4 wheels, smoke-tests on Alpine and publishes to PyPI via trusted publishing (environment `pypi`).
4. **Create the GitHub Release only after PyPI has the wheels.** The Marketplace offers the update from the release, and HA installs the requirement from PyPI.

## Privacy

This project is developed against a real house. **Never commit personal data:**
- coordinates, addresses, entity ids that identify devices (serial numbers), meter data, tokens;
- tuned site values in tests or docs: use generic numbers.

Local data lives in the gitignored `local/` directory (SQLite store, entity mapping, site config, API tokens). Coordinates are rounded to 0.01° before any external request. API tokens (ENTSO-E) come from the environment or `local/`; never print them or include them in errors.
