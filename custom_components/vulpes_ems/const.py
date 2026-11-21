"""Constants for Vulpes EMS: the settings keys the forms use.

Option lists, defaults and history windows come from the Rust core (`core.C`).
"""

from __future__ import annotations

DOMAIN = "vulpes_ems"

# Sources (step "user")
CONF_SOC_ENTITY = "soc_entity"
CONF_PRICE_ENTITY = "price_entity"
CONF_GRID_IMPORT = "grid_import"
CONF_GRID_EXPORT = "grid_export"
CONF_SOLAR_ENERGY = "solar_energy"
CONF_BATTERY_CHARGE = "battery_charge"
CONF_BATTERY_DISCHARGE = "battery_discharge"
CONF_WHOLESALE_SOURCE = "wholesale_source"

# Battery and grid (step "battery")
CONF_CAPACITY_KWH = "capacity_kwh"
CONF_MAX_CHARGE_W = "max_charge_w"
CONF_MAX_DISCHARGE_W = "max_discharge_w"
CONF_CHARGE_EFF = "charge_eff"
CONF_DISCHARGE_EFF = "discharge_eff"
CONF_MIN_SOC = "min_soc"
CONF_MAX_SOC = "max_soc"
CONF_WEAR_COST = "wear_cost"
CONF_MAX_IMPORT_W = "max_import_w"
CONF_MAX_EXPORT_W = "max_export_w"

# Tariff (step "tariff")
CONF_TARIFF_A = "tariff_a"
CONF_TARIFF_B = "tariff_b"
CONF_EXPORT_A = "export_a"
CONF_EXPORT_B = "export_b"
CONF_BILLING_INTERVAL = "billing_interval"

# Modes (step "modes"); each mode key from core.C["modes"] is a boolean field too
CONF_POWER_CONTROL = "power_control"
CONF_CHARGE_W = "charge_w"
CONF_DISCHARGE_W = "discharge_w"
CONF_PV_CURTAIL = "pv_curtail"
CONF_EXPORT_BLOCK = "export_block"
CONF_MODE_ENTITY = "mode_entity"
CONF_MODE_MAP = "mode_map"
CONF_CONTROL_SCRIPT = "control_script"

# Solar (steps "solar*")
CONF_PLANES = "planes"
CONF_SOLAR_SETUP = "solar_setup"  # "form" or "json"; absent for setups before 0.3.0
CONF_ARRAY_COUNT = "array_count"
CONF_INVERTER_W = "inverter_ac_w"
CONF_DIRECTION = "direction"
CONF_TILT = "tilt"
CONF_KWP = "kwp"
CONF_POWER_ENTITY = "power_entity"

WHOLESALE_SOURCES = ["energyzero", "none"]
# How the contract bills; "quarter" is the default, also for settings from before the option.
BILLING_INTERVALS = ["quarter", "hour"]

# First automatic calibration after setup: late enough not to slow down startup.
CALIBRATION_DELAY_S = 600
