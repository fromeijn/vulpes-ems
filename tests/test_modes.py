"""Tests for the integration's mode handling (no Home Assistant needed)."""

import importlib.util
from pathlib import Path

_path = Path(__file__).parents[1] / "custom_components" / "vulpes_ems" / "modes.py"
_spec = importlib.util.spec_from_file_location("vulpes_modes", _path)
modes = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(modes)

V1 = {
    "soc_entity": "sensor.battery_soc",
    "mode_entity": "input_select.ess_mode",
    "modes": [
        {"name": "Charging", "behaviour": {"type": "grid_setpoint", "w": 2500.0}, "pv_curtail": False},
        {"name": "Grid", "behaviour": {"type": "no_discharge"}, "pv_curtail": False},
        {"name": "Net Zero", "behaviour": {"type": "self_consume"}, "pv_curtail": False},
        {"name": "Discharging", "behaviour": {"type": "grid_setpoint", "w": -2800.0}, "pv_curtail": False},
    ],
}


def test_defaults_for_new_setup():
    assert modes.enabled_modes({}) == ["charge", "hold", "self_consume"]


def test_engine_modes_grid_setpoint_signs():
    cfg = {"charge": True, "discharge": True, "charge_w": 2500, "discharge_w": -2800}
    by_name = {m["name"]: m for m in modes.engine_modes(cfg)}
    assert by_name["charge"]["behaviour"] == {"type": "grid_setpoint", "w": 2500.0}
    assert by_name["discharge"]["behaviour"] == {"type": "grid_setpoint", "w": -2800.0}
    assert by_name["hold"]["behaviour"] == {"type": "no_discharge"}
    assert by_name["self_consume"]["behaviour"] == {"type": "self_consume"}
    assert "idle" not in by_name


def test_engine_modes_battery_power_and_pv_off_variants():
    cfg = {"power_control": "battery_power", "hold": False, "self_consume": False, "charge_w": 2000, "pv_curtail": True}
    out = modes.engine_modes(cfg)
    assert [m["name"] for m in out] == ["charge", "charge:pv_off"]
    assert out[1] == {
        "name": "charge:pv_off",
        "behaviour": {"type": "battery_power", "w": 2000.0},
        "pv_curtail": True,
        "no_export": False,
    }
    assert modes.split_mode("charge:pv_off") == ("charge", True, False)
    assert modes.split_mode("hold") == ("hold", False, False)


def test_engine_modes_no_export_variants_only_where_export_can_be_blocked():
    cfg = {"idle": True, "discharge": True, "charge_w": 2000, "discharge_w": 2000, "export_block": True}
    names = [m["name"] for m in modes.engine_modes(cfg)]
    assert names == [
        "charge",
        "hold",
        "hold:no_export",
        "self_consume",
        "self_consume:no_export",
        "idle",
        "idle:no_export",
        "discharge",
    ]
    variant = next(m for m in modes.engine_modes(cfg) if m["name"] == "self_consume:no_export")
    assert variant == {
        "name": "self_consume:no_export",
        "behaviour": {"type": "self_consume"},
        "pv_curtail": False,
        "no_export": True,
    }
    assert modes.split_mode("self_consume:no_export") == ("self_consume", False, True)
    assert all(not m["no_export"] for m in modes.engine_modes({**cfg, "export_block": False}))


def test_migrate_v1_keeps_behaviour_and_comparison():
    new = modes.migrate_v1(V1)
    assert "modes" not in new
    assert new["soc_entity"] == "sensor.battery_soc"
    assert modes.enabled_modes(new) == ["charge", "hold", "self_consume", "discharge"]
    assert (new["power_control"], new["charge_w"], new["discharge_w"]) == ("grid_setpoint", 2500.0, 2800.0)
    assert new["mode_map"] == {"charge": "Charging", "hold": "Grid", "self_consume": "Net Zero", "discharge": "Discharging"}
    assert modes.mode_of_state(new, "Net Zero") == "self_consume"
    assert modes.mode_of_state(new, "Something else") is None
    # Same engine behaviour as before, under Vulpes names.
    old = {m["name"]: m["behaviour"] for m in V1["modes"]}
    for m in modes.engine_modes(new):
        assert m["behaviour"] == old[new["mode_map"][m["name"]]]


def test_migrate_leaves_v2_and_empty_options_alone():
    assert modes.migrate_v1({}) == {}
    v2 = modes.migrate_v1(V1)
    assert modes.migrate_v1(v2) == v2


def test_migrate_v2_turns_net_metering_into_the_export_tariff():
    v2 = {"tariff_a": 1.21, "tariff_b": 0.13, "export_a": 1.1, "export_b": -0.02, "charge": True}
    # Switch on: export was paid at the import price, so the import tariff becomes the export tariff.
    on = modes.migrate_v2({**v2, "net_metering": True})
    assert on == {**v2, "export_a": 1.21, "export_b": 0.13}
    # Switch off: the export tariff was already in use and stays.
    assert modes.migrate_v2({**v2, "net_metering": False}) == v2


def test_migrate_v2_leaves_migrated_and_empty_settings_alone():
    assert modes.migrate_v2({}) == {}
    v3 = {"tariff_a": 1.21, "tariff_b": 0.13, "export_a": 1.0, "export_b": 0.0}
    assert modes.migrate_v2(v3) == v3
    # A switch without an import tariff (never saved by the flow) is only dropped.
    assert modes.migrate_v2({"net_metering": True, "export_a": 1.0}) == {"export_a": 1.0}


def test_options_replace_data_so_cleared_fields_stay_cleared():
    data = {"mode_entity": "input_select.ess_mode", "charge": True}
    assert modes.settings(data, {}) == data
    assert "mode_entity" not in modes.settings(data, {"charge": True})
