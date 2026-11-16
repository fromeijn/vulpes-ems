"""Diagnostics download: settings (redacted), the plan's inputs and warnings,
control and calibration state, and the recent history file contents."""

from __future__ import annotations

import json
from typing import Any

import vulpes_ems

from homeassistant.core import HomeAssistant
from homeassistant.util import dt as dt_util

from . import VulpesConfigEntry
from .core import dumps, ha


async def async_get_config_entry_diagnostics(hass: HomeAssistant, entry: VulpesConfigEntry) -> dict[str, Any]:
    c = entry.runtime_data
    plan = c.data or {}
    try:
        history = json.loads(
            await hass.async_add_executor_job(
                vulpes_ems.export_history, c.history_path, dumps({"now": dt_util.utcnow().timestamp()})
            )
        )
    except ValueError as err:
        history = {"error": str(err)}
    return {
        "version": vulpes_ems.__version__,
        "config_entry_version": entry.version,
        "settings": ha("redact_settings", cfg=c.cfg),
        "last_update_success": c.last_update_success,
        "plan": {
            "planned_at": plan.get("planned_at"),
            "summary": plan.get("summary"),
            "inputs": plan.get("inputs"),
            "warnings": plan.get("warnings"),
        },
        "control": {"active": c.active, "last_command": c.last_command, "last_error": c.last_error},
        "calibration": {"status": c.calibration.status, "last_run": c.calibration.last_run, "error": c.calibration.error},
        "agreement_today": {"compared": c.compared, "agreed": c.agree},
        "history_error": c.history_error,
        "history": history,
    }
