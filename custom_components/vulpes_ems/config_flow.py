"""Config flow for Vulpes EMS: sources, battery, tariff, modes, comparison, solar arrays.

The config flow walks through every step; the options flow is a menu of
sections, each saved on its own.
"""

from __future__ import annotations

from datetime import timedelta
import json
from typing import Any

import voluptuous as vol

from homeassistant.config_entries import ConfigEntry, ConfigFlow, ConfigFlowResult, OptionsFlow
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import selector
from homeassistant.util import dt as dt_util

from .const import (
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
    CONF_PLANES,
    CONF_PRICE_ENTITY,
    CONF_SOC_ENTITY,
    CONF_SOLAR_ENERGY,
    CONF_TARIFF_A,
    CONF_TARIFF_B,
    CONF_WEAR_COST,
    CONF_WHOLESALE_SOURCE,
    DOMAIN,
    LOAD_HISTORY_DAYS,
    WHOLESALE_SOURCES,
)
from .coordinator import async_energy_changes
from .modes import (
    CONF_CHARGE_W,
    CONF_CONTROL_SCRIPT,
    CONF_DISCHARGE_W,
    CONF_EXPORT_BLOCK,
    CONF_MODE_ENTITY,
    CONF_MODE_MAP,
    CONF_POWER_CONTROL,
    CONF_PV_CURTAIL,
    MODES,
    POWER_CONTROLS,
    enabled_modes,
    settings,
)
from .setup_checks import (
    check_battery,
    check_modes,
    dashboard_defaults,
    duplicate_statistics,
    energy_ids,
    load_groups,
    load_series,
    parse_price_attributes,
    soc_value,
)
from .solar_setup import (
    CONF_ARRAY_COUNT,
    CONF_DIRECTION,
    CONF_INVERTER_W,
    CONF_KWP,
    CONF_POWER_ENTITY,
    CONF_SOLAR_SETUP,
    CONF_TILT,
    DEFAULT_ARRAY,
    DIRECTIONS,
    MAX_ARRAYS,
    apply_proposal,
    fits_form,
    form_defaults,
    plane_from_array,
    summarize,
)

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
    return dashboard_defaults(prefs)


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
            selector.SelectSelectorConfig(options=POWER_CONTROLS, translation_key=CONF_POWER_CONTROL)
        ),
    }
    for key, default in MODES.items():
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
            vol.Required(CONF_ARRAY_COUNT, default=count): _num(1, MAX_ARRAYS, 1),
            vol.Required(CONF_INVERTER_W, default=inverter_w): _num(100, 100000, 1, "W"),
        }
    )


def _solar_array_schema(d: dict[str, Any]) -> vol.Schema:
    return vol.Schema(
        {
            vol.Required(CONF_DIRECTION, default=str(d.get(CONF_DIRECTION, "s"))): selector.SelectSelector(
                selector.SelectSelectorConfig(
                    options=list(DIRECTIONS),
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
        errors: dict[str, str] = {}
        placeholders = {"statistic": ""}
        soc = self.hass.states.get(d[CONF_SOC_ENTITY])
        if soc_value(soc.state if soc else None) is None:
            errors[CONF_SOC_ENTITY] = "soc_unavailable"
        price = self.hass.states.get(d[CONF_PRICE_ENTITY])
        unit = price.attributes.get("unit_of_measurement") if price else None
        if not price or not parse_price_attributes(dict(price.attributes), unit):
            errors[CONF_PRICE_ENTITY] = "no_prices"
        if not d.get(CONF_GRID_IMPORT):
            errors[CONF_GRID_IMPORT] = "grid_import_required"
            return errors, placeholders
        if duplicates := duplicate_statistics(d):
            errors["base"] = "duplicate_statistic"
            placeholders["statistic"] = ", ".join(duplicates)
            return errors, placeholders
        ids = energy_ids(d)
        start = (dt_util.utcnow() - timedelta(days=LOAD_HISTORY_DAYS)).replace(minute=0, second=0, microsecond=0)
        stats = await async_energy_changes(self.hass, set(ids), start)
        if empty := [i for i in ids if not stats.get(i)]:
            errors["base"] = "no_statistics"
            placeholders["statistic"] = ", ".join(empty)
        elif not load_series(load_groups(d), stats):
            errors["base"] = "no_complete_hours"
        return errors, placeholders

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
            errors = check_battery(candidate)
            if not errors:
                self.data = candidate
                return await self._next("battery")
            defaults = candidate
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="battery", data_schema=_battery_schema(defaults, self._currency), errors=errors
        )

    async def async_step_tariff(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        if user_input is not None:
            self.data.update(user_input)
            return await self._next("tariff")
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="tariff", data_schema=_tariff_schema(self.data, self._currency)
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
            if not enabled_modes(candidate):
                errors["base"] = "no_modes"
            else:
                errors = check_modes(candidate)
            if not errors and mode_entity and not _mode_options(self.hass, mode_entity):
                errors[CONF_MODE_ENTITY] = "no_mode_options"
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
        modes = enabled_modes(self.data)
        if user_input is not None:
            self.data[CONF_MODE_MAP] = {k: v for k, v in user_input.items() if k in modes and v}
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
            placeholders["calibration"] = "\n\n**Calibration result**\n\n" + summarize(
                self.data.get(CONF_PLANES, []), proposal
            )
        return self.async_show_menu(  # type: ignore[attr-defined]
            step_id="solar", menu_options=options, description_placeholders=placeholders
        )

    async def async_step_solar_system(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        count, inverter_w, arrays = form_defaults(self.data.get(CONF_PLANES, []))
        if user_input is not None:
            self._array_count = int(user_input[CONF_ARRAY_COUNT])
            self._inverter_w = float(user_input[CONF_INVERTER_W])
            self._prefill = arrays
            self._new_planes: list[dict[str, Any]] = []
            return await self.async_step_solar_array()
        return self.async_show_form(  # type: ignore[attr-defined]
            step_id="solar_system", data_schema=_solar_system_schema(count, inverter_w)
        )

    async def async_step_solar_array(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        i = len(self._new_planes)
        planes = self.data.get(CONF_PLANES, [])
        existing = planes[i] if fits_form(planes) and i < len(planes) else None
        defaults = self._prefill[i] if i < len(self._prefill) else DEFAULT_ARRAY
        errors: dict[str, str] = {}
        if user_input is not None:
            try:
                plane = plane_from_array(user_input, self._inverter_w, existing)
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
                self.data[CONF_PLANES] = parse_planes(user_input[CONF_PLANES])
            except (ValueError, KeyError, TypeError):
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
        self.data[CONF_PLANES] = apply_proposal(self.data.get(CONF_PLANES, []), proposal)
        await calibration.async_mark_applied(self.data[CONF_PLANES])
        return self._finish()


class VulpesEmsConfigFlow(_Steps, ConfigFlow, domain=DOMAIN):
    """Handle a config flow for Vulpes EMS."""

    VERSION = 3

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
            self.data = settings(self.config_entry.data, self.config_entry.options)
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
