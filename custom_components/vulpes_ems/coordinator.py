"""Coordinator: gathers Home Assistant data every 15 minutes, plans in Rust and,
when the Active switch is on, runs the user's control script."""

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
    CONF_CAPACITY_KWH,
    CONF_CHARGE_EFF,
    CONF_DISCHARGE_EFF,
    CONF_EXPORT_A,
    CONF_EXPORT_B,
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
from .control import command, safe_command
from .modes import CONF_CONTROL_SCRIPT, CONF_MODE_ENTITY, engine_modes, settings, mode_of_state, split_mode
from .setup_checks import load_groups, load_series, parse_price_attributes, soc_value
from .solar_setup import engine_planes

_LOGGER = logging.getLogger(__name__)


async def async_energy_changes(
    hass: HomeAssistant, ids: set[str], start: datetime
) -> dict[str, list[dict[str, Any]]]:
    """Hourly energy changes (kWh) per statistic since `start`, in the recorder's executor."""
    return await get_instance(hass).async_add_executor_job(
        statistics_during_period, hass, start, None, ids, "hour", {"energy": "kWh"}, {"change"}
    )


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
        # Active control: set by the Active switch (restored across restarts).
        self.active = False
        self.last_command: dict[str, Any] | None = None
        self.last_error: str | None = None

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
        self.hass.async_create_task(self._async_refresh_and_apply())

    async def _async_refresh_and_apply(self) -> None:
        await self.async_refresh()
        await self.async_apply()

    @property
    def control_script(self) -> str | None:
        return self.cfg.get(CONF_CONTROL_SCRIPT) or None

    async def async_set_active(self, active: bool) -> None:
        """Hand over control (apply the plan now) or take it back (send the safe command once)."""
        was_active, self.active = self.active, active
        if active:
            await self.async_apply()
        elif was_active and self.control_script:
            await self._async_run_script(safe_command(self.cfg, dt_util.utcnow().timestamp(), fallback=False))
        else:
            self.async_update_listeners()

    async def async_apply(self) -> None:
        """Run the control script for the current slot, or the safe command without a usable plan."""
        if not self.active or not self.control_script:
            return
        now = dt_util.utcnow().timestamp()
        cmd = command(self.cfg, self.data, now, failed=not self.last_update_success)
        if cmd["fallback"]:
            _LOGGER.warning("No usable plan; sending the safe mode (%s) to %s", cmd["mode"], self.control_script)
        await self._async_run_script(cmd)

    async def _async_run_script(self, cmd: dict[str, Any]) -> None:
        script = self.control_script
        try:
            await self.hass.services.async_call(
                "script", "turn_on", {"entity_id": script, "variables": cmd}, blocking=True
            )
        except Exception as err:  # noqa: BLE001 - a failing script must not stop the schedule
            _LOGGER.warning("Control script %s failed: %s", script, err)
            self.last_error = str(err)
        else:
            self.last_error = None
        self.last_command = {**cmd, "sent": dt_util.utcnow().isoformat()}
        self.async_update_listeners()

    async def _load_history(self, now: datetime) -> list[tuple[int, float]]:
        groups = load_groups(self.cfg)
        ids = {s for g in groups.values() for s in g}
        if not ids:
            raise UpdateFailed("no energy statistics selected")
        start = (now - timedelta(days=LOAD_HISTORY_DAYS)).replace(minute=0, second=0, microsecond=0)
        return load_series(groups, await async_energy_changes(self.hass, ids, start))

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
        soc_pct = soc_value(soc_state.state) if soc_state else None
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
            s["mode"], s["pv_off"], s["no_export"] = split_mode(s["mode"])
        summary = plan["summary"]
        summary["mode_now"], summary["pv_off_now"], summary["no_export_now"] = split_mode(summary["mode_now"])
        plan["inputs"] = {"load_hours": len(load), "known_price_slots": len(known), "wholesale_hours": len(wholesale)}
        plan["planned_at"] = int(now.timestamp())
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
