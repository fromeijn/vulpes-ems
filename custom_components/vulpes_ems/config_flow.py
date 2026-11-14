"""Config flow for Vulpes EMS: sources, battery, tariff, modes, comparison, solar arrays.

The config flow walks through every step; the options flow is a menu of
sections, each saved on its own.
"""

from __future__ import annotations

import json
from typing import Any

import voluptuous as vol

from homeassistant.config_entries import ConfigEntry, ConfigFlow, ConfigFlowResult, OptionsFlow
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import selector
from homeassistant.util import dt as dt_util

from .const import (
    CONF_ARRAY_COUNT,
    CONF_BATTERY_CHARGE,
    CONF_BATTERY_DISCHARGE,
    CONF_CAPACITY_KWH,
    CONF_CHARGE_EFF,
    CONF_CHARGE_W,
    CONF_CONTROL_SCRIPT,
    CONF_DIRECTION,
    CONF_DISCHARGE_EFF,
    CONF_DISCHARGE_W,
    CONF_EXPORT_A,
    CONF_EXPORT_B,
    CONF_EXPORT_BLOCK,
    CONF_GRID_EXPORT,
    CONF_GRID_IMPORT,
    CONF_INVERTER_W,
    CONF_KWP,
    CONF_MAX_CHARGE_W,
    CONF_MAX_DISCHARGE_W,
    CONF_MAX_EXPORT_W,
    CONF_MAX_IMPORT_W,
    CONF_MAX_SOC,
    CONF_MIN_SOC,
    CONF_MODE_ENTITY,
    CONF_MODE_MAP,
    CONF_PLANES,
    CONF_POWER_CONTROL,
    CONF_POWER_ENTITY,
    CONF_PRICE_ENTITY,
    CONF_PV_CURTAIL,
    CONF_SOC_ENTITY,
    CONF_SOLAR_ENERGY,
    CONF_SOLAR_SETUP,
    CONF_TARIFF_A,
    CONF_TARIFF_B,
    CONF_TILT,
    CONF_WEAR_COST,
    CONF_WHOLESALE_SOURCE,
    DOMAIN,
    WHOLESALE_SOURCES,
)
from .coordinator import async_energy_changes, history_start
from .core import C, ha, stat_rows

_SENSORS = selector.EntitySelector(selector.EntitySelectorConfig(domain="sensor", multiple=True))


def _num(minimum: float, maximum: float, step: float | str, unit: str | None = None) -> selector.NumberSelector:
    config = selector.NumberSelectorConfig(min=minimum, max=maximum, step=step, mode=selector.NumberSelectorMode.BOX)
    if unit is not None:  # HA 2026.11 rejects an explicit None
        config["unit_of_measurement"] = unit
    return selector.NumberSelector(config)


async def energy_dashboard_defaults(hass: HomeAssistant) -> dict[str, Any]:
    """Defaults from the Energy Dashboard, to prefill the source step."""
    try:
        from homeassistant.components.energy.data import async_get_manager  # noqa: PLC0415

        prefs = (await async_get_manager(hass)).data
    except Exception:  # noqa: BLE001 - energy is optional
        prefs = None
    return ha("dashboard_defaults", prefs=prefs)


def _sources_schema(d: dict[str, Any]) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_SOC_ENTITY, default=d.get(CONF_SOC_ENTITY, vol.UNDEFINED)): selector.EntitySelector(
                selector.EntitySelectorConfig(domain="sensor")
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


def _battery_schema(d: dict[str, Any], currency: str) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_CAPACITY_KWH, default=d.get(CONF_CAPACITY_KWH, 10.0)): _num(0.5, 200, 0.01, "kWh"),
            vol.Required(CONF_MAX_CHARGE_W, default=d.get(CONF_MAX_CHARGE_W, 3000)): _num(100, 50000, 1, "W"),
            vol.Required(CONF_MAX_DISCHARGE_W, default=d.get(CONF_MAX_DISCHARGE_W, 3000)): _num(100, 50000, 1, "W"),
            vol.Required(CONF_CHARGE_EFF, default=d.get(CONF_CHARGE_EFF, 0.95)): _num(0.5, 1, 0.01),
            vol.Required(CONF_DISCHARGE_EFF, default=d.get(CONF_DISCHARGE_EFF, 0.95)): _num(0.5, 1, 0.01),
            vol.Required(CONF_MIN_SOC, default=d.get(CONF_MIN_SOC, 10)): _num(0, 90, 1, "%"),
            vol.Required(CONF_MAX_SOC, default=d.get(CONF_MAX_SOC, 100)): _num(10, 100, 1, "%"),
            vol.Required(CONF_WEAR_COST, default=d.get(CONF_WEAR_COST, 0.02)): _num(0, 1, 0.001, f"{currency}/kWh"),
            vol.Required(CONF_MAX_IMPORT_W, default=d.get(CONF_MAX_IMPORT_W, 17250)): _num(100, 100000, 1, "W"),
            vol.Required(CONF_MAX_EXPORT_W, default=d.get(CONF_MAX_EXPORT_W, 17250)): _num(0, 100000, 1, "W"),
        }
    )


def _tariff_schema(d: dict[str, Any], currency: str) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_TARIFF_A, default=d.get(CONF_TARIFF_A, 1.21)): _num(0, 5, "any"),
            vol.Required(CONF_TARIFF_B, default=d.get(CONF_TARIFF_B, 0.13)): _num(-1, 2, "any", f"{currency}/kWh"),
            vol.Required(CONF_EXPORT_A, default=d.get(CONF_EXPORT_A, 1.0)): _num(0, 5, "any"),
            vol.Required(CONF_EXPORT_B, default=d.get(CONF_EXPORT_B, 0.0)): _num(-1, 2, "any", f"{currency}/kWh"),
        }
    )


def _mode_options(hass: HomeAssistant, entity_id: str | None) -> list[str]:
    state = hass.states.get(entity_id) if entity_id else None
    return list(state.attributes.get("options", [])) if state else []


def _modes_schema(d: dict[str, Any]) -> vol.Schema:
    fields: dict[Any, Any] = {
        vol.Required(CONF_POWER_CONTROL, default=d.get(CONF_POWER_CONTROL, "grid_setpoint")): selector.SelectSelector(
            selector.SelectSelectorConfig(options=C["power_controls"], translation_key=CONF_POWER_CONTROL)
        ),
    }
    for key, default in C["modes"]:
        fields[vol.Required(key, default=d.get(key, default))] = selector.BooleanSelector()
        if key == "charge":
            fields[vol.Required(CONF_CHARGE_W, default=d.get(CONF_CHARGE_W, 3000))] = _num(0, 50000, 1, "W")
        if key == "discharge":
            fields[vol.Required(CONF_DISCHARGE_W, default=d.get(CONF_DISCHARGE_W, 3000))] = _num(0, 50000, 1, "W")
    fields[vol.Required(CONF_PV_CURTAIL, default=d.get(CONF_PV_CURTAIL, False))] = selector.BooleanSelector()
    fields[vol.Required(CONF_EXPORT_BLOCK, default=d.get(CONF_EXPORT_BLOCK, False))] = selector.BooleanSelector()
    mode_entity = vol.Optional(CONF_MODE_ENTITY, description={"suggested_value": d.get(CONF_MODE_ENTITY)})
    fields[mode_entity] = selector.EntitySelector(selector.EntitySelectorConfig(domain=["input_select", "select"]))
    script = vol.Optional(CONF_CONTROL_SCRIPT, description={"suggested_value": d.get(CONF_CONTROL_SCRIPT)})
    fields[script] = selector.EntitySelector(selector.EntitySelectorConfig(domain="script"))
    return vol.Schema(fields)


def _compare_schema(modes: list[str], options: list[str], current: dict[str, str]) -> vol.Schema:
    choice = selector.SelectSelector(selector.SelectSelectorConfig(options=options))
    return vol.Schema(
        {
            vol.Optional(key, description={"suggested_value": current.get(key) if current.get(key) in options else None}): choice
            for key in modes
        }
    )


def _solar_json_schema(d: dict[str, Any]) -> vol.Schema:
    default = json.dumps(d.get(CONF_PLANES, []), indent=1) if d.get(CONF_PLANES) else ""
    return vol.Schema(
        {
            vol.Required(CONF_PLANES, default=default): selector.TextSelector(
                selector.TextSelectorConfig(multiline=True)
            )
        }
    )


def _solar_system_schema(count: int, inverter_w: float) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_ARRAY_COUNT, default=count): _num(1, C["max_arrays"], 1),
            vol.Required(CONF_INVERTER_W, default=inverter_w): _num(100, 100000, 1, "W"),
        }
    )


def _solar_array_schema(d: dict[str, Any]) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_DIRECTION, default=str(d.get(CONF_DIRECTION, "s"))): selector.SelectSelector(
                selector.SelectSelectorConfig(
                    options=C["directions"],
                    custom_value=True,
                    mode=selector.SelectSelectorMode.DROPDOWN,
                    translation_key=CONF_DIRECTION,
                )
            ),
            vol.Required(CONF_TILT, default=d.get(CONF_TILT, 35)): _num(0, 90, 1, "°"),
            vol.Required(CONF_KWP, default=d.get(CONF_KWP, 4.0)): _num(0.05, 100, 0.01, "kWp"),
            vol.Optional(CONF_POWER_ENTITY, description={"suggested_value": d.get(CONF_POWER_ENTITY)}): (
                selector.EntitySelector(selector.EntitySelectorConfig(domain="sensor", device_class="power"))
            ),
        }
    )


class _Steps:
    """Shared step logic for the config and options flows."""

    hass: HomeAssistant
    data: dict[str, Any]

    def _finish(self) -> ConfigFlowResult:  # pragma: no cover - implemented by subclasses
        raise NotImplementedError

    async def _next(self, after: str) -> ConfigFlowResult:  # pragma: no cover - implemented by subclasses
        """Where to go once the step `after` is done."""
        raise NotImplementedError

    @property
    def _currency(self) -> str:
        return self.hass.config.currency

    async def _validate_sources(self, d: dict[str, Any]) -> tuple[dict[str, str], dict[str, str]]:
        """Errors and placeholders for the source step, checked against live states and statistics."""
        soc = self.hass.states.get(d[CONF_SOC_ENTITY])
        price = self.hass.states.get(d[CONF_PRICE_ENTITY])
        request = {
            "cfg": d,
            "soc_state": soc.state if soc else None,
            "price_attributes": list(price.attributes.items()) if price else None,
            "price_unit": price.attributes.get("unit_of_measurement") if price else None,
            "timezone": str(self.hass.config.time_zone),
        }
        result = ha("check_sources", **request)
        if result["fetch"]:
            start = history_start(dt_util.utcnow(), C["load_history_days"])
            stats = await async_energy_changes(self.hass, set(result["fetch"]), start)
            result = ha("check_sources", **request, stats=stat_rows(stats, result["fetch"], "change"))
        return result["errors"], result["placeholders"]

    async def _sources(
        self, step_id: str, user_input: dict[str, Any] | None, defaults: dict[str, Any]
    ) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        placeholders = {"statistic": ""}
        if user_input is not None:
            candidate = {**self.data, **user_input}
            errors, placeholders = await self._validate_sources(candidate)
            if not errors:
                self.data = candidate
                return await self._next(step_id)
            defaults = candidate
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id=step_id,
            data_schema=_sources_schema(defaults),
            errors=errors,
            description_placeholders=placeholders,
        )

    async def async_step_battery(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        defaults = self.data
        if user_input is not None:
            candidate = {**self.data, **user_input}
            errors = ha("check_battery", cfg=candidate)
            if not errors:
                self.data = candidate
                return await self._next("battery")
            defaults = candidate
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="battery", data_schema=_battery_schema(defaults, self._currency), errors=errors
        )

    async def async_step_tariff(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        defaults = self.data
        if user_input is not None:
            candidate = {**self.data, **user_input}
            errors = ha("check_tariff", cfg=candidate)
            if not errors:
                self.data = candidate
                return await self._next("tariff")
            defaults = candidate
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="tariff", data_schema=_tariff_schema(defaults, self._currency), errors=errors
        )

    async def async_step_modes(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        defaults = self.data
        if user_input is not None:
            # An optional field left empty is absent from the input: clear the old value.
            optional = (CONF_MODE_ENTITY, CONF_CONTROL_SCRIPT)
            candidate = {k: v for k, v in self.data.items() if k not in optional}
            candidate.update(user_input)
            mode_entity = candidate.get(CONF_MODE_ENTITY)
            options = _mode_options(self.hass, mode_entity) if mode_entity else None
            errors = ha("check_modes", cfg=candidate, mode_options=options)
            if not errors:
                self.data = candidate
                if mode_entity:
                    return await self.async_step_compare()
                self.data[CONF_MODE_MAP] = {}
                return await self._next("modes")
            defaults = candidate
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="modes", data_schema=_modes_schema(defaults), errors=errors
        )

    async def async_step_compare(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        options = _mode_options(self.hass, self.data.get(CONF_MODE_ENTITY))
        if not options:  # the entity lost its options after the modes step
            return self.async_abort(reason="no_mode_options")  # type: ignore[attr-defined]
        modes = ha("enabled_modes", cfg=self.data)
        if user_input is not None:
            self.data[CONF_MODE_MAP] = ha("mode_map", cfg=self.data, input=user_input)
            return await self._next("compare")
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="compare",
            data_schema=_compare_schema(modes, options, self.data.get(CONF_MODE_MAP, {})),
            description_placeholders={"entity": self.data[CONF_MODE_ENTITY]},
        )

    def _calibration(self) -> Any:
        """The solar calibration of the loaded entry (options flow only)."""
        return None

    async def async_step_solar(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        options = ["solar_system", "solar_json"]
        placeholders = {"calibration": ""}
        calibration = self._calibration()
        proposal = calibration.proposal if calibration else None
        if proposal:
            options.insert(0, "solar_apply")
            placeholders["calibration"] = "\n\n**Calibration result**\n\n" + ha(
                "summarize", planes=self.data.get(CONF_PLANES, []), result=proposal
            )
        return self.async_show_menu(  # type: ignore[attr-defined]
            step_id="solar", menu_options=options, description_placeholders=placeholders
        )

    async def async_step_solar_system(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        defaults = ha("form_defaults", planes=self.data.get(CONF_PLANES, []))
        if user_input is not None:
            self._array_count = int(user_input[CONF_ARRAY_COUNT])
            self._inverter_w = float(user_input[CONF_INVERTER_W])
            self._prefill = defaults["arrays"]
            self._new_planes: list[dict[str, Any]] = []
            return await self.async_step_solar_array()
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="solar_system", data_schema=_solar_system_schema(defaults["count"], defaults["inverter_w"])
        )

    async def async_step_solar_array(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        i = len(self._new_planes)
        planes = self.data.get(CONF_PLANES, [])
        existing = planes[i] if ha("fits_form", planes=planes) and i < len(planes) else None
        defaults = self._prefill[i] if i < len(self._prefill) else C["default_array"]
        errors: dict[str, str] = {}
        if user_input is not None:
            try:
                plane = ha("plane_from_array", array=user_input, inverter_w=self._inverter_w, existing=existing)
            except ValueError:
                errors[CONF_DIRECTION] = "invalid_direction"
                defaults = user_input
            else:
                self._new_planes.append(plane)
                if len(self._new_planes) < self._array_count:
                    return await self.async_step_solar_array()
                self.data[CONF_PLANES] = self._new_planes
                self.data[CONF_SOLAR_SETUP] = "form"
                return self._finish()
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="solar_array",
            data_schema=_solar_array_schema(defaults),
            errors=errors,
            description_placeholders={"index": str(i + 1), "count": str(self._array_count)},
        )

    async def async_step_solar_json(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            try:
                self.data[CONF_PLANES] = ha("parse_planes", text=user_input[CONF_PLANES])
            except ValueError:
                errors[CONF_PLANES] = "invalid_planes"
            else:
                self.data[CONF_SOLAR_SETUP] = "json"
                return self._finish()
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="solar_json", data_schema=_solar_json_schema(self.data), errors=errors
        )

    async def async_step_solar_apply(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        calibration = self._calibration()
        proposal = calibration.proposal if calibration else None
        if not proposal:
            return self.async_abort(reason="no_calibration")  # type: ignore[attr-defined]
        self.data[CONF_PLANES] = ha("apply_proposal", planes=self.data.get(CONF_PLANES, []), result=proposal)
        await calibration.async_mark_applied(self.data[CONF_PLANES])
        return self._finish()


class VulpesEmsConfigFlow(_Steps, ConfigFlow, domain=DOMAIN):
    """Handle a config flow for Vulpes EMS."""

    VERSION = 3  # core.C["config_version"]; async_migrate_entry migrates older entries

    def __init__(self) -> None:
        self.data: dict[str, Any] = {}

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        defaults = await energy_dashboard_defaults(self.hass) if user_input is None else {}
        return await self._sources("user", user_input, defaults)

    async def _next(self, after: str) -> ConfigFlowResult:
        """Setup walks through every step in order."""
        step = {
            "user": self.async_step_battery,
            "battery": self.async_step_tariff,
            "tariff": self.async_step_modes,
            "modes": self.async_step_solar,
            "compare": self.async_step_solar,
        }[after]
        return await step()

    def _finish(self) -> ConfigFlowResult:
        return self.async_create_entry(title="Vulpes EMS", data=self.data)

    @staticmethod
    @callback
    def async_get_options_flow(config_entry: ConfigEntry) -> OptionsFlow:
        return VulpesEmsOptionsFlow()


OPTIONS_MENU = ["sources", "battery", "tariff", "modes", "solar"]


class VulpesEmsOptionsFlow(_Steps, OptionsFlow):
    """Edit any setting after setup; the complete settings are stored as options."""

    def __init__(self) -> None:
        self.data: dict[str, Any] = {}

    async def async_step_init(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        if not self.data:
            self.data = ha("settings", data=dict(self.config_entry.data), options=dict(self.config_entry.options))
        return self.async_show_menu(step_id="init", menu_options=OPTIONS_MENU)

    async def async_step_sources(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        return await self._sources("sources", user_input, self.data)

    async def _next(self, after: str) -> ConfigFlowResult:
        """Each section is saved on its own: the full settings copy replaces the old one."""
        return self._finish()

    def _finish(self) -> ConfigFlowResult:
        return self.async_create_entry(data=self.data)

    def _calibration(self) -> Any:
        coordinator = getattr(self.config_entry, "runtime_data", None)
        return getattr(coordinator, "calibration", None)
