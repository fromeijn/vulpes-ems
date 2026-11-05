"""Config flow for Vulpes EMS: sources, battery, tariff, modes, solar planes."""

from __future__ import annotations

import json
from typing import Any

import voluptuous as vol

from homeassistant.config_entries import ConfigEntry, ConfigFlow, ConfigFlowResult, OptionsFlow
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import selector

from .const import (
    BEHAVIOURS,
    CONF_BATTERY_CHARGE,
    CONF_BATTERY_DISCHARGE,
    CONF_CAPACITY_KWH,
    CONF_CHARGE_EFF,
    CONF_DISCHARGE_EFF,
    CONF_EXPORT_A,
    CONF_EXPORT_B,
    CONF_GRID_EXPORT,
    CONF_GRID_IMPORT,
    CONF_MAX_CHARGE_W,
    CONF_MAX_DISCHARGE_W,
    CONF_MAX_EXPORT_W,
    CONF_MAX_IMPORT_W,
    CONF_MAX_SOC,
    CONF_MIN_SOC,
    CONF_MODE_ENTITY,
    CONF_MODES,
    CONF_NET_METERING,
    CONF_PLANES,
    CONF_PRICE_ENTITY,
    CONF_SOC_ENTITY,
    CONF_SOLAR_ENERGY,
    CONF_TARIFF_A,
    CONF_TARIFF_B,
    CONF_WEAR_COST,
    CONF_WHOLESALE_SOURCE,
    DOMAIN,
    WHOLESALE_SOURCES,
)

_SENSORS = selector.EntitySelector(selector.EntitySelectorConfig(domain="sensor", multiple=True))


def _num(minimum: float, maximum: float, step: float, unit: str | None = None) -> selector.NumberSelector:
    return selector.NumberSelector(
        selector.NumberSelectorConfig(
            min=minimum, max=maximum, step=step, unit_of_measurement=unit, mode=selector.NumberSelectorMode.BOX
        )
    )


async def energy_dashboard_defaults(hass: HomeAssistant) -> dict[str, list[str]]:
    """Statistic ids from the Energy Dashboard, to prefill the source step."""
    out: dict[str, list[str]] = {
        CONF_GRID_IMPORT: [],
        CONF_GRID_EXPORT: [],
        CONF_SOLAR_ENERGY: [],
        CONF_BATTERY_CHARGE: [],
        CONF_BATTERY_DISCHARGE: [],
    }
    try:
        from homeassistant.components.energy.data import async_get_manager  # noqa: PLC0415

        prefs = (await async_get_manager(hass)).data or {}
    except Exception:  # noqa: BLE001 - energy is optional
        return out
    for src in prefs.get("energy_sources", []):
        kind = src.get("type")
        if kind == "grid":
            # Unified format (stat_energy_from/to) or the legacy flow_from/flow_to lists.
            out[CONF_GRID_IMPORT] += [s for s in [src.get("stat_energy_from")] if s]
            out[CONF_GRID_EXPORT] += [s for s in [src.get("stat_energy_to")] if s]
            out[CONF_GRID_IMPORT] += [f["stat_energy_from"] for f in src.get("flow_from", [])]
            out[CONF_GRID_EXPORT] += [f["stat_energy_to"] for f in src.get("flow_to", [])]
        elif kind == "solar":
            out[CONF_SOLAR_ENERGY].append(src["stat_energy_from"])
        elif kind == "battery":
            out[CONF_BATTERY_DISCHARGE].append(src["stat_energy_from"])
            out[CONF_BATTERY_CHARGE].append(src["stat_energy_to"])
    # Only entity-based statistics can be chosen with an entity selector.
    return {k: [s for s in v if s.startswith("sensor.")] for k, v in out.items()}


def _sources_schema(d: dict[str, Any]) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_SOC_ENTITY, default=d.get(CONF_SOC_ENTITY, vol.UNDEFINED)): selector.EntitySelector(
                selector.EntitySelectorConfig(domain="sensor")
            ),
            vol.Required(CONF_MODE_ENTITY, default=d.get(CONF_MODE_ENTITY, vol.UNDEFINED)): selector.EntitySelector(
                selector.EntitySelectorConfig(domain=["input_select", "select"])
            ),
            vol.Required(
                CONF_PRICE_ENTITY, default=d.get(CONF_PRICE_ENTITY, vol.UNDEFINED)
            ): selector.EntitySelector(selector.EntitySelectorConfig(domain="sensor")),
            vol.Required(CONF_GRID_IMPORT, default=d.get(CONF_GRID_IMPORT, [])): _SENSORS,
            vol.Required(CONF_GRID_EXPORT, default=d.get(CONF_GRID_EXPORT, [])): _SENSORS,
            vol.Optional(CONF_SOLAR_ENERGY, default=d.get(CONF_SOLAR_ENERGY, [])): _SENSORS,
            vol.Optional(CONF_BATTERY_CHARGE, default=d.get(CONF_BATTERY_CHARGE, [])): _SENSORS,
            vol.Optional(CONF_BATTERY_DISCHARGE, default=d.get(CONF_BATTERY_DISCHARGE, [])): _SENSORS,
            vol.Required(
                CONF_WHOLESALE_SOURCE, default=d.get(CONF_WHOLESALE_SOURCE, "energyzero")
            ): selector.SelectSelector(
                selector.SelectSelectorConfig(options=WHOLESALE_SOURCES, translation_key=CONF_WHOLESALE_SOURCE)
            ),
        }
    )


def _battery_schema(d: dict[str, Any]) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_CAPACITY_KWH, default=d.get(CONF_CAPACITY_KWH, 10.0)): _num(0.5, 200, 0.01, "kWh"),
            vol.Required(CONF_MAX_CHARGE_W, default=d.get(CONF_MAX_CHARGE_W, 3000)): _num(100, 50000, 1, "W"),
            vol.Required(CONF_MAX_DISCHARGE_W, default=d.get(CONF_MAX_DISCHARGE_W, 3000)): _num(100, 50000, 1, "W"),
            vol.Required(CONF_CHARGE_EFF, default=d.get(CONF_CHARGE_EFF, 0.95)): _num(0.5, 1, 0.01),
            vol.Required(CONF_DISCHARGE_EFF, default=d.get(CONF_DISCHARGE_EFF, 0.95)): _num(0.5, 1, 0.01),
            vol.Required(CONF_MIN_SOC, default=d.get(CONF_MIN_SOC, 10)): _num(0, 90, 1, "%"),
            vol.Required(CONF_MAX_SOC, default=d.get(CONF_MAX_SOC, 100)): _num(10, 100, 1, "%"),
            vol.Required(CONF_WEAR_COST, default=d.get(CONF_WEAR_COST, 0.02)): _num(0, 1, 0.001, "€/kWh"),
            vol.Required(CONF_MAX_IMPORT_W, default=d.get(CONF_MAX_IMPORT_W, 17250)): _num(100, 100000, 1, "W"),
            vol.Required(CONF_MAX_EXPORT_W, default=d.get(CONF_MAX_EXPORT_W, 17250)): _num(0, 100000, 1, "W"),
        }
    )


def _tariff_schema(d: dict[str, Any]) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_TARIFF_A, default=d.get(CONF_TARIFF_A, 1.21)): _num(0, 5, 0.0001),
            vol.Required(CONF_TARIFF_B, default=d.get(CONF_TARIFF_B, 0.13)): _num(-1, 2, 0.0001, "€/kWh"),
            vol.Required(CONF_NET_METERING, default=d.get(CONF_NET_METERING, True)): selector.BooleanSelector(),
            vol.Required(CONF_EXPORT_A, default=d.get(CONF_EXPORT_A, 1.21)): _num(0, 5, 0.0001),
            vol.Required(CONF_EXPORT_B, default=d.get(CONF_EXPORT_B, 0.0)): _num(-1, 2, 0.0001, "€/kWh"),
        }
    )


def _mode_options(hass: HomeAssistant, entity_id: str) -> list[str]:
    state = hass.states.get(entity_id)
    return list(state.attributes.get("options", [])) if state else []


def _modes_schema(options: list[str], current: list[dict[str, Any]]) -> vol.Schema:
    by_name = {m["name"]: m for m in current}
    fields: dict[Any, Any] = {}
    for opt in options:
        b = by_name.get(opt, {}).get("behaviour", {"type": "self_consume"})
        fields[vol.Required(f"{opt}::type", default=b.get("type", "self_consume"))] = selector.SelectSelector(
            selector.SelectSelectorConfig(options=BEHAVIOURS, translation_key="behaviour")
        )
        fields[vol.Required(f"{opt}::w", default=b.get("w", 0))] = _num(-50000, 50000, 1, "W")
        fields[vol.Required(f"{opt}::curtail", default=by_name.get(opt, {}).get("pv_curtail", False))] = (
            selector.BooleanSelector()
        )
    return vol.Schema(fields)


def _modes_from_input(options: list[str], user_input: dict[str, Any]) -> list[dict[str, Any]]:
    modes = []
    for opt in options:
        kind = user_input[f"{opt}::type"]
        behaviour: dict[str, Any] = {"type": kind}
        if kind in ("grid_setpoint", "battery_power"):
            behaviour["w"] = float(user_input[f"{opt}::w"])
        modes.append({"name": opt, "behaviour": behaviour, "pv_curtail": bool(user_input[f"{opt}::curtail"])})
    return modes


def parse_planes(text: str) -> list[dict[str, Any]]:
    """Accept a list of plane parameter objects, or `calibrate-solar` output."""
    data = json.loads(text)
    if isinstance(data, dict):
        data = [v["params"] if "params" in v else v for v in data.values()]
    if not isinstance(data, list) or not data:
        raise ValueError("expected a non-empty list of planes")
    for p in data:
        for key in ("azimuth", "tilt", "pdc0", "ac_max"):
            float(p[key])
    return data


def _solar_schema(d: dict[str, Any]) -> vol.Schema:
    default = json.dumps(d.get(CONF_PLANES, []), indent=1) if d.get(CONF_PLANES) else ""
    return vol.Schema(
        {
            vol.Required(CONF_PLANES, default=default): selector.TextSelector(
                selector.TextSelectorConfig(multiline=True)
            )
        }
    )


class _Steps:
    """Shared step logic for the config and options flows."""

    hass: HomeAssistant
    data: dict[str, Any]

    def _finish(self) -> ConfigFlowResult:  # pragma: no cover - implemented by subclasses
        raise NotImplementedError

    async def async_step_battery(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        if user_input is not None:
            self.data.update(user_input)
            return await self.async_step_tariff()
        return self.async_show_form(step_id="battery", data_schema=_battery_schema(self.data))  # type: ignore[attr-defined]

    async def async_step_tariff(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        if user_input is not None:
            self.data.update(user_input)
            return await self.async_step_modes()
        return self.async_show_form(step_id="tariff", data_schema=_tariff_schema(self.data))  # type: ignore[attr-defined]

    async def async_step_modes(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        options = _mode_options(self.hass, self.data[CONF_MODE_ENTITY])
        if not options:
            return self.async_abort(reason="no_mode_options")  # type: ignore[attr-defined]
        if user_input is not None:
            self.data[CONF_MODES] = _modes_from_input(options, user_input)
            return await self.async_step_solar()
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="modes", data_schema=_modes_schema(options, self.data.get(CONF_MODES, []))
        )

    async def async_step_solar(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            try:
                self.data[CONF_PLANES] = parse_planes(user_input[CONF_PLANES])
            except (ValueError, KeyError, TypeError):
                errors[CONF_PLANES] = "invalid_planes"
            else:
                return self._finish()
        return self.async_show_form(step_id="solar", data_schema=_solar_schema(self.data), errors=errors)  # type: ignore[attr-defined]


class VulpesEmsConfigFlow(_Steps, ConfigFlow, domain=DOMAIN):
    """Handle a config flow for Vulpes EMS."""

    VERSION = 1

    def __init__(self) -> None:
        self.data: dict[str, Any] = {}

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        if user_input is not None:
            self.data.update(user_input)
            return await self.async_step_battery()
        defaults = await energy_dashboard_defaults(self.hass)
        return self.async_show_form(step_id="user", data_schema=_sources_schema(defaults))

    def _finish(self) -> ConfigFlowResult:
        return self.async_create_entry(title="Vulpes EMS", data=self.data)

    @staticmethod
    @callback
    def async_get_options_flow(config_entry: ConfigEntry) -> OptionsFlow:
        return VulpesEmsOptionsFlow()


class VulpesEmsOptionsFlow(_Steps, OptionsFlow):
    """Edit any setting after setup; stored as options (merged over data)."""

    def __init__(self) -> None:
        self.data: dict[str, Any] = {}

    async def async_step_init(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        if not self.data:
            self.data = {**self.config_entry.data, **self.config_entry.options}
        if user_input is not None:
            self.data.update(user_input)
            return await self.async_step_battery()
        return self.async_show_form(step_id="init", data_schema=_sources_schema(self.data))

    def _finish(self) -> ConfigFlowResult:
        return self.async_create_entry(data=self.data)
