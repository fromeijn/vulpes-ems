"""Active control: the variables Vulpes passes to the user's control script.

Every 15 minutes, while the Active switch is on, Vulpes runs one script that
the user picked. The script turns the variables into inverter actions. When
there is no usable plan, Vulpes sends the safe command instead: self-consume,
with solar on and export allowed.

This module has no Home Assistant imports so it can be tested on its own.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import Any

from .modes import CONF_POWER_CONTROL, POWERED, split_mode

CONF_MIN_SOC = "min_soc"
CONF_MAX_SOC = "max_soc"

SLOT_S = 900
# A plan older than this is not followed: the safe command is sent instead.
STALE_AFTER_S = 1800
SAFE_MODE = "self_consume"

# States of the control status sensor.
OFF = "off"
FOLLOWING = "following_plan"
FALLBACK = "fallback"
STATES = [OFF, FOLLOWING, FALLBACK]


def _iso(ts: float) -> str:
    return datetime.fromtimestamp(ts, UTC).isoformat()


def _variables(cfg: dict[str, Any], mode: str, pv_off: bool, no_export: bool) -> dict[str, Any]:
    power = POWERED.get(mode)
    return {
        "mode": mode,
        "pv_off": pv_off,
        "no_export": no_export,
        "power_control": cfg.get(CONF_POWER_CONTROL, "grid_setpoint"),
        "power_w": round(abs(float(cfg[power]))) if power and cfg.get(power) is not None else 0,
        "soc_min": float(cfg.get(CONF_MIN_SOC, 0)),
        "soc_max": float(cfg.get(CONF_MAX_SOC, 100)),
    }


def safe_command(cfg: dict[str, Any], now: float, *, fallback: bool = True) -> dict[str, Any]:
    """Self-consume with solar on and export allowed, until the next quarter hour."""
    return {
        **_variables(cfg, SAFE_MODE, False, False),
        "planned_grid_w": None,
        "planned_battery_w": None,
        "fallback": fallback,
        "until": _iso(now - now % SLOT_S + SLOT_S),
    }


def current_slot(plan: dict[str, Any], now: float) -> dict[str, Any] | None:
    """The plan slot that contains `now`, if any."""
    for s in plan.get("slots", []):
        if s["start"] <= now < s["start"] + SLOT_S:
            return s
    return None


def command(cfg: dict[str, Any], plan: dict[str, Any] | None, now: float, *, failed: bool) -> dict[str, Any]:
    """Script variables for `now`: the planned slot, or the safe command.

    The safe command is used when the last planning run failed, when the plan
    is older than `STALE_AFTER_S`, or when it has no slot for `now`. Slots
    carry the Vulpes mode, `pv_off` and `no_export` (see `coordinator`).
    """
    if failed or not plan:
        return safe_command(cfg, now)
    planned_at = plan.get("planned_at")
    if planned_at is None or now - planned_at > STALE_AFTER_S:
        return safe_command(cfg, now)
    slot = current_slot(plan, now)
    if slot is None:
        return safe_command(cfg, now)
    if "pv_off" in slot:
        mode, pv_off, no_export = slot["mode"], slot["pv_off"], slot.get("no_export", False)
    else:  # an engine slot that was not split yet
        mode, pv_off, no_export = split_mode(slot["mode"])
    return {
        **_variables(cfg, mode, bool(pv_off), bool(no_export)),
        "planned_grid_w": round(slot["grid_w"]),
        "planned_battery_w": round(slot["battery_w"]),
        "fallback": False,
        "until": _iso(slot["start"] + SLOT_S),
    }


def state_of(active: bool, last: dict[str, Any] | None) -> str:
    """The control status: off, following the plan, or in fallback."""
    if not active:
        return OFF
    return FALLBACK if last is None or last.get("fallback") else FOLLOWING
