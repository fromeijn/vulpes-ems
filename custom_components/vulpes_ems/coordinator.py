"""Coordinator: gathers Home Assistant data every 15 minutes, plans in Rust and,
when the Active switch is on, runs the user's control script."""

from __future__ import annotations

import asyncio
from collections.abc import Callable
from datetime import datetime
import json
import logging
from typing import Any

import vulpes_ems

from homeassistant.components.recorder import get_instance
from homeassistant.components.recorder.statistics import statistics_during_period
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.event import async_track_time_change
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from homeassistant.util import dt as dt_util

from .calibration import SolarCalibration
from .const import CONF_CONTROL_SCRIPT, CONF_MODE_ENTITY, CONF_PRICE_ENTITY, CONF_SOC_ENTITY, DOMAIN
from .core import C, dumps, ha, stat_rows

_LOGGER = logging.getLogger(__name__)
# The control script runs to completion before the next step; errors in it
# show up on the Battery control sensor.
SCRIPT_TIMEOUT_S = C["script_timeout_s"]


async def async_energy_changes(
    hass: HomeAssistant, ids: set[str], start: datetime
) -> dict[str, list[dict[str, Any]]]:
    """Hourly energy changes (kWh) per statistic since `start`, in the recorder's executor."""
    return await get_instance(hass).async_add_executor_job(
        statistics_during_period, hass, start, None, ids, "hour", {"energy": "kWh"}, {"change"}
    )


def stats_start(now: datetime) -> datetime:
    """Where the energy statistics the plan reads start (decided in Rust)."""
    return dt_util.utc_from_timestamp(ha("stats_start", now=now.timestamp()))


class VulpesCoordinator(DataUpdateCoordinator[dict[str, Any]]):
    """Plans every 15 minutes; tracks agreement with the current mode."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry) -> None:
        super().__init__(hass, _LOGGER, config_entry=entry, name=DOMAIN, update_interval=None)
        self._unsub: Callable[[], None] | None = None
        self._wholesale: list[list[float]] = []
        self._wholesale_fetched: float | None = None
        # Wind and solar at the price reference points: day-ahead history and forecast.
        self._drivers: dict[str, list | None] = {"past": None, "ahead": None}
        self._drivers_fetched: dict[str, float | None] = {"past": None, "ahead": None}
        self._stats: dict[str, list] = {}
        # Agreement with the user's mode entity today, counted from the history file.
        self.agree = 0
        self.compared = 0
        self.agreement_pct: float | None = None
        self.agrees_now: bool | None = None
        # The history file (same format as the CLI's store); see vulpes_ha::history.
        self.history_path = hass.config.path(DOMAIN, entry.entry_id, "vulpes.db")
        self._history_lock = asyncio.Lock()
        self.history_error: str | None = None
        self.newest_energy_ts: int | None = None
        self.energy_until: int | None = None
        # The review of the history file: savings, forecast quality, control.
        self.review: dict[str, Any] | None = None
        self.review_error: str | None = None
        # Health: plain facts for the Rust checks, and the result.
        self.started_at = dt_util.utcnow().timestamp()
        self.last_success_ts: float | None = None
        self.script_failures = 0
        self.health: dict[str, Any] = {"status": "ok", "checks": [], "problems": [], "issues": []}
        # Every issue counts as open at first, so the first check closes any that an
        # earlier run of this entry left behind (closing a missing issue does nothing).
        self._open_issues: set[str] = set(ha("issue_ids"))
        # Set on unload: a tick still running then stops before its next step.
        self._closed = False
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
        if self._closed:
            return
        await self.async_apply()
        # Recorded after the script ran: the mode entity then shows this slot's mode.
        await self.async_record()
        await self.async_review()
        self.update_health()

    @callback
    def update_health(self) -> None:
        """Run the health checks on what Home Assistant shows now; open or close repair issues."""
        if self._closed:
            return
        cfg = self.cfg
        soc = self.hass.states.get(cfg.get(CONF_SOC_ENTITY, ""))
        price = self.hass.states.get(cfg.get(CONF_PRICE_ENTITY, ""))
        snapshot = {
            "cfg": cfg,
            "now": dt_util.utcnow().timestamp(),
            "timezone": str(self.hass.config.time_zone),
            "started_at": self.started_at,
            "refresh_ok": self.last_update_success,
            "last_success": self.last_success_ts,
            "refresh_error": str(self.last_exception) if self.last_exception else None,
            "plan": self.data,
            "soc_state": soc.state if soc else None,
            "soc_last_changed": soc.last_changed.timestamp() if soc else None,
            "soc_last_reported": soc.last_reported.timestamp() if soc else None,
            "price_attributes": list(price.attributes.items()) if price else None,
            "price_unit": price.attributes.get("unit_of_measurement") if price else None,
            "wholesale_fetched_at": self._wholesale_fetched,
            "price_drivers_fetched_at": self._drivers_fetched["ahead"],
            "newest_energy_ts": self.newest_energy_ts,
            "active": self.active,
            "control_script": self.control_script,
            "last_command": self.last_command,
            "last_error": self.last_error,
            "script_failures": self.script_failures,
            "history_error": self.history_error,
            "review_error": self.review_error,
            "review_checks": (self.review or {}).get("checks", []),
        }
        self.health = ha("health", **snapshot)
        entry_id = self.config_entry.entry_id
        wanted = set(self.health["issues"])
        for issue in wanted - self._open_issues:
            ir.async_create_issue(
                self.hass,
                DOMAIN,
                f"{issue}_{entry_id}",
                is_fixable=False,
                severity=ir.IssueSeverity.WARNING,
                translation_key=issue,
            )
        for issue in self._open_issues - wanted:
            ir.async_delete_issue(self.hass, DOMAIN, f"{issue}_{entry_id}")
        self._open_issues = wanted
        self.async_update_listeners()

    @callback
    def clear_issues(self) -> None:
        """Close this entry's repair issues (on unload)."""
        for issue in self._open_issues:
            ir.async_delete_issue(self.hass, DOMAIN, f"{issue}_{self.config_entry.entry_id}")
        self._open_issues = set()

    async def async_review(self) -> None:
        """Review the history file (savings, forecasts, control); never blocks planning."""
        request = {"cfg": self.cfg, "now": dt_util.utcnow().timestamp(), "timezone": str(self.hass.config.time_zone)}
        async with self._history_lock:
            if self._closed:
                return
            try:
                raw = await self.hass.async_add_executor_job(vulpes_ems.review_ha, self.history_path, dumps(request))
            except ValueError as err:
                if self.review_error != str(err):
                    _LOGGER.warning("Could not review the history: %s", err)
                self.review_error = str(err)
                return
        self.review = json.loads(raw)
        self.review_error = None

    async def async_record(self) -> None:
        """Record this tick in the history file; agreement comes back from it."""
        cfg = self.cfg
        soc = self.hass.states.get(cfg.get(CONF_SOC_ENTITY, ""))
        entity = cfg.get(CONF_MODE_ENTITY)
        mode = self.hass.states.get(entity) if entity else None
        request = {
            "cfg": cfg,
            "now": dt_util.utcnow().timestamp(),
            "timezone": str(self.hass.config.time_zone),
            "plan": self.data if self.last_update_success else None,
            "soc_state": soc.state if soc else None,
            "mode_state": mode.state if mode else None,
            "active": self._controlling,
            "last_command": self.last_command,
            "last_error": self.last_error,
            "stats": self._stats,
            "wholesale": self._wholesale,
        }
        async with self._history_lock:
            if self._closed:
                return
            try:
                raw = await self.hass.async_add_executor_job(vulpes_ems.record_ha, self.history_path, dumps(request))
            except ValueError as err:
                if self.history_error != str(err):
                    _LOGGER.warning("Could not record the history: %s", err)
                self.history_error = str(err)
                return
        out = json.loads(raw)
        self.history_error = None
        self.compared = out["agreement_today"]["compared"]
        self.agree = out["agreement_today"]["agreed"]
        self.agreement_pct = out["agreement_today"]["pct"]
        self.agrees_now = out["agrees_now"]
        self.newest_energy_ts = out["newest_energy_ts"]
        self.energy_until = out["energy_until"]

    def _safe_command(self) -> dict[str, Any]:
        return ha("safe_command", cfg=self.cfg, now=dt_util.utcnow().timestamp(), fallback=False)

    async def async_release(self) -> None:
        """On unload (settings changed, entry removed or disabled): hand control back.

        Without this, the inverter would stay in the last mode Vulpes sent. A
        reload with the same script restores the Active switch and applies the
        plan again right away.

        A tick that is still planning stops before its next step, so it can't
        send the plan after the safe command, write the history file after the
        entry is removed, or reopen repair issues.
        """
        self._closed = True
        self.stop_schedule()
        # Let a history write that is already running finish before the entry's folder goes.
        async with self._history_lock:
            pass
        self.clear_issues()
        if self.active and self.control_script:
            await self._async_run_script(self._safe_command())

    async def async_set_active(self, active: bool) -> None:
        """Hand over control (apply the plan now) or take it back (send the safe command once)."""
        if self._closed:
            return
        was_active, self.active = self.active, active
        if active:
            await self.async_apply()
            await self.async_record()
            self.update_health()
        else:
            if was_active and self.control_script:
                await self._async_run_script(self._safe_command())
            self.update_health()  # Status and repairs follow at once; notifies the listeners

    async def async_apply(self) -> None:
        """Run the control script for the current slot, or the safe command without a usable plan."""
        if self._closed or not self.active or not self.control_script:
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
        error: str | None = None
        timed_out = False
        try:
            # Call the script's own service so errors in its actions reach us.
            async with asyncio.timeout(SCRIPT_TIMEOUT_S):
                await self.hass.services.async_call("script", script.split(".", 1)[1], cmd, blocking=True)
        except TimeoutError:
            _LOGGER.warning("Control script %s did not finish within %d s", script, SCRIPT_TIMEOUT_S)
            timed_out = True
        except Exception as err:  # noqa: BLE001 - a failing script must not stop the schedule
            _LOGGER.warning("Control script %s failed: %s", script, err)
            error = str(err) or type(err).__name__
        result = ha(
            "script_result",
            cmd=cmd,
            error=error,
            timed_out=timed_out,
            failures=self.script_failures,
            now=dt_util.utcnow().timestamp(),
        )
        self.last_command = result["last_command"]
        self.last_error = result["last_error"]
        self.script_failures = result["script_failures"]
        self.async_update_listeners()

    async def _wholesale_history(self, cfg: dict[str, Any], now: datetime) -> list[list[float]]:
        cached = self._wholesale
        fetch = ha(
            "wholesale_fetch",
            cfg=cfg,
            now=now.timestamp(),
            fetched_at=self._wholesale_fetched,
            cached_from=cached[0][0] if cached else None,
            cached_until=cached[-1][0] if cached else None,
        )
        if not fetch["fetch"]:
            return self._wholesale

        def fetch_and_merge() -> list[list[float]]:
            fetched = vulpes_ems.energyzero_hourly(fetch["start"], fetch["end"])
            return ha("wholesale_merge", cached=cached, fetched=fetched, now=now.timestamp())

        try:
            self._wholesale = await self.hass.async_add_executor_job(fetch_and_merge)
            self._wholesale_fetched = now.timestamp()
        except ValueError as err:
            _LOGGER.warning("Wholesale price history unavailable, keeping cached data: %s", err)
        return self._wholesale

    async def _price_drivers(self, cfg: dict[str, Any], now: datetime) -> dict[str, list | None]:
        fetch = ha(
            "price_drivers_fetch",
            cfg=cfg,
            now=now.timestamp(),
            past_fetched_at=self._drivers_fetched["past"],
            ahead_fetched_at=self._drivers_fetched["ahead"],
        )
        if not fetch["fetch"]:
            return self._drivers
        try:
            raw = await self.hass.async_add_executor_job(vulpes_ems.price_drivers, dumps(fetch["request"]))
        except ValueError as err:
            _LOGGER.warning("Wind and solar for the price forecast unavailable, keeping cached data: %s", err)
            return self._drivers
        resp = json.loads(raw)
        for err in resp.get("errors", []):
            _LOGGER.warning("Wind and solar for the price forecast unavailable, keeping cached data: %s", err)
        for part in ("past", "ahead"):
            if resp.get(part) is not None:
                self._drivers[part] = resp[part]
                self._drivers_fetched[part] = now.timestamp()
        return self._drivers

    async def _async_update_data(self) -> dict[str, Any]:
        cfg = self.cfg
        now = dt_util.utcnow()
        soc = self.hass.states.get(cfg.get(CONF_SOC_ENTITY, ""))
        price = self.hass.states.get(cfg.get(CONF_PRICE_ENTITY, ""))
        ids = ha("energy_ids", cfg=cfg)
        stats = (
            await async_energy_changes(self.hass, set(ids), stats_start(now))
            if ids
            else {}
        )
        self._stats = stat_rows(stats, ids, "change")
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
            "stats": self._stats,
            "wholesale": await self._wholesale_history(cfg, now),
            "price_drivers": await self._price_drivers(cfg, now),
        }
        try:
            raw = await self.hass.async_add_executor_job(vulpes_ems.plan_ha, dumps(request))
        except ValueError as err:
            raise UpdateFailed(str(err)) from err
        self.last_success_ts = now.timestamp()
        return json.loads(raw)
