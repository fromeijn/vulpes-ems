"""Coordinator: gathers Home Assistant data every 15 minutes and plans in Rust."""

from __future__ import annotations

from collections.abc import Callable
from datetime import datetime, timedelta
import json
import logging
from typing import Any

import vulpes_ems

from homeassistant.components.recorder import get_instance
from homeassistant.components.recorder.statistics import statistics_during_period
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.event import async_track_time_change
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from homeassistant.util import dt as dt_util

from .const import (
    CONF_BATTERY_CHARGE,
    CONF_BATTERY_DISCHARGE,
    CONF_CAPACITY_KWH,
    CONF_CHARGE_EFF,
    CONF_DISCHARGE_EFF,
    CONF_EXPORT_A,
    CONF_EXPORT_B,
    CONF_GRID_EXPORT,
    CONF_GRID_IMPORT,
    CONF_MAX_CHARGE_W,
    CONF_MAX_DISCHARGE_W,
    CONF_MAX_EXPORT_W,
    CONF_MAX_IMPORT_W,
    CONF_MAX_SOC,
    CONF_MIN_SOC,
    CONF_NET_METERING,
    CONF_PLANES,
    CONF_PRICE_ENTITY,
    CONF_SOC_ENTITY,
    CONF_SOLAR_ENERGY,
    CONF_TARIFF_A,
    CONF_TARIFF_B,
    CONF_WEAR_COST,
    CONF_WHOLESALE_SOURCE,
    DOMAIN,
    HORIZON_HOURS,
    LOAD_HISTORY_DAYS,
    WHOLESALE_HISTORY_DAYS,
)
from .calibration import SolarCalibration
from .modes import CONF_MODE_ENTITY, engine_modes, settings, mode_of_state, split_mode
from .solar_setup import engine_planes

_LOGGER = logging.getLogger(__name__)

_TIME_KEYS = ("startsAt", "start", "from", "start_time", "time", "datetime")
_VALUE_KEYS = ("total", "value", "price", "price_eur")


def _ts(value: Any) -> float | None:
    """Timestamp from a datetime, ISO string or number."""
    if isinstance(value, datetime):
        return value.timestamp()
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        parsed = dt_util.parse_datetime(value)
        return parsed.timestamp() if parsed else None
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
            if t is not None and isinstance(v, (int, float)):
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


def _row_start(row: dict[str, Any]) -> int | None:
    t = _ts(row.get("start"))
    return int(t) if t is not None else None


class VulpesCoordinator(DataUpdateCoordinator[dict[str, Any]]):
    """Plans every 15 minutes; tracks agreement with the current mode."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry) -> None:
        super().__init__(hass, _LOGGER, config_entry=entry, name=DOMAIN, update_interval=None)
        self._unsub: Callable[[], None] | None = None
        self._wholesale: list[tuple[int, float]] = []
        self._wholesale_fetched: datetime | None = None
        self.agreement_day: str | None = None
        self.agree = 0
        self.compared = 0
        self.agrees_now: bool | None = None
        self.calibration = SolarCalibration(hass, entry)

    @property
    def cfg(self) -> dict[str, Any]:
        return settings(self.config_entry.data, self.config_entry.options)

    def start_schedule(self) -> None:
        self._unsub = async_track_time_change(self.hass, self._tick, minute=[0, 15, 30, 45], second=20)

    def stop_schedule(self) -> None:
        if self._unsub:
            self._unsub()
            self._unsub = None

    @callback
    def _tick(self, _now: datetime) -> None:
        self.hass.async_create_task(self.async_refresh())

    async def _load_history(self, now: datetime) -> list[tuple[int, float]]:
        cfg = self.cfg
        groups = {
            +1: cfg.get(CONF_GRID_IMPORT, []) + cfg.get(CONF_SOLAR_ENERGY, []) + cfg.get(CONF_BATTERY_DISCHARGE, []),
            -1: cfg.get(CONF_GRID_EXPORT, []) + cfg.get(CONF_BATTERY_CHARGE, []),
        }
        ids = {s for g in groups.values() for s in g}
        if not ids:
            raise UpdateFailed("no energy statistics selected")
        start = (now - timedelta(days=LOAD_HISTORY_DAYS)).replace(minute=0, second=0, microsecond=0)
        stats = await get_instance(self.hass).async_add_executor_job(
            statistics_during_period, self.hass, start, None, ids, "hour", {"energy": "kWh"}, {"change"}
        )
        per_hour: dict[int, dict[str, float]] = {}
        for stat_id, rows in stats.items():
            for row in rows:
                t, change = _row_start(row), row.get("change")
                if t is not None and change is not None:
                    per_hour.setdefault(t, {})[stat_id] = change
        history = []
        for t in sorted(per_hour):
            vals = per_hour[t]
            if not ids <= vals.keys():
                continue  # an hour missing any series would give a wrong balance
            kwh = sum(sign * vals[s] for sign, group in groups.items() for s in group)
            history.append((t, max(kwh, 0.0) * 1000.0))
        return history

    async def _wholesale_history(self, now: datetime) -> list[tuple[int, float]]:
        if self.cfg.get(CONF_WHOLESALE_SOURCE) != "energyzero":
            return []
        if self._wholesale_fetched and now - self._wholesale_fetched < timedelta(hours=6):
            return self._wholesale
        start = (now - timedelta(days=WHOLESALE_HISTORY_DAYS)).date().isoformat()
        end = (now + timedelta(days=1)).date().isoformat()
        try:
            self._wholesale = await self.hass.async_add_executor_job(
                vulpes_ems.energyzero_hourly, start, end
            )
            self._wholesale_fetched = now
        except ValueError as err:
            _LOGGER.warning("Wholesale price history unavailable, keeping cached data: %s", err)
        return self._wholesale

    def _request(self, now: datetime, soc_pct: float, load, known, wholesale) -> dict[str, Any]:
        cfg = self.cfg
        cap_wh = float(cfg[CONF_CAPACITY_KWH]) * 1000.0
        return {
            "now": int(now.timestamp()),
            "horizon_h": HORIZON_HOURS,
            # Rounded to ~1 km in the core before any external request.
            "location": {"lat": self.hass.config.latitude, "lon": self.hass.config.longitude},
            "timezone": str(self.hass.config.time_zone),
            "country": (self.hass.config.country or "").lower(),
            "battery": {
                "capacity_wh": cap_wh,
                "max_charge_w": float(cfg[CONF_MAX_CHARGE_W]),
                "max_discharge_w": float(cfg[CONF_MAX_DISCHARGE_W]),
                "charge_eff": float(cfg[CONF_CHARGE_EFF]),
                "discharge_eff": float(cfg[CONF_DISCHARGE_EFF]),
                "min_soc": float(cfg[CONF_MIN_SOC]) / 100.0,
                "max_soc": float(cfg[CONF_MAX_SOC]) / 100.0,
                "wear_cost_per_kwh": float(cfg[CONF_WEAR_COST]),
            },
            "grid": {"max_import_w": float(cfg[CONF_MAX_IMPORT_W]), "max_export_w": float(cfg[CONF_MAX_EXPORT_W])},
            "modes": engine_modes(cfg),
            "soc_wh": soc_pct / 100.0 * cap_wh,
            "planes": engine_planes(cfg[CONF_PLANES]),
            "load_history": load,
            "known_prices": known,
            "tariff": {"a": float(cfg[CONF_TARIFF_A]), "b": float(cfg[CONF_TARIFF_B])},
            "export": {
                "net_metering": bool(cfg[CONF_NET_METERING]),
                "a": float(cfg[CONF_EXPORT_A]),
                "b": float(cfg[CONF_EXPORT_B]),
            },
            "wholesale_history": wholesale,
        }

    async def _async_update_data(self) -> dict[str, Any]:
        cfg = self.cfg
        now = dt_util.utcnow()
        soc_state = self.hass.states.get(cfg[CONF_SOC_ENTITY])
        try:
            soc_pct = float(soc_state.state) if soc_state else None
        except ValueError:
            soc_pct = None
        if soc_pct is None:
            raise UpdateFailed(f"battery SOC from {cfg[CONF_SOC_ENTITY]} is unavailable")
        price_state = self.hass.states.get(cfg[CONF_PRICE_ENTITY])
        known = (
            parse_price_attributes(dict(price_state.attributes), price_state.attributes.get("unit_of_measurement"))
            if price_state
            else []
        )
        load = await self._load_history(now)
        wholesale = await self._wholesale_history(now)
        request = self._request(now, soc_pct, load, known, wholesale)
        try:
            raw = await self.hass.async_add_executor_job(vulpes_ems.plan, json.dumps(request))
        except ValueError as err:
            raise UpdateFailed(f"planning failed: {err}") from err
        plan = json.loads(raw)
        for s in plan["slots"]:
            s["mode"], s["pv_off"] = split_mode(s["mode"])
        plan["summary"]["mode_now"], plan["summary"]["pv_off_now"] = split_mode(plan["summary"]["mode_now"])
        plan["inputs"] = {"load_hours": len(load), "known_price_slots": len(known), "wholesale_hours": len(wholesale)}
        self._track_agreement(plan)
        return plan

    def _track_agreement(self, plan: dict[str, Any]) -> None:
        """Compare the plan with the user's own mode entity, if one is mapped."""
        entity_id = self.cfg.get(CONF_MODE_ENTITY)
        mode_state = self.hass.states.get(entity_id) if entity_id else None
        today = dt_util.now().date().isoformat()
        if today != self.agreement_day:
            self.agreement_day, self.agree, self.compared = today, 0, 0
        if not mode_state or mode_state.state in ("unknown", "unavailable"):
            self.agrees_now = None
            return
        # A state that is not mapped to any Vulpes mode counts as disagreement.
        self.agrees_now = mode_of_state(self.cfg, mode_state.state) == plan["summary"]["mode_now"]
        self.compared += 1
        self.agree += int(self.agrees_now)
