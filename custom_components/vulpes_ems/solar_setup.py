"""Solar arrays: the simple setup form, and solar calibration requests and results.

A user describes each array (direction, tilt, kWp, optionally the power sensor
of the string that feeds it) and the inverter's AC limit. That becomes a list
of engine planes on one shared inverter. After setup, Vulpes calibrates the
planes from recorder history and proposes corrections; the user applies them.

This module has no Home Assistant imports so it can be tested on its own.
"""

from __future__ import annotations

from datetime import datetime
from typing import Any

CONF_SOLAR_SETUP = "solar_setup"  # "form" or "json"; absent for setups before 0.3.0
CONF_ARRAY_COUNT = "array_count"
CONF_INVERTER_W = "inverter_ac_w"
CONF_DIRECTION = "direction"
CONF_TILT = "tilt"
CONF_KWP = "kwp"
CONF_POWER_ENTITY = "power_entity"

MAX_ARRAYS = 4
# The inverter id the form gives every array: one shared inverter.
FORM_INVERTER = 0

DIRECTIONS: dict[str, float] = {
    "n": 0.0,
    "ne": 45.0,
    "e": 90.0,
    "se": 135.0,
    "s": 180.0,
    "sw": 225.0,
    "w": 270.0,
    "nw": 315.0,
}

# Plane keys that only the integration uses; the engine ignores them, but they
# are stripped before sending to keep requests clean.
LOCAL_KEYS = (CONF_POWER_ENTITY,)

DEFAULT_ARRAY = {CONF_DIRECTION: "s", CONF_TILT: 35.0, CONF_KWP: 4.0}
DEFAULT_INVERTER_W = 4000.0


def azimuth_of(direction: Any) -> float:
    """Compass key (`"se"`) or degrees clockwise from north (`"200"`, 200) -> azimuth."""
    key = str(direction).strip().lower()
    if key in DIRECTIONS:
        return DIRECTIONS[key]
    az = float(key.rstrip("°"))
    if not 0.0 <= az <= 360.0:
        raise ValueError(f"azimuth {az} out of range 0..360")
    return az % 360.0


def direction_of(azimuth: float) -> str:
    """The compass key for an exact compass azimuth, else the degrees as text."""
    az = float(azimuth) % 360.0
    for key, value in DIRECTIONS.items():
        if abs(value - az) < 1e-9:
            return key
    return f"{az:g}"


def plane_from_array(array: dict[str, Any], inverter_w: float, existing: dict[str, Any] | None = None) -> dict[str, Any]:
    """One form array -> an engine plane on the shared inverter.

    Keys of `existing` that the form doesn't show (loss parameters, for
    example) are kept, so editing through the form loses nothing.
    """
    plane = dict(existing or {})
    plane.update(
        azimuth=azimuth_of(array[CONF_DIRECTION]),
        tilt=float(array[CONF_TILT]),
        pdc0=float(array[CONF_KWP]) * 1000.0,
        ac_max=float(inverter_w),
        inverter=FORM_INVERTER,
    )
    if array.get(CONF_POWER_ENTITY):
        plane[CONF_POWER_ENTITY] = array[CONF_POWER_ENTITY]
    else:
        plane.pop(CONF_POWER_ENTITY, None)
    return plane


def array_from_plane(plane: dict[str, Any]) -> dict[str, Any]:
    """An engine plane -> the values to prefill a form array with."""
    out = {
        CONF_DIRECTION: direction_of(plane["azimuth"]),
        CONF_TILT: round(float(plane["tilt"]), 1),
        CONF_KWP: round(float(plane["pdc0"]) / 1000.0, 3),
    }
    if plane.get(CONF_POWER_ENTITY):
        out[CONF_POWER_ENTITY] = plane[CONF_POWER_ENTITY]
    return out


def fits_form(planes: list[dict[str, Any]]) -> bool:
    """Whether the planes can be edited with the form without changing behaviour:
    1 to MAX_ARRAYS planes, on one shared inverter (or a single plane)."""
    if not planes or len(planes) > MAX_ARRAYS:
        return False
    if len(planes) == 1:
        return True
    return all(p.get("inverter") == FORM_INVERTER for p in planes) and len({float(p["ac_max"]) for p in planes}) == 1


def form_defaults(planes: list[dict[str, Any]]) -> tuple[int, float, list[dict[str, Any]]]:
    """Array count, inverter limit and per-array values to prefill the form with."""
    if fits_form(planes):
        return len(planes), float(planes[0]["ac_max"]), [array_from_plane(p) for p in planes]
    return 1, DEFAULT_INVERTER_W, []


def engine_planes(planes: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Planes as the engine expects them, without integration-only keys."""
    return [{k: v for k, v in p.items() if k not in LOCAL_KEYS} for p in planes]


def hourly_series(rows: list[dict[str, Any]], field: str, scale: float = 1.0) -> list[tuple[int, float]]:
    """Recorder statistics rows -> `(hour_start, value × scale)`, skipping empty hours."""
    out = []
    for row in rows:
        start, value = row.get("start"), row.get(field)
        if isinstance(start, datetime):
            start = start.timestamp()
        if start is None or value is None:
            continue
        out.append((int(start), float(value) * scale))
    return sorted(out)


def total_power(energy: dict[str, list[tuple[int, float]]]) -> list[tuple[int, float]]:
    """Hourly energy per statistic (kWh) -> total mean power (W) for hours all statistics have."""
    if not energy:
        return []
    per_hour: dict[int, list[float]] = {}
    for series in energy.values():
        for t, kwh in series:
            per_hour.setdefault(t, []).append(kwh)
    n = len(energy)
    return [(t, max(sum(v), 0.0) * 1000.0) for t, v in sorted(per_hour.items()) if len(v) == n]


def calibration_request(
    lat: float,
    lon: float,
    planes: list[dict[str, Any]],
    strings: dict[str, list[tuple[int, float]]],
    total: list[tuple[int, float]],
) -> dict[str, Any]:
    """The engine's calibration request. `strings` maps power entity -> hourly mean W."""
    per_plane = [strings.get(p.get(CONF_POWER_ENTITY) or "", []) for p in planes]
    return {
        # Rounded to ~1 km in the core before any external request.
        "location": {"lat": lat, "lon": lon},
        "planes": engine_planes(planes),
        "strings": per_plane if any(per_plane) else [],
        "total": total,
    }


def improvement_pct(result: dict[str, Any]) -> float | None:
    """Relative reduction of the hourly RMSE by the proposal, in %."""
    cur, new = result.get("score_current"), result.get("score_proposed")
    if not cur or not new or not cur.get("rmse_w"):
        return None
    return round(100.0 * (1.0 - new["rmse_w"] / cur["rmse_w"]), 1)


def has_changes(result: dict[str, Any], planes: list[dict[str, Any]]) -> bool:
    """Whether the proposal differs noticeably from the given planes:
    more than 0.5° in orientation or 0.5 % in capacity."""
    proposed = result.get("planes", [])
    if len(proposed) != len(planes):
        return False
    for new, old in zip(proposed, planes):
        turn = abs((float(new["azimuth"]) - float(old["azimuth"]) + 180.0) % 360.0 - 180.0)
        tilt = abs(float(new["tilt"]) - float(old["tilt"]))
        size = abs(float(new["pdc0"]) - float(old["pdc0"])) / max(float(old["pdc0"]), 1.0)
        if turn > 0.5 or tilt > 0.5 or size > 0.005:
            return True
    return False


def apply_proposal(planes: list[dict[str, Any]], result: dict[str, Any]) -> list[dict[str, Any]]:
    """Take the proposed orientation and capacity, keep everything else (sensors, limits)."""
    proposed = result["planes"]
    if len(proposed) != len(planes):
        raise ValueError("the calibration is for a different number of planes")
    out = []
    for old, new in zip(planes, proposed):
        plane = dict(old)
        plane.update(
            azimuth=round(float(new["azimuth"]), 1),
            tilt=round(float(new["tilt"]), 1),
            pdc0=round(float(new["pdc0"])),
        )
        out.append(plane)
    return out


def summarize(planes: list[dict[str, Any]], result: dict[str, Any]) -> str:
    """Markdown summary of a calibration result for the options flow."""
    lines = []
    for i, (old, new) in enumerate(zip(planes, result.get("planes", [])), start=1):
        fit = (result.get("fits") or [{}] * len(planes))[i - 1]
        method = fit.get("method", "unchanged")
        lines.append(
            f"- Array {i}: {float(old['azimuth']):.0f}° / {float(old['tilt']):.0f}° / "
            f"{float(old['pdc0']) / 1000:.2f} kWp → **{float(new['azimuth']):.0f}° / {float(new['tilt']):.0f}° / "
            f"{float(new['pdc0']) / 1000:.2f} kWp** ({method.replace('_', ' ')}, {fit.get('hours', 0)} clear hours)"
        )
    cur, new = result.get("score_current"), result.get("score_proposed")
    if cur and new:
        where = "held-out weeks" if result.get("held_out") else "the same data it was fitted on"
        lines.append(
            f"- Hourly forecast error (RMSE) on {where}: {cur['rmse_w']:.0f} W → **{new['rmse_w']:.0f} W**"
            f" (bias {cur['bias_w']:+.0f} → {new['bias_w']:+.0f} W)"
        )
    lines += [f"- ⚠ {w}" for w in result.get("warnings", [])]
    return "\n".join(lines)
