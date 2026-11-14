# 🦊 vulpes-ems

Open-source home energy management for Home Assistant. It's written in Rust and ships as a Python wheel plus a HACS integration.

vulpes-ems does two things:

- **It forecasts** your solar production, household load and electricity prices. Each forecast is a physics or statistical model that works without training data.
- **It decides** every 15 minutes which battery (ESS) mode to run over the next 84 hours. You pick which of its battery modes your system supports.

> Status: early development (phase 1, skeleton). Not usable yet.

## Design

| Area | Approach |
|---|---|
| Solar | Open-Meteo multi-model GHI/DNI/DHI → solar position (SPA) → Perez transposition per plane → temperature/inverter model. Arrays are entered roughly (direction, tilt, kWp) and calibrated from Home Assistant history, per string when the inverter reports per-MPPT power. Arrays on one inverter share its limit. The forecast is the mean over two weather models. |
| Load | Weighted similar-day profile: the same hour on recent days of the same type (workday, Saturday, Sunday or holiday), with recent days counting more. |
| Price | Published day-ahead prices where available. Later hours use a similar-day profile of recent wholesale prices, converted with your tariff (`a × wholesale + b`). |
| Optimizer | Dynamic programming over slot × SOC × mode. The objective is import cost − export revenue + battery wear − terminal SOC value. A probabilistic night-reserve constraint guards against running empty. |
| Modes | Vulpes's own modes: charge from grid, hold (no discharge), self-consume, idle, discharge to grid, each optionally with solar switched off. You tick the ones your system supports and set their power. An existing mode entity (such as an `input_select`) can be mapped to them to compare the plan with what the house does. |
| Safety | Starts in shadow mode, with agreement against your current automation and a "Plan would save" sensor that shows what handing over control is worth. To hand over control you pick a control script and turn the *Active* switch on; Vulpes then runs the script every 15 minutes. Without a usable plan it sends a safe mode (self-consume) instead. See [docs/control.md](docs/control.md). |
| Monitoring | A Status sensor (ok, degraded, problem) with the reason for each check, repair issues for problems that last, and performance sensors from Vulpes's own history file: savings against self-consumption, forecast errors, and whether the battery followed the plan. See [docs/monitoring.md](docs/monitoring.md). |

## Layout

- `crates/vulpes-core`: time grid, mode/battery types, error metrics, storage
- `crates/vulpes-{solar,load,price}`: forecasters
- `crates/vulpes-opt`: DP optimizer
- `crates/vulpes-sources`: Open-Meteo, ENTSO-E clients
- `crates/vulpes-py`: PyO3 bindings (PyPI package `vulpes-ems`)
- `crates/vulpes-ha`: Home Assistant adapter (settings, checks, control commands, history and health)
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
