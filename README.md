# 🦊 vulpes-ems

Open-source home energy management for Home Assistant. It's written in Rust and ships as a Python wheel plus a HACS integration.

vulpes-ems does two things:

- **It forecasts** your solar production, household load and electricity prices. Each forecast starts from a physics or statistical baseline that works without training data. Machine learning then corrects the remaining errors.
- **It decides** every 15 minutes which battery (ESS) mode to run over the next 48 hours. You pick which of its battery modes your system supports.

> Status: early development (phase 1, skeleton). Not usable yet.

## Design

| Area | Approach |
|---|---|
| Solar | Open-Meteo multi-model GHI/DNI/DHI → solar position (SPA) → Perez transposition per plane → temperature/inverter model. Arrays are entered roughly (direction, tilt, kWp) and calibrated from Home Assistant history, per string when the inverter reports per-MPPT power. Arrays on one inverter share its limit. A GBDT residual model and a weather-station nowcast correct the result. P10/P50/P90 come from the model ensemble. |
| Load | Weighted similar-day profile + GBDT residual (calendar, temperature, lags). Heating loads such as AC get a separate model. |
| Price | Day-ahead wholesale forecast (ENTSO-E fundamentals: wind, solar, load; plus gas) with an auto-fitted consumer tariff. Known published prices always take precedence. |
| Optimizer | Dynamic programming over slot × SOC × mode. The objective is import cost − export revenue + battery wear − terminal SOC value. A probabilistic night-reserve constraint guards against running empty. |
| Modes | Vulpes's own modes: charge from grid, hold (no discharge), self-consume, idle, discharge to grid, each optionally with solar switched off. You tick the ones your system supports and set their power. An existing mode entity (such as an `input_select`) can be mapped to them to compare the plan with what the house does. |
| Safety | Starts in shadow mode, with agreement and cost comparison against your current automation. To hand over control you pick a control script and turn the *Active* switch on; Vulpes then runs the script every 15 minutes. Without a usable plan it sends a safe mode (self-consume) instead. See [docs/control.md](docs/control.md). |

## Layout

- `crates/vulpes-core`: time grid, mode/battery types, storage
- `crates/vulpes-{solar,load,price}`: forecasters
- `crates/vulpes-ml`: shared features, recency weighting, backtesting
- `crates/vulpes-opt`: DP optimizer
- `crates/vulpes-sources`: Open-Meteo, ENTSO-E clients
- `crates/vulpes-py`: PyO3 bindings (PyPI package `vulpes-ems`)
- `crates/vulpes-cli`: backtest/replay tools
- `custom_components/vulpes_ems`: Home Assistant integration

## Development

```sh
cargo test --workspace
python3 -m venv .venv && .venv/bin/pip install maturin ziglang
# wheel for Home Assistant (Alpine/musl):
.venv/bin/maturin build --release -m crates/vulpes-py/Cargo.toml \
  --target x86_64-unknown-linux-musl --zig --compatibility musllinux_1_2 -o dist
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
