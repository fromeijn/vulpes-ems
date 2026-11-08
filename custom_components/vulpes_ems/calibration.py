"""Solar calibration: reads PV history from the recorder, fits the planes in
Rust, and keeps the result as a proposal the user can apply in the options flow."""

from __future__ import annotations

from collections.abc import Callable
from datetime import timedelta
import json
import logging
from typing import Any

import vulpes_ems

from homeassistant.components import persistent_notification
from homeassistant.components.recorder import get_instance
from homeassistant.components.recorder.statistics import statistics_during_period
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import CALLBACK_TYPE, HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.event import async_call_later
from homeassistant.helpers.storage import Store
from homeassistant.util import dt as dt_util

from .const import CALIBRATION_DELAY_S, CALIBRATION_HISTORY_DAYS, CONF_PLANES, CONF_SOLAR_ENERGY, DOMAIN
from .modes import settings
from .solar_setup import (
    CONF_POWER_ENTITY,
    CONF_SOLAR_SETUP,
    calibration_request,
    engine_planes,
    has_changes,
    hourly_series,
    improvement_pct,
    total_power,
)

_LOGGER = logging.getLogger(__name__)

NOTIFICATION_ID = f"{DOMAIN}_solar_calibration"


class SolarCalibration:
    """The latest calibration result for one config entry, persisted in `.storage`."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry) -> None:
        self.hass = hass
        self.entry = entry
        self._store: Store[dict[str, Any]] = Store(hass, 1, f"{DOMAIN}.{entry.entry_id}.calibration")
        self._stored: dict[str, Any] = {}
        self._listeners: list[Callable[[], None]] = []
        self._cancel_auto: CALLBACK_TYPE | None = None
        self.running = False
        self.error: str | None = None

    @property
    def cfg(self) -> dict[str, Any]:
        return settings(self.entry.data, self.entry.options)

    def _current(self) -> list[dict[str, Any]]:
        return engine_planes(self.cfg.get(CONF_PLANES, []))

    @property
    def applied(self) -> bool:
        """Whether the current planes are the applied result of the last calibration."""
        return bool(self._stored.get("result")) and self._stored.get("applied") == self._current()

    @property
    def result(self) -> dict[str, Any] | None:
        """The last result, if it belongs to the planes as they are now (or was applied to them)."""
        result = self._stored.get("result")
        if result and (self._stored.get("based_on") == self._current() or self.applied):
            return result
        return None

    @property
    def proposal(self) -> dict[str, Any] | None:
        """The result, if it was computed for the current planes and would change them."""
        result = self._stored.get("result")
        if result and self._stored.get("based_on") == self._current() and has_changes(
            result, self.cfg.get(CONF_PLANES, [])
        ):
            return result
        return None

    async def async_mark_applied(self, planes: list[dict[str, Any]]) -> None:
        """Remember that `planes` are the applied proposal (called by the options flow)."""
        self._stored["applied"] = engine_planes(planes)
        await self._store.async_save(self._stored)
        self.async_dismiss()

    @property
    def last_run(self) -> str | None:
        return self._stored.get("last_run")

    @property
    def status(self) -> str:
        if self.running:
            return "running"
        if self.error:
            return "failed"
        if self.applied:
            return "applied"
        if self.result:
            return "done"
        return "stale" if self._stored.get("result") else "not_run"

    def improvement(self) -> float | None:
        """Forecast improvement of the last result in %; 0 when it changed nothing."""
        result = self.result
        if not result:
            return None
        if not has_changes(result, self._stored.get("based_on", [])):
            return 0.0
        return improvement_pct(result)

    @callback
    def async_add_listener(self, update: Callable[[], None]) -> Callable[[], None]:
        self._listeners.append(update)
        return lambda: self._listeners.remove(update)

    @callback
    def _notify_listeners(self) -> None:
        for update in list(self._listeners):
            update()

    async def async_load(self) -> None:
        self._stored = await self._store.async_load() or {}
        # Run once by itself for setups made with the array form; advanced
        # (JSON) setups calibrate only when asked.
        if self.cfg.get(CONF_SOLAR_SETUP) == "form" and not self._stored.get("auto_ran"):
            self._cancel_auto = async_call_later(self.hass, CALIBRATION_DELAY_S, self._auto_run)

    async def _auto_run(self, _now: Any) -> None:
        self._cancel_auto = None
        self._stored["auto_ran"] = True
        try:
            await self.async_run()
        except HomeAssistantError as err:
            _LOGGER.warning("Automatic solar calibration failed: %s", err)
            await self._store.async_save(self._stored)

    @callback
    def async_unload(self) -> None:
        if self._cancel_auto:
            self._cancel_auto()
            self._cancel_auto = None

    async def async_remove(self) -> None:
        await self._store.async_remove()

    async def _strings(self, planes: list[dict[str, Any]], start) -> dict[str, list[tuple[int, float]]]:
        ids = {p[CONF_POWER_ENTITY] for p in planes if p.get(CONF_POWER_ENTITY)}
        if not ids:
            return {}
        stats = await get_instance(self.hass).async_add_executor_job(
            statistics_during_period, self.hass, start, None, ids, "hour", {"power": "W"}, {"mean"}
        )
        return {i: hourly_series(stats.get(i, []), "mean") for i in ids}

    async def _total(self, start) -> list[tuple[int, float]]:
        ids = set(self.cfg.get(CONF_SOLAR_ENERGY, []))
        if not ids:
            return []
        stats = await get_instance(self.hass).async_add_executor_job(
            statistics_during_period, self.hass, start, None, ids, "hour", {"energy": "kWh"}, {"change"}
        )
        return total_power({i: hourly_series(stats.get(i, []), "change") for i in ids})

    async def async_run(self) -> dict[str, Any]:
        """Calibrate now; returns the result. Raises HomeAssistantError on failure."""
        if self.running:
            raise HomeAssistantError("solar calibration is already running")
        planes = self.cfg.get(CONF_PLANES, [])
        if not planes:
            raise HomeAssistantError("no solar arrays configured")
        self.running, self.error = True, None
        self._notify_listeners()
        try:
            start = (dt_util.utcnow() - timedelta(days=CALIBRATION_HISTORY_DAYS)).replace(
                minute=0, second=0, microsecond=0
            )
            strings = await self._strings(planes, start)
            total = await self._total(start)
            missing = [i for i, s in strings.items() if not s]
            request = calibration_request(
                self.hass.config.latitude, self.hass.config.longitude, planes, strings, total
            )
            try:
                raw = await self.hass.async_add_executor_job(vulpes_ems.calibrate_solar, json.dumps(request))
            except ValueError as err:
                raise HomeAssistantError(f"solar calibration failed: {err}") from err
            result = json.loads(raw)
            result["warnings"] = [
                f"no hourly statistics for {i}; it needs state_class measurement and some history" for i in missing
            ] + result.get("warnings", [])
            self._stored.update(
                result=result,
                based_on=engine_planes(planes),
                last_run=dt_util.utcnow().isoformat(),
            )
            await self._store.async_save(self._stored)
            self._notify(result)
            return result
        except HomeAssistantError as err:
            self.error = str(err)
            raise
        finally:
            self.running = False
            self._notify_listeners()

    def _notify(self, result: dict[str, Any]) -> None:
        if not has_changes(result, self.cfg.get(CONF_PLANES, [])):
            message = "Solar calibration found nothing to change."
            if result.get("warnings"):
                message += "\n\n" + "\n".join(f"- {w}" for w in result["warnings"])
        else:
            pct = improvement_pct(result)
            better = f" It lowers the hourly forecast error by {pct:.0f}%." if pct is not None else ""
            message = (
                f"Vulpes EMS has a calibrated solar setup ready.{better}\n\n"
                "Review and apply it under Settings → Devices & services → Vulpes EMS → Configure → "
                "Solar arrays → Apply calibration."
            )
        persistent_notification.async_create(
            self.hass, message, title="Vulpes EMS: solar calibration", notification_id=NOTIFICATION_ID
        )

    @callback
    def async_dismiss(self) -> None:
        persistent_notification.async_dismiss(self.hass, NOTIFICATION_ID)
