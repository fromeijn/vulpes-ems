"""Tests for the setup helpers and input checks (no Home Assistant needed)."""

from datetime import datetime, timezone
import importlib
from pathlib import Path
import sys
import types

import pytest

# setup_checks imports modes.py relatively; load both as submodules of a bare
# package so the integration's __init__ (which needs Home Assistant) never runs.
_pkg = types.ModuleType("vulpes_pkg")
_pkg.__path__ = [str(Path(__file__).parents[1] / "custom_components" / "vulpes_ems")]
sys.modules.setdefault("vulpes_pkg", _pkg)
sc = importlib.import_module("vulpes_pkg.setup_checks")

T0 = int(datetime(2026, 11, 8, 0, tzinfo=timezone.utc).timestamp())


# --- prices -----------------------------------------------------------------


def test_price_lists_from_attributes():
    attrs = {
        "today": [
            {"start": "2026-11-08T00:00:00+00:00", "price": 0.20},
            {"start": "2026-11-08T01:00:00+00:00", "price": 0.25},
        ],
        "tomorrow": [{"start": "2026-11-08T02:00:00+00:00", "price": 0.30}],
        "friendly_name": "Price",
    }
    prices = sc.parse_price_attributes(attrs, "EUR/kWh")
    assert prices == [
        {"start": T0, "end": T0 + 3600, "import": 0.20},
        {"start": T0 + 3600, "end": T0 + 7200, "import": 0.25},
        {"start": T0 + 7200, "end": T0 + 10800, "import": 0.30},  # last one: typical spacing
    ]


def test_price_lists_in_cents_and_quarter_hours():
    attrs = {"prices": [{"startsAt": T0 + 900 * i, "total": 20.0 + i} for i in range(3)]}
    prices = sc.parse_price_attributes(attrs, "ct/kWh")
    assert [p["end"] - p["start"] for p in prices] == [900, 900, 900]
    assert prices[0]["import"] == pytest.approx(0.20)


def test_epex_spot_hourly_price_list():
    """EPEX Spot: `data` with start_time/end_time/price_per_kwh, plus a scalar price next to it."""
    hour = lambda h: f"2026-11-08T{h:02d}:00:00+00:00"  # noqa: E731
    attrs = {
        "data": [
            {"start_time": hour(h), "end_time": hour(h + 1), "price_per_kwh": 0.20 + h / 100} for h in range(3)
        ],
        "price_per_kwh": 0.20,
        "unit_of_measurement": "€/kWh",
    }
    prices = sc.parse_price_attributes(attrs, "€/kWh")
    assert [(p["start"] - T0, p["end"] - p["start"]) for p in prices] == [(0, 3600), (3600, 3600), (7200, 3600)]
    assert [p["import"] for p in prices] == pytest.approx([0.20, 0.21, 0.22])


def test_price_attributes_without_prices():
    assert sc.parse_price_attributes({}, None) == []
    assert sc.parse_price_attributes({"options": ["a", "b"], "list": [], "x": [{"foo": 1}]}, None) == []
    # Unparsable times and non-numeric prices are skipped.
    attrs = {"today": [{"start": "soon", "price": 0.2}, {"start": T0, "price": "n/a"}, {"start": T0, "price": True}]}
    assert sc.parse_price_attributes(attrs, None) == []


# --- load balance -----------------------------------------------------------


def _rows(changes):
    return [{"start": float(T0 + 3600 * i), "change": c} for i, c in enumerate(changes) if c is not None]


def test_load_series_balances_and_skips_incomplete_hours():
    groups = {+1: ["sensor.import", "sensor.solar"], -1: ["sensor.export"]}
    stats = {
        "sensor.import": _rows([0.5, 0.2, 0.1]),
        "sensor.solar": _rows([0.0, None, 2.0]),
        "sensor.export": _rows([0.0, 0.0, 2.5]),
    }
    # Hour 1 lacks solar; hour 2 is negative (export > import + solar) and clamps to 0.
    assert sc.load_series(groups, stats) == [(T0, 500.0), (T0 + 7200, 0.0)]


def test_load_series_empty():
    assert sc.load_series({+1: [], -1: []}, {}) == []
    assert sc.load_series({+1: ["sensor.a"], -1: []}, {}) == []
    assert sc.load_series({+1: ["sensor.a"], -1: []}, {"sensor.a": []}) == []


def test_load_groups():
    cfg = {
        "grid_import": ["sensor.i"],
        "grid_export": ["sensor.e"],
        "solar_energy": ["sensor.s"],
        "battery_charge": ["sensor.bc"],
        "battery_discharge": ["sensor.bd"],
    }
    assert sc.load_groups(cfg) == {+1: ["sensor.i", "sensor.s", "sensor.bd"], -1: ["sensor.e", "sensor.bc"]}
    assert sc.load_groups({}) == {+1: [], -1: []}


# --- Energy Dashboard defaults ----------------------------------------------


def test_dashboard_defaults_legacy_format():
    prefs = {
        "energy_sources": [
            {
                "type": "grid",
                "flow_from": [
                    {"stat_energy_from": "sensor.import_t1", "entity_energy_price": "sensor.price"},
                    {"stat_energy_from": "sensor.import_t2", "entity_energy_price": "sensor.price"},
                ],
                "flow_to": [{"stat_energy_to": "sensor.export_t1"}],
            },
            {"type": "solar", "stat_energy_from": "sensor.pv"},
            {"type": "battery", "stat_energy_from": "sensor.bat_out", "stat_energy_to": "sensor.bat_in"},
        ]
    }
    d = sc.dashboard_defaults(prefs)
    assert d["grid_import"] == ["sensor.import_t1", "sensor.import_t2"]
    assert d["grid_export"] == ["sensor.export_t1"]
    assert d["solar_energy"] == ["sensor.pv"]
    assert d["battery_discharge"] == ["sensor.bat_out"]
    assert d["battery_charge"] == ["sensor.bat_in"]
    assert d["price_entity"] == "sensor.price"


def test_dashboard_defaults_unified_format_and_external_statistics():
    prefs = {
        "energy_sources": [
            {
                "type": "grid",
                "stat_energy_from": "sensor.import",
                "stat_energy_to": "sensor.export",
                "entity_energy_price": "sensor.price",
            },
            {"type": "solar", "stat_energy_from": "solar:external"},  # not an entity
            {"type": "gas", "stat_energy_from": "sensor.gas"},
        ]
    }
    d = sc.dashboard_defaults(prefs)
    assert d["grid_import"] == ["sensor.import"]
    assert d["grid_export"] == ["sensor.export"]
    assert d["solar_energy"] == []
    assert d["price_entity"] == "sensor.price"


def test_dashboard_defaults_price_only_when_unambiguous():
    prefs = {
        "energy_sources": [
            {
                "type": "grid",
                "flow_from": [
                    {"stat_energy_from": "sensor.t1", "entity_energy_price": "sensor.price_a"},
                    {"stat_energy_from": "sensor.t2", "entity_energy_price": "sensor.price_b"},
                ],
            }
        ]
    }
    assert "price_entity" not in sc.dashboard_defaults(prefs)
    one_number = {"energy_sources": [{"type": "grid", "flow_from": [{"stat_energy_from": "sensor.t1"}]}]}
    assert "price_entity" not in sc.dashboard_defaults(one_number)


def test_dashboard_defaults_without_prefs():
    empty = {k: [] for k in sc.ENERGY_KEYS}
    assert sc.dashboard_defaults(None) == empty
    assert sc.dashboard_defaults({}) == empty


# --- source checks ----------------------------------------------------------


def test_duplicate_statistics_across_fields():
    cfg = {
        "grid_import": ["sensor.a", "sensor.b"],
        "grid_export": ["sensor.c"],
        "solar_energy": ["sensor.a"],
        "battery_charge": ["sensor.c", "sensor.c"],
    }
    assert sc.duplicate_statistics(cfg) == ["sensor.a", "sensor.c"]
    assert sc.duplicate_statistics({"grid_import": ["sensor.a"], "grid_export": ["sensor.b"]}) == []
    assert sc.energy_ids(cfg) == ["sensor.a", "sensor.b", "sensor.c"]


@pytest.mark.parametrize(
    ("state", "value"),
    [
        ("55.5", 55.5),
        ("0", 0.0),
        ("100", 100.0),
        ("100.4", 100.0),  # a full battery that reports slightly over 100 %
        ("105", 100.0),
        ("-1", 0.0),
        ("106", None),
        ("-6", None),
        ("nan", None),
        ("unavailable", None),
        (None, None),
    ],
)
def test_soc_value(state, value):
    assert sc.soc_value(state) == value


# --- battery and mode checks ------------------------------------------------


def test_check_battery_soc_range():
    assert sc.check_battery({"min_soc": 10, "max_soc": 100}) == {}
    assert sc.check_battery({"min_soc": 50, "max_soc": 50}) == {"base": "soc_range"}
    assert sc.check_battery({"min_soc": 80, "max_soc": 20}) == {"base": "soc_range"}


LIMITS = {"max_charge_w": 3000, "max_discharge_w": 2500, "max_import_w": 10000, "max_export_w": 8000}


def test_check_modes_battery_power_uses_battery_limits():
    cfg = {**LIMITS, "power_control": "battery_power", "charge": True, "discharge": True}
    assert sc.check_modes({**cfg, "charge_w": 3000, "discharge_w": 2500}) == {}
    assert sc.check_modes({**cfg, "charge_w": 3001, "discharge_w": 2600}) == {
        "charge_w": "charge_w_too_high",
        "discharge_w": "discharge_w_too_high",
    }


def test_check_modes_grid_setpoint_uses_grid_limits():
    cfg = {**LIMITS, "power_control": "grid_setpoint", "charge": True, "discharge": True}
    # Above the battery limits is fine: the house load sits in between.
    assert sc.check_modes({**cfg, "charge_w": 5000, "discharge_w": 5000}) == {}
    assert sc.check_modes({**cfg, "charge_w": 12000, "discharge_w": 5000}) == {"charge_w": "charge_w_too_high"}


def test_check_modes_ignores_disabled_modes():
    cfg = {**LIMITS, "power_control": "battery_power", "charge": False, "discharge": False}
    assert sc.check_modes({**cfg, "charge_w": 99999, "discharge_w": 99999}) == {}
