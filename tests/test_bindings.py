"""Tests for the Python bindings (`vulpes_ems`), run against the built module.

Build it first with `maturin develop -m crates/vulpes-py/Cargo.toml`; the tests
are skipped when the module is not installed. No network access is needed.
"""

import json

import pytest

vulpes_ems = pytest.importorskip("vulpes_ems")

NOW = 1_781_000_000  # mid-June 2026, 08:13:20 UTC


def _request(**overrides):
    start = NOW - NOW % 900
    req = {
        "now": NOW,
        "horizon_h": 2,
        "location": {"lat": 52.0, "lon": 5.0},
        "timezone": "Europe/Amsterdam",
        "country": "nl",
        "battery": {
            "capacity_wh": 10000,
            "max_charge_w": 3000,
            "max_discharge_w": 3000,
            "charge_eff": 0.95,
            "discharge_eff": 0.95,
            "min_soc": 0.1,
            "max_soc": 1.0,
            "wear_cost_per_kwh": 0.02,
        },
        "grid": {"max_import_w": 17250, "max_export_w": 17250},
        "modes": [
            {"name": "charge", "behaviour": {"type": "grid_setpoint", "w": 3000}},
            {"name": "self_consume", "behaviour": {"type": "self_consume"}},
        ],
        "soc_wh": 5000,
        "planes": [{"azimuth": 180, "tilt": 35, "pdc0": 3000, "ac_max": 2800}],
        "load_history": [[start - h * 3600 - (start % 3600), 400.0] for h in range(1, 24 * 7)],
        "known_prices": [{"start": start - 3600, "end": start + 4 * 3600, "import": 0.25}],
        "tariff": {"a": 1.21, "b": 0.13},
        "export": {"a": 1.21, "b": 0.0},
        # Weather supplied by the caller, so nothing is fetched.
        "weather": [
            {"start": start + k * 900, "ghi": 500, "dni": 600, "dhi": 150, "temp_air": 15, "wind_speed": 2}
            for k in range(8)
        ],
    }
    req.update(overrides)
    return req


def test_version_matches_package():
    from importlib.metadata import version

    assert vulpes_ems.__version__ == version("vulpes-ems")


def test_quarter_hour_floor():
    assert vulpes_ems.quarter_hour_floor(NOW) == NOW - NOW % 900
    assert vulpes_ems.quarter_hour_floor(900) == 900
    assert vulpes_ems.quarter_hour_floor(899) == 0


def test_plan_returns_a_response():
    resp = json.loads(vulpes_ems.plan(json.dumps(_request())))
    assert len(resp["slots"]) == 8
    assert resp["slots"][0]["start"] == NOW - NOW % 900
    assert resp["summary"]["mode_now"] in ("charge", "self_consume")
    assert all(s["price_known"] for s in resp["slots"])
    assert resp["version"] == vulpes_ems.__version__


def test_plan_blocks_export_when_it_costs_money():
    start = NOW - NOW % 900
    modes = [
        {"name": "self_consume", "behaviour": {"type": "self_consume"}},
        {"name": "self_consume:no_export", "behaviour": {"type": "self_consume"}, "no_export": True},
    ]

    def plan(export):
        req = _request(
            modes=modes,
            soc_wh=10000,  # full: the solar surplus can only go to the grid
            # 0.05 import at a = 1.21, b = 0.13 means a negative wholesale price.
            known_prices=[{"start": start - 3600, "end": start + 4 * 3600, "import": 0.05}],
            export=export,
        )
        return json.loads(vulpes_ems.plan(json.dumps(req)))

    blocked = plan({"a": 1.0, "b": 0.0})
    assert all(s["mode"] == "self_consume:no_export" for s in blocked["slots"]), blocked["slots"]
    assert all(s["grid_w"] >= -1e-6 for s in blocked["slots"])
    # Export tariff equal to the import tariff (net metering): export pays 0.05.
    paid = plan({"a": 1.21, "b": 0.13})
    assert all(s["mode"] == "self_consume" for s in paid["slots"])


@pytest.mark.parametrize(
    "payload",
    [
        "{",
        json.dumps({"now": 1}),
        json.dumps(_request(modes=[])),
        json.dumps(_request(timezone="Nowhere/City")),
        json.dumps(_request(horizon_h=0)),  # no slots: used to panic in the engine
        json.dumps(_request(horizon=24)),  # a misspelled key
        json.dumps(_request(tariff={"a": 0, "b": 0.13})),
    ],
)
def test_plan_errors_raise_value_error(payload):
    with pytest.raises(ValueError):
        vulpes_ems.plan(payload)


def test_energyzero_rejects_bad_dates_before_any_request():
    with pytest.raises(ValueError, match="bad date"):
        vulpes_ems.energyzero_hourly("2026-13-01", "2026-01-02")
    with pytest.raises(ValueError, match="bad date"):
        vulpes_ems.energyzero_hourly("2026-01-01", "tomorrow")


def test_calibrate_solar_with_supplied_weather():
    # Two days of flat weather and a total that matches nothing in particular:
    # too few clear hours, so the planes come back unchanged with a warning.
    start = NOW - NOW % 86_400
    weather = [
        {"start": start + k * 900, "ghi": 300, "dni": 200, "dhi": 150, "temp_air": 15, "wind_speed": 2}
        for k in range(2 * 96)
    ]
    planes = [{"azimuth": 90, "tilt": 30, "pdc0": 3000, "ac_max": 4000, "inverter": 0, "power_entity": "sensor.x"}]
    req = {
        "location": {"lat": 52.0, "lon": 5.0},
        "planes": planes,
        "total": [[start + h * 3600, 500.0] for h in range(48)],
        "weather": weather,
        "weather_day_ahead": weather,
    }
    resp = json.loads(vulpes_ems.calibrate_solar(json.dumps(req)))
    assert resp["version"] == vulpes_ems.__version__
    assert resp["planes"][0]["azimuth"] == 90 and resp["planes"][0]["inverter"] == 0
    assert "power_entity" not in resp["planes"][0]
    assert resp["fits"][0]["method"] == "unchanged"
    assert any("clear hours" in w for w in resp["warnings"])


@pytest.mark.parametrize(
    "payload",
    [
        "{",
        json.dumps({"location": {"lat": 52.0, "lon": 5.0}, "planes": []}),
        json.dumps({"location": {"lat": 52.0, "lon": 5.0}, "planes": [{"azimuth": 1, "tilt": 2, "pdc0": 3, "ac_max": 4}]}),
    ],
)
def test_calibrate_solar_errors_raise_value_error(payload):
    with pytest.raises(ValueError):
        vulpes_ems.calibrate_solar(payload)
