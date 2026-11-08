"""Setup helpers: Energy Dashboard defaults, price parsing, load balance and
input checks for the config and options flows.

This module has no Home Assistant imports so it can be tested on its own.
"""

from __future__ import annotations

from collections import Counter
from datetime import datetime
from typing import Any

from .modes import CONF_CHARGE_W, CONF_DISCHARGE_W, CONF_POWER_CONTROL, enabled_modes

# Keys shared with const.py; repeated here so this module imports nothing from Home Assistant.
CONF_SOC_ENTITY = "soc_entity"
CONF_PRICE_ENTITY = "price_entity"
CONF_GRID_IMPORT = "grid_import"
CONF_GRID_EXPORT = "grid_export"
CONF_SOLAR_ENERGY = "solar_energy"
CONF_BATTERY_CHARGE = "battery_charge"
CONF_BATTERY_DISCHARGE = "battery_discharge"
CONF_MAX_CHARGE_W = "max_charge_w"
CONF_MAX_DISCHARGE_W = "max_discharge_w"
CONF_MAX_IMPORT_W = "max_import_w"
CONF_MAX_EXPORT_W = "max_export_w"
CONF_MIN_SOC = "min_soc"
CONF_MAX_SOC = "max_soc"

ENERGY_KEYS = (CONF_GRID_IMPORT, CONF_GRID_EXPORT, CONF_SOLAR_ENERGY, CONF_BATTERY_CHARGE, CONF_BATTERY_DISCHARGE)

_TIME_KEYS = ("startsAt", "start", "from", "start_time", "time", "datetime")
_VALUE_KEYS = ("total", "value", "price", "price_eur", "price_per_kwh")


def _ts(value: Any) -> float | None:
    """Timestamp from a datetime, ISO string or number."""
    if isinstance(value, datetime):
        return value.timestamp()
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return datetime.fromisoformat(value).timestamp()
        except ValueError:
            return None
    return None


def parse_price_attributes(attributes: dict[str, Any], unit: str | None) -> list[dict[str, float]]:
    """Find price lists (e.g. today/tomorrow) in a sensor's attributes.

    Each list item needs a start time and a price; intervals end at the next
    start (or after the typical spacing for the last item).
    """
    scale = 0.01 if unit and "ct" in unit.lower() else 1.0
    points: dict[float, float] = {}
    for value in attributes.values():
        if not isinstance(value, list) or not value or not isinstance(value[0], dict):
            continue
        tkey = next((k for k in _TIME_KEYS if k in value[0]), None)
        vkey = next((k for k in _VALUE_KEYS if k in value[0]), None)
        if not tkey or not vkey:
            continue
        for item in value:
            t, v = _ts(item.get(tkey)), item.get(vkey)
            if t is not None and isinstance(v, (int, float)) and not isinstance(v, bool):
                points[t] = float(v) * scale
    starts = sorted(points)
    if not starts:
        return []
    gaps = [b - a for a, b in zip(starts, starts[1:]) if b > a]
    spacing = min(gaps) if gaps else 3600.0
    return [
        {
            "start": int(s),
            "end": int(min(starts[i + 1], s + spacing) if i + 1 < len(starts) else s + spacing),
            "import": points[s],
        }
        for i, s in enumerate(starts)
    ]


def load_groups(cfg: dict[str, Any]) -> dict[int, list[str]]:
    """Statistics that add to (+1) or subtract from (-1) the house load."""
    return {
        +1: cfg.get(CONF_GRID_IMPORT, []) + cfg.get(CONF_SOLAR_ENERGY, []) + cfg.get(CONF_BATTERY_DISCHARGE, []),
        -1: cfg.get(CONF_GRID_EXPORT, []) + cfg.get(CONF_BATTERY_CHARGE, []),
    }


def load_series(groups: dict[int, list[str]], stats: dict[str, list[dict[str, Any]]]) -> list[tuple[int, float]]:
    """Hourly house load in W from hourly energy changes (kWh) per statistic.

    `stats` maps statistic ids to recorder rows with `start` and `change`.
    An hour missing any of the series is skipped: it would give a wrong balance.
    """
    ids = {s for g in groups.values() for s in g}
    per_hour: dict[int, dict[str, float]] = {}
    for stat_id, rows in stats.items():
        for row in rows:
            t, change = _ts(row.get("start")), row.get("change")
            if t is not None and change is not None:
                per_hour.setdefault(int(t), {})[stat_id] = change
    history = []
    for t in sorted(per_hour):
        vals = per_hour[t]
        if not ids or not ids <= vals.keys():
            continue
        kwh = sum(sign * vals[s] for sign, group in groups.items() for s in group)
        history.append((t, max(kwh, 0.0) * 1000.0))
    return history


def dashboard_defaults(prefs: dict[str, Any] | None) -> dict[str, Any]:
    """Source step defaults from the Energy Dashboard preferences.

    Energy statistics are lists of `sensor.` statistic ids (only entity-based
    statistics can be chosen with an entity selector). The price entity is
    set when the grid sources name exactly one.
    """
    out: dict[str, list[str]] = {k: [] for k in ENERGY_KEYS}
    prices: list[str] = []
    for src in (prefs or {}).get("energy_sources", []):
        kind = src.get("type")
        if kind == "grid":
            # Unified format (stat_energy_from/to on the source) or the legacy flow_from/flow_to lists.
            out[CONF_GRID_IMPORT] += [s for s in [src.get("stat_energy_from")] if s]
            out[CONF_GRID_EXPORT] += [s for s in [src.get("stat_energy_to")] if s]
            prices += [p for p in [src.get("entity_energy_price")] if p]
            for flow in src.get("flow_from", []):
                out[CONF_GRID_IMPORT] += [s for s in [flow.get("stat_energy_from")] if s]
                prices += [p for p in [flow.get("entity_energy_price")] if p]
            out[CONF_GRID_EXPORT] += [s for s in (f.get("stat_energy_to") for f in src.get("flow_to", [])) if s]
        elif kind == "solar":
            out[CONF_SOLAR_ENERGY] += [s for s in [src.get("stat_energy_from")] if s]
        elif kind == "battery":
            out[CONF_BATTERY_DISCHARGE] += [s for s in [src.get("stat_energy_from")] if s]
            out[CONF_BATTERY_CHARGE] += [s for s in [src.get("stat_energy_to")] if s]
    defaults: dict[str, Any] = {k: [s for s in v if s.startswith("sensor.")] for k, v in out.items()}
    unique = set(prices)
    if len(unique) == 1 and next(iter(unique)).startswith("sensor."):
        defaults[CONF_PRICE_ENTITY] = unique.pop()
    return defaults


def energy_ids(cfg: dict[str, Any]) -> list[str]:
    """All selected energy statistics, in form order, without repeats."""
    return list(dict.fromkeys(s for k in ENERGY_KEYS for s in cfg.get(k, [])))


def duplicate_statistics(cfg: dict[str, Any]) -> list[str]:
    """Statistics chosen more than once across the energy fields."""
    counts = Counter(s for k in ENERGY_KEYS for s in cfg.get(k, []))
    return [s for s, n in counts.items() if n > 1]


# Some battery management systems report a little over 100 % when full (or
# just below 0 %): such readings are clamped instead of rejected.
SOC_TOLERANCE = 5.0


def soc_value(state: str | None) -> float | None:
    """A state of charge in % from a sensor state, or None if it isn't one."""
    try:
        value = float(state) if state is not None else None
    except ValueError:
        return None
    if value is None or not -SOC_TOLERANCE <= value <= 100.0 + SOC_TOLERANCE:
        return None  # also NaN
    return min(max(value, 0.0), 100.0)


def check_battery(cfg: dict[str, Any]) -> dict[str, str]:
    """Errors for the battery step."""
    if float(cfg[CONF_MIN_SOC]) >= float(cfg[CONF_MAX_SOC]):
        return {"base": "soc_range"}
    return {}


def check_modes(cfg: dict[str, Any]) -> dict[str, str]:
    """Errors for the modes step: set powers within the battery or grid limits.

    With battery power control the battery runs at the set power, so it must fit
    the battery. With a grid setpoint the house load sits between grid and
    battery, so the setpoint only has to fit the grid connection.
    """
    battery = cfg.get(CONF_POWER_CONTROL, "grid_setpoint") == "battery_power"
    limits = {
        "charge": (CONF_CHARGE_W, CONF_MAX_CHARGE_W if battery else CONF_MAX_IMPORT_W),
        "discharge": (CONF_DISCHARGE_W, CONF_MAX_DISCHARGE_W if battery else CONF_MAX_EXPORT_W),
    }
    errors = {}
    for mode in enabled_modes(cfg):
        if mode in limits:
            power, limit = limits[mode]
            if float(cfg.get(power, 0.0)) > float(cfg[limit]):
                errors[power] = f"{power}_too_high"
    return errors
