"""Tests for the Python bindings (`vulpes_ems`), run against the built module.

Build it first with `maturin develop -m crates/vulpes-py/Cargo.toml`; the tests
are skipped when the module is not installed. No network access is needed.
"""

import json

import pytest

vulpes_ems = pytest.importorskip("vulpes_ems")

NOW = 1_781_000_000  # mid-June 2026, 10:13:20 UTC


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


def test_plan_with_fixed_import_and_export_prices():
    fixed = _request(tariff={"a": 0, "b": 0.30}, export={"a": 0, "b": 0.07})
    plan = json.loads(vulpes_ems.plan(json.dumps(fixed)))
    assert {s["export_price"] for s in plan["slots"]} == {0.07}


def test_plan_ends_where_the_weather_ends():
    # The request has 8 quarter hours of weather and asks for 10: enough to plan on.
    req = _request(horizon_h=3)
    last = req["weather"][-1]
    req["weather"] += [{**last, "start": last["start"] + 900 * k} for k in (1, 2)]
    plan = json.loads(vulpes_ems.plan(json.dumps(req)))
    assert len(plan["slots"]) == 10
    assert any("weather reaches 10 of 12 slots" in w for w in plan["warnings"])
    # With 8 of 12 it is too little, and the error says so.
    with pytest.raises(ValueError, match="weather covers 8 of 12 slots"):
        vulpes_ems.plan(json.dumps(_request(horizon_h=3)))


@pytest.mark.parametrize(
    "payload",
    [
        "{",
        json.dumps({"now": 1}),
        json.dumps(_request(modes=[])),
        json.dumps(_request(timezone="Nowhere/City")),
        json.dumps(_request(horizon_h=0)),  # no slots: used to panic in the engine
        json.dumps(_request(horizon=24)),  # a misspelled key
        json.dumps(_request(tariff={"a": 0, "b": 0.13})),  # export a is not 0
        json.dumps(_request(export={"net_metering": True, "a": 1.0, "b": 0.0})),  # a key from before 0.4.0
        json.dumps(_request(weather=[])),
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


# --- the Home Assistant adapter: vulpes_ems.ha, plan_ha, calibrate_ha -------------------


def ha(op, **request):
    return json.loads(vulpes_ems.ha(op, json.dumps(request)))


HA_CFG = {
    "soc_entity": "sensor.soc",
    "price_entity": "sensor.price",
    "grid_import": ["sensor.import"],
    "grid_export": ["sensor.export"],
    "capacity_kwh": 10.0,
    "max_charge_w": 3000,
    "max_discharge_w": 3000,
    "charge_eff": 0.95,
    "discharge_eff": 0.95,
    "min_soc": 11,
    "max_soc": 100,
    "wear_cost": 0.02,
    "max_import_w": 17250,
    "max_export_w": 17250,
    "tariff_a": 1.21,
    "tariff_b": 0.13,
    "export_a": 1.0,
    "export_b": 0.0,
    "charge_w": 3000,
    "planes": [{"azimuth": 180, "tilt": 35, "pdc0": 3000, "ac_max": 2800, "inverter": 0, "power_entity": "sensor.pv"}],
    "solar_energy": ["sensor.pv_energy"],
}


def test_every_ha_op_answers():
    """One call per op, so a routing or JSON mistake in the bindings shows up here."""
    plan = {"planned_at": NOW, "slots": [{"start": NOW - NOW % 900, "mode": "charge", "pv_off": False,
                                          "no_export": False, "grid_w": 2999.6, "battery_w": -1200.4}]}
    prices = [["today", [{"start": NOW, "price": 0.2}]]]
    stats = {"sensor.import": [[NOW - NOW % 3600, 0.5]], "sensor.export": [[NOW - NOW % 3600, 0.0]]}
    calls = {
        "constants": {},
        "error_keys": {},
        "settings": {"data": HA_CFG, "options": {}},
        "migrate": {"cfg": HA_CFG, "version": 3},
        "enabled_modes": {"cfg": HA_CFG},
        "agrees": {"cfg": HA_CFG, "state": "x", "mode_now": "hold"},
        "mode_map": {"cfg": HA_CFG, "input": {"charge": "Charging"}},
        "check_battery": {"cfg": HA_CFG},
        "check_tariff": {"cfg": HA_CFG},
        "check_modes": {"cfg": HA_CFG, "mode_options": None},
        "check_sources": {"cfg": HA_CFG, "soc_state": "50", "price_attributes": prices, "stats": stats},
        "dashboard_defaults": {"prefs": None},
        "energy_ids": {"cfg": HA_CFG},
        "command": {"cfg": HA_CFG, "plan": plan, "now": NOW, "failed": False},
        "safe_command": {"cfg": HA_CFG, "now": NOW, "fallback": False},
        "control_state": {"active": True, "last": {"fallback": False}, "error": None},
        "plane_from_array": {"array": {"direction": "s", "tilt": 35, "kwp": 3}, "inverter_w": 2800, "existing": None},
        "form_defaults": {"planes": HA_CFG["planes"]},
        "fits_form": {"planes": HA_CFG["planes"]},
        "engine_planes": {"planes": HA_CFG["planes"]},
        "parse_planes": {"text": json.dumps(HA_CFG["planes"])},
        "apply_proposal": {"planes": HA_CFG["planes"], "result": {"planes": HA_CFG["planes"]}},
        "summarize": {"planes": HA_CFG["planes"], "result": {"planes": HA_CFG["planes"]}},
        "calibration_view": {"cfg": HA_CFG, "stored": {}, "running": False, "error": False},
        "calibration_message": {"cfg": HA_CFG, "result": {"planes": HA_CFG["planes"]}},
        "calibration_inputs": {"cfg": HA_CFG},
        "wholesale_fetch": {"cfg": {"wholesale_source": "energyzero"}, "now": NOW, "fetched_at": None},
        "known_prices": {"price_attributes": prices, "price_unit": "EUR/kWh", "timezone": "UTC"},
        "load_series": {"cfg": HA_CFG, "stats": stats},
    }
    for op, request in calls.items():
        ha(op, **request)
    assert ha("constants")["config_version"] == 3
    with pytest.raises(ValueError, match="unknown op"):
        vulpes_ems.ha("no_such_op", "{}")


def test_script_variables_are_an_api():
    """The exact variables and types the control script gets (docs/control.md)."""
    start = NOW - NOW % 900
    plan = {"planned_at": NOW, "slots": [{"start": start, "mode": "charge", "pv_off": False, "no_export": False,
                                          "grid_w": 2999.6, "battery_w": -1200.4}]}
    cmd = ha("command", cfg=HA_CFG, plan=plan, now=NOW, failed=False)
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
        "until": "2026-06-09T10:15:00+00:00",
    }
    assert isinstance(cmd["power_w"], int) and isinstance(cmd["soc_min"], float)
    safe = ha("command", cfg=HA_CFG, plan=plan, now=NOW, failed=True)
    assert (safe["mode"], safe["fallback"], safe["planned_grid_w"]) == ("self_consume", True, None)


def test_plan_ha_offline():
    start = NOW - NOW % 900
    weather = [
        {"start": start + k * 900, "ghi": 500, "dni": 600, "dhi": 150, "temp_air": 15, "wind_speed": 2}
        for k in range(96 * 4)
    ]
    hour = NOW - NOW % 3600
    stats = {
        "sensor.import": [[hour - h * 3600, 0.4] for h in range(1, 24 * 7)],
        "sensor.export": [[hour - h * 3600, 0.0] for h in range(1, 24 * 7)],
    }
    request = {
        "cfg": {**HA_CFG, "pv_curtail": True, "solar_energy": []},
        "now": NOW + 0.5,
        "lat": 52.0,
        "lon": 5.0,
        "timezone": "Europe/Amsterdam",
        "country": "NL",
        "soc_state": "55",
        "price_attributes": [["today", [{"start": hour + h * 3600, "price": 0.25} for h in range(24)]]],
        "price_unit": "EUR/kWh",
        "stats": stats,
        "wholesale": [],
        "weather": weather,
    }
    plan = json.loads(vulpes_ems.plan_ha(json.dumps(request)))
    assert plan["planned_at"] == NOW
    assert plan["inputs"] == {"load_hours": 24 * 7 - 1, "known_price_slots": 24, "wholesale_hours": 0}
    assert all(":" not in s["mode"] and isinstance(s["pv_off"], bool) for s in plan["slots"])
    for key in ("pv_kwh_today", "pv_kwh_today_remaining", "pv_kwh_tomorrow", "pv_kwh_day_after"):
        assert key in plan["summary"], key
    with pytest.raises(ValueError, match="battery SOC from sensor.soc is unavailable"):
        vulpes_ems.plan_ha(json.dumps({**request, "soc_state": "unavailable"}))
    with pytest.raises(ValueError, match="planning failed"):
        vulpes_ems.plan_ha(json.dumps({**request, "stats": {}}))


def test_calibrate_ha_offline():
    start = NOW - NOW % 86_400
    weather = [
        {"start": start + k * 900, "ghi": 300, "dni": 200, "dhi": 150, "temp_air": 15, "wind_speed": 2}
        for k in range(2 * 96)
    ]
    request = {
        "cfg": HA_CFG,
        "lat": 52.0,
        "lon": 5.0,
        "strings": {"sensor.pv": []},
        "total": {"sensor.pv_energy": [[start + h * 3600, 0.5] for h in range(48)]},
        "weather": weather,
        "weather_day_ahead": weather,
    }
    out = json.loads(vulpes_ems.calibrate_ha(json.dumps(request)))
    assert out["based_on"] == [{k: v for k, v in HA_CFG["planes"][0].items() if k != "power_entity"}]
    assert out["result"]["warnings"][0].startswith("no hourly statistics for sensor.pv")
    with pytest.raises(ValueError, match="no solar arrays configured"):
        vulpes_ems.calibrate_ha(json.dumps({**request, "cfg": {}}))
