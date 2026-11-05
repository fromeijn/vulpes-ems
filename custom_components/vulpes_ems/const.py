"""Constants for Vulpes EMS."""

from __future__ import annotations

DOMAIN = "vulpes_ems"

# Sources (step "user")
CONF_SOC_ENTITY = "soc_entity"
CONF_MODE_ENTITY = "mode_entity"
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
CONF_NET_METERING = "net_metering"
CONF_EXPORT_A = "export_a"
CONF_EXPORT_B = "export_b"

# Modes (step "modes") and solar (step "solar")
CONF_MODES = "modes"
CONF_PLANES = "planes"

BEHAVIOURS = ["grid_setpoint", "self_consume", "no_discharge", "idle", "battery_power"]
WHOLESALE_SOURCES = ["energyzero", "none"]

LOAD_HISTORY_DAYS = 60
WHOLESALE_HISTORY_DAYS = 35
HORIZON_HOURS = 36
