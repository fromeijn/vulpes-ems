"""Coordinator: gathers Home Assistant data every 15 minutes, plans in Rust and,
when the Active switch is on, runs the user's control script."""

from __future__ import annotations

import asyncio
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

from .calibration import SolarCalibration
from .const import CONF_CONTROL_SCRIPT, CONF_MODE_ENTITY, CONF_PRICE_ENTITY, CONF_SOC_ENTITY, DOMAIN
from .core import C, dumps, ha, stat_rows

_LOGGER = logging.getLogger(__name__)
# The control script runs to completion before the next step; errors in it
# show up on the Battery control sensor.
SCRIPT_TIMEOUT_S = 60


async def async_energy_changes(
    hass: HomeAssistant, ids: set[str], start: datetime
) -> dict[str, list[dict[str, Any]]]:
    """Hourly energy changes (kWh) per statistic since `start`, in the recorder's executor."""
    return await get_instance(hass).async_add_executor_job(
        statistics_during_period, hass, start, None, ids, "hour", {"energy": "kWh"}, {"change"}
    )


def history_start(now: datetime, days: int) -> datetime:
    """The start of the hour `days` before `now`."""
    return (now - timedelta(days=days)).replace(minute=0, second=0, microsecond=0)


class VulpesCoordinator(DataUpdateCoordinator[dict[str, Any]]):
    """Plans every 15 minutes; tracks agreement with the current mode."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry) -> None:
        super().__init__(hass, _LOGGER, config_entry=entry, name=DOMAIN, update_interval=None)
        self._unsub: Callable[[], None] | None = None
        self._wholesale: list[tuple[int, float]] = []
        self._wholesale_fetched: float | None = None
        self.agreement_day: str | None = None
        self.agree = 0
        self.compared = 0
        self.agrees_now: bool | None = None
        self.calibration = SolarCalibration(hass, entry)
        # Active control: set by the Active switch (restored across restarts).
        # The script is fixed for this setup: changing it reloads the entry.
        self.control_script: str | None = self.cfg.get(CONF_CONTROL_SCRIPT) or None
        self.active = False
        self.last_command: dict[str, Any] | None = None
        self.last_error: str | None = None

    @property
    def cfg(self) -> dict[str, Any]:
        return ha("settings", data=dict(self.config_entry.data), options=dict(self.config_entry.options))

    def start_schedule(self) -> None:
        self._unsub = async_track_time_change(self.hass, self._tick, minute=[0, 15, 30, 45], second=20)

    def stop_schedule(self) -> None:
        if self._unsub:
            self._unsub()
            self._unsub = None

    @callback
    def _tick(self, _now: datetime) -> None:
        self.hass.async_create_task(self._async_refresh_and_apply())

    @property
    def _controlling(self) -> bool:
        return self.active and bool(self.control_script)

    async def _async_refresh_and_apply(self) -> None:
        await self.async_refresh()
        await self.async_apply()
        # While Vulpes sets the mode itself, compare after the script ran:
        # before it, the mode entity still shows the previous slot.
        if self._controlling and self.last_update_success and self.data:
            self._track_agreement(self.data)
            self.async_update_listeners()

    def _safe_command(self) -> dict[str, Any]:
        return ha("safe_command", cfg=self.cfg, now=dt_util.utcnow().timestamp(), fallback=False)

    async def async_release(self) -> None:
        """On unload (settings changed, entry removed or disabled): hand control back.

        Without this, the inverter would stay in the last mode Vulpes sent. A
        reload with the same script restores the Active switch and applies the
        plan again right away.
        """
        self.stop_schedule()
        if self.active and self.control_script:
            await self._async_run_script(self._safe_command())

    async def async_set_active(self, active: bool) -> None:
        """Hand over control (apply the plan now) or take it back (send the safe command once)."""
        was_active, self.active = self.active, active
        if active:
            await self.async_apply()
        elif was_active and self.control_script:
            await self._async_run_script(self._safe_command())
        else:
            self.async_update_listeners()

    async def async_apply(self) -> None:
        """Run the control script for the current slot, or the safe command without a usable plan."""
        if not self.active or not self.control_script:
            return
        cmd = ha(
            "command",
            cfg=self.cfg,
            plan=self.data,
            now=dt_util.utcnow().timestamp(),
            failed=not self.last_update_success,
        )
        if cmd["fallback"]:
            _LOGGER.warning("No usable plan; sending the safe mode (%s) to %s", cmd["mode"], self.control_script)
        await self._async_run_script(cmd)

    async def _async_run_script(self, cmd: dict[str, Any]) -> None:
        script = self.control_script
        if script is None:
            return
        try:
            # Call the script's own service so errors in its actions reach us.
            async with asyncio.timeout(SCRIPT_TIMEOUT_S):
                await self.hass.services.async_call("script", script.split(".", 1)[1], cmd, blocking=True)
        except TimeoutError:
            _LOGGER.warning("Control script %s did not finish within %d s", script, SCRIPT_TIMEOUT_S)
            self.last_error = f"did not finish within {SCRIPT_TIMEOUT_S} s"
        except Exception as err:  # noqa: BLE001 - a failing script must not stop the schedule
            _LOGGER.warning("Control script %s failed: %s", script, err)
            self.last_error = str(err) or type(err).__name__
        else:
            self.last_error = None
        self.last_command = {**cmd, "sent": dt_util.utcnow().isoformat()}
        self.async_update_listeners()

    async def _wholesale_history(self, cfg: dict[str, Any], now: datetime) -> list[tuple[int, float]]:
        fetch = ha("wholesale_fetch", cfg=cfg, now=now.timestamp(), fetched_at=self._wholesale_fetched)
        if not fetch["fetch"]:
            return self._wholesale
        try:
            self._wholesale = await self.hass.async_add_executor_job(
                vulpes_ems.energyzero_hourly, fetch["start"], fetch["end"]
            )
            self._wholesale_fetched = now.timestamp()
        except ValueError as err:
            _LOGGER.warning("Wholesale price history unavailable, keeping cached data: %s", err)
        return self._wholesale

    async def _async_update_data(self) -> dict[str, Any]:
        cfg = self.cfg
        now = dt_util.utcnow()
        soc = self.hass.states.get(cfg.get(CONF_SOC_ENTITY, ""))
        price = self.hass.states.get(cfg.get(CONF_PRICE_ENTITY, ""))
        ids = ha("energy_ids", cfg=cfg)
        stats = (
            await async_energy_changes(self.hass, set(ids), history_start(now, C["load_history_days"]))
            if ids
            else {}
        )
        request = {
            "cfg": cfg,
            "now": now.timestamp(),
            "lat": self.hass.config.latitude,
            "lon": self.hass.config.longitude,
            "timezone": str(self.hass.config.time_zone),
            "country": self.hass.config.country or "",
            "soc_state": soc.state if soc else None,
            "price_attributes": list(price.attributes.items()) if price else None,
            "price_unit": price.attributes.get("unit_of_measurement") if price else None,
            "stats": stat_rows(stats, ids, "change"),
            "wholesale": await self._wholesale_history(cfg, now),
        }
        try:
            raw = await self.hass.async_add_executor_job(vulpes_ems.plan_ha, dumps(request))
        except ValueError as err:
            raise UpdateFailed(str(err)) from err
        plan = json.loads(raw)
        if not self._controlling:
            self._track_agreement(plan)
        return plan

    def _track_agreement(self, plan: dict[str, Any]) -> None:
        """Compare the plan with the user's own mode entity, if one is mapped."""
        cfg = self.cfg
        entity_id = cfg.get(CONF_MODE_ENTITY)
        state = self.hass.states.get(entity_id) if entity_id else None
        today = dt_util.now().date().isoformat()
        if today != self.agreement_day:
            self.agreement_day, self.agree, self.compared = today, 0, 0
        self.agrees_now = ha(
            "agrees", cfg=cfg, state=state.state if state else None, mode_now=plan["summary"]["mode_now"]
        )
        if self.agrees_now is not None:
            self.compared += 1
            self.agree += int(self.agrees_now)
