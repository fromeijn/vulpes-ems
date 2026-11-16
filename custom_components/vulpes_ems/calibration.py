"""Solar calibration: reads PV history from the recorder, fits the planes in
Rust, and keeps the result as a proposal the user can apply in the options flow."""

from __future__ import annotations

from collections.abc import Callable
from datetime import datetime, timedelta
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

from .const import CALIBRATION_DELAY_S, DOMAIN
from .core import C, dumps, ha, stat_rows

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
        return ha("settings", data=dict(self.entry.data), options=dict(self.entry.options))

    def _view(self) -> dict[str, Any]:
        return ha("calibration_view", cfg=self.cfg, stored=self._stored, running=self.running, error=bool(self.error))

    @property
    def applied(self) -> bool:
        """Whether the current planes are the applied result of the last calibration."""
        return self._view()["applied"]

    @property
    def result(self) -> dict[str, Any] | None:
        """The last result, if it belongs to the planes as they are now (or was applied to them)."""
        return self._view()["result"]

    @property
    def proposal(self) -> dict[str, Any] | None:
        """The result, if it was computed for the current planes and would change them."""
        return self._view()["proposal"]

    async def async_mark_applied(self, planes: list[dict[str, Any]]) -> None:
        """Remember that `planes` are the applied proposal (called by the options flow)."""
        self._stored["applied"] = ha("engine_planes", planes=planes)
        await self._store.async_save(self._stored)
        self.async_dismiss()

    @property
    def last_run(self) -> str | None:
        return self._stored.get("last_run")

    @property
    def status(self) -> str:
        return self._view()["status"]

    def improvement(self) -> float | None:
        """Forecast improvement of the last result in %; 0 when it changed nothing."""
        return self._view()["improvement"]

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
        if self._view()["auto_run"]:
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

    async def _statistics(self, ids: list[str], start: datetime, units: dict, field: str) -> dict[str, list]:
        if not ids:
            return {}
        stats = await get_instance(self.hass).async_add_executor_job(
            statistics_during_period, self.hass, start, None, set(ids), "hour", units, {field}
        )
        return stat_rows(stats, ids, field)

    async def async_run(self) -> dict[str, Any]:
        """Calibrate now; returns the result. Raises HomeAssistantError on failure."""
        if self.running:
            raise HomeAssistantError("solar calibration is already running")
        cfg = self.cfg
        inputs = ha("calibration_inputs", cfg=cfg)
        if "error" in inputs:
            raise HomeAssistantError(inputs["error"])
        self.running, self.error = True, None
        self._notify_listeners()
        try:
            start = (dt_util.utcnow() - timedelta(days=C["calibration_history_days"])).replace(
                minute=0, second=0, microsecond=0
            )
            request = {
                "cfg": cfg,
                "lat": self.hass.config.latitude,
                "lon": self.hass.config.longitude,
                "strings": await self._statistics(inputs["power_ids"], start, {"power": "W"}, "mean"),
                "total": await self._statistics(inputs["energy_ids"], start, {"energy": "kWh"}, "change"),
            }
            try:
                raw = await self.hass.async_add_executor_job(vulpes_ems.calibrate_ha, dumps(request))
            except ValueError as err:
                raise HomeAssistantError(str(err)) from err
            out = json.loads(raw)
            result = out["result"]
            self._stored.update(result=result, based_on=out["based_on"], last_run=dt_util.utcnow().isoformat())
            await self._store.async_save(self._stored)
            persistent_notification.async_create(
                self.hass,
                ha("calibration_message", cfg=cfg, result=result),
                title="Vulpes EMS: solar calibration",
                notification_id=NOTIFICATION_ID,
            )
            return result
        except HomeAssistantError as err:
            self.error = str(err)
            raise
        finally:
            self.running = False
            self._notify_listeners()

    @callback
    def async_dismiss(self) -> None:
        persistent_notification.async_dismiss(self.hass, NOTIFICATION_ID)
