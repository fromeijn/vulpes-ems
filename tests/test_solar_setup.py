"""Tests for the solar array form and calibration helpers (no Home Assistant needed)."""

from datetime import datetime, timezone
import importlib.util
from pathlib import Path

import pytest

_path = Path(__file__).parents[1] / "custom_components" / "vulpes_ems" / "solar_setup.py"
_spec = importlib.util.spec_from_file_location("vulpes_solar_setup", _path)
ss = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ss)


def test_directions_and_degrees():
    assert ss.azimuth_of("s") == 180.0
    assert ss.azimuth_of("NW") == 315.0
    assert ss.azimuth_of("200") == 200.0
    assert ss.azimuth_of(" 95° ") == 95.0
    assert ss.azimuth_of(360) == 0.0
    for bad in ("up", "-5", "361", ""):
        with pytest.raises(ValueError):
            ss.azimuth_of(bad)
    assert ss.direction_of(90) == "e"
    assert ss.direction_of(450) == "e"
    assert ss.direction_of(97.5) == "97.5"


def test_plane_from_array_shares_the_inverter_and_keeps_extra_keys():
    array = {"direction": "e", "tilt": 30, "kwp": 3.2, "power_entity": "sensor.pv1_power"}
    plane = ss.plane_from_array(array, 4000, existing={"gamma": -0.0035, "power_entity": "sensor.old"})
    assert plane == {
        "azimuth": 90.0,
        "tilt": 30.0,
        "pdc0": 3200.0,
        "ac_max": 4000.0,
        "inverter": 0,
        "gamma": -0.0035,
        "power_entity": "sensor.pv1_power",
    }
    # Clearing the sensor removes it.
    cleared = ss.plane_from_array({"direction": "e", "tilt": 30, "kwp": 3.2}, 4000, existing=plane)
    assert "power_entity" not in cleared


def test_form_round_trip():
    arrays = [
        {"direction": "e", "tilt": 25.0, "kwp": 3.2, "power_entity": "sensor.pv1_power"},
        {"direction": "275", "tilt": 25.0, "kwp": 2.4},
    ]
    planes = [ss.plane_from_array(a, 3600, None) for a in arrays]
    assert ss.fits_form(planes)
    count, inverter_w, prefill = ss.form_defaults(planes)
    assert (count, inverter_w) == (2, 3600.0)
    assert prefill == arrays
    assert [ss.plane_from_array(a, inverter_w, p) for a, p in zip(prefill, planes)] == planes


def test_which_setups_the_form_can_edit():
    single = [{"azimuth": 180, "tilt": 35, "pdc0": 3000, "ac_max": 2800}]
    assert ss.fits_form(single)
    # Pasted microinverter planes: one inverter each, so not form-editable.
    micro = [{"azimuth": 180, "tilt": 35, "pdc0": 400, "ac_max": 300} for _ in range(3)]
    assert not ss.fits_form(micro)
    assert ss.form_defaults(micro) == (1, ss.DEFAULT_INVERTER_W, [])
    shared = [dict(p, inverter=0) for p in micro]
    assert ss.fits_form(shared)
    assert not ss.fits_form(shared[:1] + [dict(shared[1], ac_max=500)])
    assert not ss.fits_form([dict(p, inverter=0) for p in micro * 2]), "more than MAX_ARRAYS"
    assert not ss.fits_form([])


def test_engine_planes_drop_local_keys():
    planes = [{"azimuth": 90, "tilt": 30, "pdc0": 3000, "ac_max": 4000, "power_entity": "sensor.x"}]
    assert ss.engine_planes(planes) == [{"azimuth": 90, "tilt": 30, "pdc0": 3000, "ac_max": 4000}]
    assert planes[0]["power_entity"] == "sensor.x", "input left alone"


def test_hourly_series_from_statistics_rows():
    t = datetime(2026, 6, 1, 12, tzinfo=timezone.utc)
    rows = [
        {"start": t.timestamp() + 3600, "mean": 1.5},
        {"start": t, "mean": 2.0},
        {"start": t.timestamp() + 7200, "mean": None},
        {"mean": 3.0},
    ]
    assert ss.hourly_series(rows, "mean") == [(int(t.timestamp()), 2.0), (int(t.timestamp()) + 3600, 1.5)]
    assert ss.hourly_series(rows, "mean", 1000.0)[0][1] == 2000.0
    assert ss.hourly_series([], "mean") == []


def test_total_power_sums_complete_hours():
    energy = {"sensor.a": [(0, 1.0), (3600, 0.5)], "sensor.b": [(0, 0.25), (7200, 1.0)]}
    assert ss.total_power(energy) == [(0, 1250.0)]
    assert ss.total_power({"sensor.a": [(0, -0.01)]}) == [(0, 0.0)], "never negative"
    assert ss.total_power({}) == []


def test_calibration_request():
    planes = [
        {"azimuth": 90, "tilt": 30, "pdc0": 3000, "ac_max": 4000, "inverter": 0, "power_entity": "sensor.pv1"},
        {"azimuth": 270, "tilt": 30, "pdc0": 3000, "ac_max": 4000, "inverter": 0},
    ]
    req = ss.calibration_request(52.0, 5.0, planes, {"sensor.pv1": [(0, 100.0)]}, [(0, 95.0)])
    assert req["strings"] == [[(0, 100.0)], []]
    assert "power_entity" not in req["planes"][0]
    assert req["total"] == [(0, 95.0)]
    assert ss.calibration_request(52.0, 5.0, planes, {}, [])["strings"] == []


RESULT = {
    "planes": [
        {"azimuth": 95.4, "tilt": 24.6, "pdc0": 3212.7, "ac_max": 4000, "inverter": 0},
        {"azimuth": 270.0, "tilt": 30.0, "pdc0": 3000.0, "ac_max": 4000, "inverter": 0},
    ],
    "fits": [{"method": "string", "hours": 120, "rmse_w": 40.0}, {"method": "unchanged", "hours": 0, "rmse_w": 0.0}],
    "score_current": {"hours": 300, "mae_w": 150.0, "rmse_w": 200.0, "bias_w": 50.0},
    "score_proposed": {"hours": 300, "mae_w": 100.0, "rmse_w": 150.0, "bias_w": -5.0},
    "held_out": True,
    "warnings": ["planes without a string power sensor are kept as entered"],
}
PLANES = [
    {"azimuth": 90.0, "tilt": 30.0, "pdc0": 3000.0, "ac_max": 4000.0, "inverter": 0, "power_entity": "sensor.pv1"},
    {"azimuth": 270.0, "tilt": 30.0, "pdc0": 3000.0, "ac_max": 4000.0, "inverter": 0},
]


def test_apply_proposal_keeps_sensors_and_limits():
    applied = ss.apply_proposal(PLANES, RESULT)
    assert applied[0] == {
        "azimuth": 95.4,
        "tilt": 24.6,
        "pdc0": 3213,
        "ac_max": 4000.0,
        "inverter": 0,
        "power_entity": "sensor.pv1",
    }
    assert applied[1] == PLANES[1]
    with pytest.raises(ValueError):
        ss.apply_proposal(PLANES[:1], RESULT)


def test_changes_and_improvement():
    assert ss.has_changes(RESULT, PLANES)
    assert not ss.has_changes(RESULT, ss.apply_proposal(PLANES, RESULT)), "rounding is not a change"
    assert not ss.has_changes(RESULT, PLANES[:1])
    north = [dict(PLANES[1], azimuth=359.8)]
    assert not ss.has_changes({"planes": [dict(PLANES[1], azimuth=0.1)]}, north), "wraps around north"
    assert ss.improvement_pct(RESULT) == 25.0
    assert ss.improvement_pct({"score_current": None, "score_proposed": None}) is None
    assert ss.improvement_pct({"score_current": {"rmse_w": 0.0}, "score_proposed": {"rmse_w": 0.0}}) is None


def test_summary_lists_changes_scores_and_warnings():
    text = ss.summarize(PLANES, RESULT)
    assert "Array 1: 90° / 30° / 3.00 kWp → **95° / 25° / 3.21 kWp** (string, 120 clear hours)" in text
    assert "Array 2:" in text and "(unchanged, 0 clear hours)" in text
    assert "held-out weeks: 200 W → **150 W**" in text
    assert "⚠ planes without a string power sensor" in text
    assert "same data" in ss.summarize(PLANES, dict(RESULT, held_out=False))
    assert "RMSE" not in ss.summarize(PLANES, dict(RESULT, score_current=None))


def test_calibration_status():
    base = {"running": False, "error": False, "applied": False, "proposal": False, "result": False, "stored": False}
    status = lambda **kw: ss.calibration_status(**{**base, **kw})  # noqa: E731
    assert status() == "not_run"
    assert status(stored=True) == "stale"
    assert status(result=True, stored=True) == "done"
    assert status(applied=True, result=True, stored=True) == "applied"
    # A new run on the applied planes proposes more changes: there is something to apply again.
    assert status(applied=True, proposal=True, result=True, stored=True) == "done"
    assert status(error=True, proposal=True) == "failed"
    assert status(running=True, error=True) == "running"
