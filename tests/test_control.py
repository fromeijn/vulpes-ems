"""Tests for the active-control commands (no Home Assistant needed)."""

from datetime import datetime, timezone
import importlib
from pathlib import Path
import sys
import types

import pytest

# control imports modes.py relatively; load both as submodules of a bare
# package so the integration's __init__ (which needs Home Assistant) never runs.
_pkg = types.ModuleType("vulpes_pkg")
_pkg.__path__ = [str(Path(__file__).parents[1] / "custom_components" / "vulpes_ems")]
sys.modules.setdefault("vulpes_pkg", _pkg)
control = importlib.import_module("vulpes_pkg.control")

T0 = int(datetime(2026, 11, 8, 12, 0, tzinfo=timezone.utc).timestamp())
CFG = {
    "power_control": "grid_setpoint",
    "charge_w": 3000,
    "discharge_w": -2500,
    "min_soc": 11,
    "max_soc": 100,
}


def _slot(i: int, mode: str, pv_off: bool = False, no_export: bool = False) -> dict:
    return {
        "start": T0 + 900 * i,
        "mode": mode,
        "pv_off": pv_off,
        "no_export": no_export,
        "grid_w": 2999.6,
        "battery_w": -1200.4,
    }


def _plan(*slots: dict, planned_at: int = T0) -> dict:
    return {"planned_at": planned_at, "slots": list(slots)}


def test_follows_the_slot_that_contains_now():
    plan = _plan(_slot(0, "hold"), _slot(1, "charge"), _slot(2, "discharge"))
    cmd = control.command(CFG, plan, T0 + 900 + 20, failed=False)
    assert cmd == {
        "mode": "charge",
        "pv_off": False,
        "no_export": False,
        "power_control": "grid_setpoint",
        "power_w": 3000,
        "soc_min": 11.0,
        "soc_max": 100.0,
        "planned_grid_w": 3000,
        "planned_battery_w": -1200,
        "fallback": False,
        "until": "2026-11-08T12:30:00+00:00",
    }


@pytest.mark.parametrize(
    ("mode", "power_w"),
    [("charge", 3000), ("discharge", 2500), ("hold", 0), ("self_consume", 0), ("idle", 0)],
)
def test_power_is_unsigned_and_only_for_powered_modes(mode, power_w):
    cmd = control.command(CFG, _plan(_slot(0, mode)), T0, failed=False)
    assert (cmd["mode"], cmd["power_w"]) == (mode, power_w)


def test_battery_power_control_is_passed_on():
    cfg = {**CFG, "power_control": "battery_power"}
    assert control.command(cfg, _plan(_slot(0, "charge")), T0, failed=False)["power_control"] == "battery_power"


def test_pv_off_and_no_export_flags():
    plan = _plan(_slot(0, "self_consume", no_export=True), _slot(1, "idle", pv_off=True))
    first = control.command(CFG, plan, T0, failed=False)
    second = control.command(CFG, plan, T0 + 900, failed=False)
    assert (first["mode"], first["pv_off"], first["no_export"]) == ("self_consume", False, True)
    assert (second["mode"], second["pv_off"], second["no_export"]) == ("idle", True, False)


def test_engine_mode_names_are_split_when_slots_were_not():
    slot = {"start": T0, "mode": "hold:no_export", "grid_w": 0.0, "battery_w": 0.0}
    cmd = control.command(CFG, _plan(slot), T0, failed=False)
    assert (cmd["mode"], cmd["pv_off"], cmd["no_export"]) == ("hold", False, True)


def _is_safe(cmd: dict) -> bool:
    return (
        cmd["mode"] == "self_consume"
        and not cmd["pv_off"]
        and not cmd["no_export"]
        and cmd["fallback"]
        and cmd["planned_grid_w"] is None
    )


def test_fallback_when_planning_failed():
    cmd = control.command(CFG, _plan(_slot(0, "charge")), T0, failed=True)
    assert _is_safe(cmd)
    assert cmd["until"] == "2026-11-08T12:15:00+00:00"


def test_fallback_without_a_plan_or_slots():
    assert _is_safe(control.command(CFG, None, T0, failed=False))
    assert _is_safe(control.command(CFG, _plan(), T0, failed=False))
    assert _is_safe(control.command(CFG, {"slots": [_slot(0, "charge")]}, T0, failed=False))  # no planned_at


def test_fallback_when_now_is_outside_the_plan():
    plan = _plan(_slot(0, "charge"), planned_at=T0)
    assert _is_safe(control.command(CFG, plan, T0 + 900, failed=False))
    assert _is_safe(control.command(CFG, plan, T0 - 1, failed=False))


def test_stale_plan_boundary():
    slots = [_slot(i, "charge") for i in range(4)]
    plan = _plan(*slots, planned_at=T0)
    fresh = control.command(CFG, plan, T0 + control.STALE_AFTER_S, failed=False)
    assert not fresh["fallback"] and fresh["mode"] == "charge"
    assert _is_safe(control.command(CFG, plan, T0 + control.STALE_AFTER_S + 1, failed=False))


def test_safe_command_when_switched_off_is_not_a_fallback():
    cmd = control.safe_command(CFG, T0 + 1, fallback=False)
    assert (cmd["mode"], cmd["fallback"], cmd["until"]) == ("self_consume", False, "2026-11-08T12:15:00+00:00")


def test_soc_limits_default_when_missing():
    cmd = control.safe_command({}, T0)
    assert (cmd["soc_min"], cmd["soc_max"], cmd["power_control"]) == (0.0, 100.0, "grid_setpoint")


def test_state_of():
    assert control.state_of(False, {"fallback": False}) == control.OFF
    assert control.state_of(True, None) == control.FALLBACK
    assert control.state_of(True, {"fallback": True}) == control.FALLBACK
    assert control.state_of(True, {"fallback": False}) == control.FOLLOWING
