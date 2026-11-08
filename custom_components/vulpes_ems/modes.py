"""Vulpes's own battery modes, and how they relate to a user's existing setup.

Vulpes defines a fixed set of modes. The user picks the ones their system can
do and sets their power. An existing mode entity (for example an
`input_select`) is optional: its states can be mapped to Vulpes modes to
compare the plan with what the house actually does.

This module has no Home Assistant imports so it can be tested on its own.
"""

from __future__ import annotations

from typing import Any

CONF_POWER_CONTROL = "power_control"
CONF_CHARGE_W = "charge_w"
CONF_DISCHARGE_W = "discharge_w"
CONF_PV_CURTAIL = "pv_curtail"
CONF_EXPORT_BLOCK = "export_block"
CONF_CONTROL_SCRIPT = "control_script"
CONF_MODE_ENTITY = "mode_entity"
CONF_MODE_MAP = "mode_map"
CONF_MODES_V1 = "modes"

POWER_CONTROLS = ["grid_setpoint", "battery_power"]

# Mode key -> enabled by default for new setups.
MODES: dict[str, bool] = {
    "charge": True,  # charge from the grid at a set power
    "hold": True,  # charge from solar surplus only, never discharge
    "self_consume": True,  # keep the grid at zero
    "idle": False,  # battery does nothing
    "discharge": False,  # discharge to the grid at a set power
}
POWERED = {"charge": CONF_CHARGE_W, "discharge": CONF_DISCHARGE_W}

# Suffix for the variant of a mode with solar switched off.
PV_OFF = ":pv_off"
# Suffix for the variant of a mode with export blocked (zero export).
NO_EXPORT = ":no_export"
# Modes that get a zero-export variant: in charge and discharge, blocking
# export means nothing or contradicts the mode.
EXPORT_BLOCK_MODES = ("hold", "self_consume", "idle")


def settings(data: dict[str, Any], options: dict[str, Any]) -> dict[str, Any]:
    """Current settings: the options flow saves a complete copy, which replaces the setup data."""
    return dict(options) if options else dict(data)


def enabled_modes(cfg: dict[str, Any]) -> list[str]:
    return [m for m, default in MODES.items() if cfg.get(m, default)]


def engine_modes(cfg: dict[str, Any]) -> list[dict[str, Any]]:
    """Mode definitions for the planning engine."""
    control = cfg.get(CONF_POWER_CONTROL, "grid_setpoint")
    out = []
    for key in enabled_modes(cfg):
        if key in POWERED:
            sign = 1.0 if key == "charge" else -1.0
            behaviour: dict[str, Any] = {"type": control, "w": sign * abs(float(cfg[POWERED[key]]))}
        elif key == "hold":
            behaviour = {"type": "no_discharge"}
        else:
            behaviour = {"type": key}
        out.append({"name": key, "behaviour": behaviour, "pv_curtail": False, "no_export": False})
        if cfg.get(CONF_PV_CURTAIL):
            out.append({"name": key + PV_OFF, "behaviour": behaviour, "pv_curtail": True, "no_export": False})
        if cfg.get(CONF_EXPORT_BLOCK) and key in EXPORT_BLOCK_MODES:
            out.append({"name": key + NO_EXPORT, "behaviour": behaviour, "pv_curtail": False, "no_export": True})
    return out


def split_mode(name: str) -> tuple[str, bool, bool]:
    """Engine mode name -> (Vulpes mode, solar off, export blocked)."""
    if name.endswith(PV_OFF):
        return name[: -len(PV_OFF)], True, False
    if name.endswith(NO_EXPORT):
        return name[: -len(NO_EXPORT)], False, True
    return name, False, False


def mode_of_state(cfg: dict[str, Any], state: str) -> str | None:
    """The Vulpes mode a state of the user's mode entity corresponds to."""
    for key, option in cfg.get(CONF_MODE_MAP, {}).items():
        if option == state:
            return key
    return None


def migrate_v1(cfg: dict[str, Any]) -> dict[str, Any]:
    """Turn 0.1.0 settings (user-named modes with behaviours) into Vulpes modes.

    Each old mode is matched to a Vulpes mode by its behaviour; its name
    becomes the comparison mapping, so the shadow comparison keeps working.
    """
    if CONF_MODES_V1 not in cfg:
        return cfg
    new = {k: v for k, v in cfg.items() if k != CONF_MODES_V1}
    new.update({m: False for m in MODES})
    mapping: dict[str, str] = {}
    for mode in cfg[CONF_MODES_V1]:
        b = mode.get("behaviour", {})
        kind, w = b.get("type"), float(b.get("w", 0.0))
        if kind in POWER_CONTROLS:
            key = "charge" if w >= 0 else "discharge"
            new[CONF_POWER_CONTROL] = kind
            new[POWERED[key]] = abs(w)
        elif kind == "no_discharge":
            key = "hold"
        elif kind in ("self_consume", "idle"):
            key = kind
        else:
            continue
        if key in mapping:
            continue  # two old modes with the same behaviour: keep the first
        new[key] = True
        mapping[key] = mode["name"]
        if mode.get("pv_curtail"):
            new[CONF_PV_CURTAIL] = True
    new[CONF_MODE_MAP] = mapping
    new.setdefault(CONF_CHARGE_W, 3000.0)
    new.setdefault(CONF_DISCHARGE_W, 3000.0)
    return new
