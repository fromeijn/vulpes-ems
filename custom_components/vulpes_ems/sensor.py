"""Shadow-mode sensors: planned mode with the full plan, forecasts and agreement."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorEntityDescription,
    SensorStateClass,
)
from homeassistant.const import PERCENTAGE, EntityCategory, UnitOfEnergy, UnitOfPower
from homeassistant.core import HomeAssistant
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from homeassistant.util import dt as dt_util

from . import VulpesConfigEntry
from .const import CONF_MODE_ENTITY, CONF_PLANES, DOMAIN
from .coordinator import VulpesCoordinator
from .core import C, ha


@dataclass(frozen=True, kw_only=True)
class VulpesSensorDescription(SensorEntityDescription):
    value: Callable[[VulpesCoordinator], Any]


SENSORS = (
    VulpesSensorDescription(
        key="planned_mode",
        translation_key="planned_mode",
        device_class=SensorDeviceClass.ENUM,
        value=lambda c: c.data["summary"]["mode_now"],
    ),
    VulpesSensorDescription(
        key="planned_battery_power",
        translation_key="planned_battery_power",
        device_class=SensorDeviceClass.POWER,
        native_unit_of_measurement=UnitOfPower.WATT,
        suggested_display_precision=0,
        value=lambda c: round(c.data["slots"][0]["battery_w"]) if c.data["slots"] else None,
    ),
    VulpesSensorDescription(
        key="planned_grid_power",
        translation_key="planned_grid_power",
        device_class=SensorDeviceClass.POWER,
        native_unit_of_measurement=UnitOfPower.WATT,
        suggested_display_precision=0,
        value=lambda c: round(c.data["slots"][0]["grid_w"]) if c.data["slots"] else None,
    ),
    VulpesSensorDescription(
        key="pv_forecast_24h",
        translation_key="pv_forecast_24h",
        device_class=SensorDeviceClass.ENERGY,
        native_unit_of_measurement=UnitOfEnergy.KILO_WATT_HOUR,
        suggested_display_precision=1,
        value=lambda c: c.data["summary"]["pv_kwh_24h"],
    ),
    VulpesSensorDescription(
        key="pv_today",
        translation_key="pv_today",
        device_class=SensorDeviceClass.ENERGY,
        native_unit_of_measurement=UnitOfEnergy.KILO_WATT_HOUR,
        suggested_display_precision=1,
        value=lambda c: c.data["summary"].get("pv_kwh_today"),
    ),
    VulpesSensorDescription(
        key="pv_remaining_today",
        translation_key="pv_remaining_today",
        device_class=SensorDeviceClass.ENERGY,
        native_unit_of_measurement=UnitOfEnergy.KILO_WATT_HOUR,
        suggested_display_precision=1,
        value=lambda c: c.data["summary"].get("pv_kwh_today_remaining"),
    ),
    VulpesSensorDescription(
        key="pv_tomorrow",
        translation_key="pv_tomorrow",
        device_class=SensorDeviceClass.ENERGY,
        native_unit_of_measurement=UnitOfEnergy.KILO_WATT_HOUR,
        suggested_display_precision=1,
        value=lambda c: c.data["summary"].get("pv_kwh_tomorrow"),
    ),
    VulpesSensorDescription(
        key="pv_day_after",
        translation_key="pv_day_after",
        device_class=SensorDeviceClass.ENERGY,
        native_unit_of_measurement=UnitOfEnergy.KILO_WATT_HOUR,
        suggested_display_precision=1,
        value=lambda c: c.data["summary"].get("pv_kwh_day_after"),
    ),
    VulpesSensorDescription(
        key="load_forecast_24h",
        translation_key="load_forecast_24h",
        device_class=SensorDeviceClass.ENERGY,
        native_unit_of_measurement=UnitOfEnergy.KILO_WATT_HOUR,
        suggested_display_precision=1,
        value=lambda c: c.data["summary"]["load_kwh_24h"],
    ),
    VulpesSensorDescription(
        key="agreement_today",
        translation_key="agreement_today",
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
        suggested_display_precision=0,
        value=lambda c: c.agreement_pct,
    ),
)


def _r(c: VulpesCoordinator, *path: str) -> Any:
    """A value from the review, or None while there is none."""
    node: Any = c.review
    for key in path:
        if not isinstance(node, dict):
            return None
        node = node.get(key)
    return node


def _window(c: VulpesCoordinator, field: str) -> dict[str, Any]:
    w7 = _r(c, "windows", "days_7") or {}
    return {
        "yesterday": _r(c, "windows", "yesterday", field),
        "last_30_days": _r(c, "windows", "days_30", field),
        "days": w7.get("days"),
        "cost_self_consumption": w7.get("cost_baseline"),
        "cost_actual": w7.get("cost_actual"),
        "cost_plan": w7.get("cost_plan"),
        "active_share_pct": w7.get("active_share_pct"),
    }


def _forecast(c: VulpesCoordinator, kind: str) -> dict[str, Any]:
    f = _r(c, "windows", "days_7", kind) or {}
    return {
        "bias_pct": f.get("bias_pct"),
        "day_ahead": f.get("day_ahead"),
        "latest": f.get("latest"),
        "persistence": f.get("persistence"),
        "skill": f.get("skill"),
        "hours": f.get("hours"),
        "mean_w": f.get("mean_w"),
        "last_30_days_pct": _r(c, "windows", "days_30", kind, "error_pct"),
    }


@dataclass(frozen=True, kw_only=True)
class ReviewSensorDescription(SensorEntityDescription):
    value: Callable[[VulpesCoordinator], Any]
    attrs: Callable[[VulpesCoordinator], dict[str, Any]] = lambda c: {}
    needs: str | None = None  # "planes" or "script": only created with those


REVIEW_SENSORS = (
    ReviewSensorDescription(
        key="saved_7d",
        translation_key="saved_7d",
        device_class=SensorDeviceClass.MONETARY,
        suggested_display_precision=2,
        value=lambda c: _r(c, "windows", "days_7", "saved_actual"),
        attrs=lambda c: _window(c, "saved_actual"),
    ),
    ReviewSensorDescription(
        key="plan_saved_7d",
        translation_key="plan_saved_7d",
        device_class=SensorDeviceClass.MONETARY,
        suggested_display_precision=2,
        value=lambda c: _r(c, "windows", "days_7", "saved_plan"),
        attrs=lambda c: _window(c, "saved_plan"),
    ),
    ReviewSensorDescription(
        key="total_saved",
        translation_key="total_saved",
        device_class=SensorDeviceClass.MONETARY,
        state_class=SensorStateClass.TOTAL,
        suggested_display_precision=2,
        value=lambda c: _r(c, "totals", "saved_actual"),
        attrs=lambda c: {"since": _r(c, "totals", "since"), "plan_total": _r(c, "totals", "saved_plan")},
    ),
    ReviewSensorDescription(
        key="solar_forecast_error",
        translation_key="solar_forecast_error",
        entity_category=EntityCategory.DIAGNOSTIC,
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
        suggested_display_precision=0,
        value=lambda c: _r(c, "windows", "days_7", "solar", "error_pct"),
        attrs=lambda c: _forecast(c, "solar"),
        needs="planes",
    ),
    ReviewSensorDescription(
        key="load_forecast_error",
        translation_key="load_forecast_error",
        entity_category=EntityCategory.DIAGNOSTIC,
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
        suggested_display_precision=0,
        value=lambda c: _r(c, "windows", "days_7", "load", "error_pct"),
        attrs=lambda c: _forecast(c, "load"),
    ),
    ReviewSensorDescription(
        key="control_uptime",
        translation_key="control_uptime",
        entity_category=EntityCategory.DIAGNOSTIC,
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
        suggested_display_precision=0,
        value=lambda c: _r(c, "windows", "days_7", "control", "uptime_pct"),
        attrs=lambda c: {
            k: _r(c, "windows", "days_7", "control", k) for k in ("slots_active", "slots_followed")
        },
        needs="script",
    ),
    ReviewSensorDescription(
        key="soc_tracking_error",
        translation_key="soc_tracking_error",
        entity_category=EntityCategory.DIAGNOSTIC,
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
        suggested_display_precision=1,
        value=lambda c: _r(c, "windows", "days_7", "control", "soc_mae_pct"),
        attrs=lambda c: {
            "bias_pct": _r(c, "windows", "days_7", "control", "soc_bias_pct"),
            "hours": _r(c, "windows", "days_7", "control", "soc_hours"),
        },
        needs="script",
    ),
)


def remove_stale(hass: HomeAssistant, entry: VulpesConfigEntry, domain: str, keys: set[str]) -> None:
    """Remove this platform's entities for features that are now switched off."""
    registry = er.async_get(hass)
    for reg in er.async_entries_for_config_entry(registry, entry.entry_id):
        key = reg.unique_id.removeprefix(f"{entry.entry_id}_")
        if reg.domain == domain and key in keys:
            registry.async_remove(reg.entity_id)


def currency(coordinator: VulpesCoordinator, description: SensorEntityDescription) -> str | None:
    """Money in Home Assistant's currency, the one the tariffs are entered in."""
    if description.device_class == SensorDeviceClass.MONETARY:
        return coordinator.hass.config.currency
    return description.native_unit_of_measurement


def device_info(entry: VulpesConfigEntry) -> DeviceInfo:
    return DeviceInfo(identifiers={(DOMAIN, entry.entry_id)}, name="Vulpes EMS", manufacturer="vulpes-ems")


async def async_setup_entry(
    hass: HomeAssistant, entry: VulpesConfigEntry, async_add_entities: AddConfigEntryEntitiesCallback
) -> None:
    coordinator = entry.runtime_data
    compare = bool(coordinator.cfg.get(CONF_MODE_ENTITY))
    # The Plan cost sensor was removed in 0.7.0: take it out of existing installs.
    remove_stale(hass, entry, "sensor", {"plan_cost"})
    if not compare:
        remove_stale(hass, entry, "sensor", {"agreement_today"})
    entities: list[SensorEntity] = [
        VulpesSensor(coordinator, entry, d) for d in SENSORS if compare or d.key != "agreement_today"
    ]
    entities.append(SolarCalibrationSensor(coordinator, entry))
    entities += [StatusSensor(coordinator, entry), LastPlanSensor(coordinator, entry), EnergyDataSensor(coordinator, entry)]
    has = {"planes": bool(coordinator.cfg.get(CONF_PLANES)), "script": bool(coordinator.control_script)}
    for d in REVIEW_SENSORS:
        if d.needs is None or has[d.needs]:
            entities.append(ReviewSensor(coordinator, entry, d))
        else:
            remove_stale(hass, entry, "sensor", {d.key})
    if coordinator.control_script:
        entities.append(ControlSensor(coordinator, entry))
    else:
        remove_stale(hass, entry, "sensor", {"control"})
    async_add_entities(entities)


class VulpesSensor(CoordinatorEntity[VulpesCoordinator], SensorEntity):
    """A sensor derived from the latest plan."""

    _attr_has_entity_name = True
    # The full plan is large; keep it out of the recorder database.
    _unrecorded_attributes = frozenset({"plan", "forecast"})
    entity_description: VulpesSensorDescription

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry, description) -> None:
        super().__init__(coordinator)
        self.entity_description = description
        self._attr_unique_id = f"{entry.entry_id}_{description.key}"
        self._attr_device_info = device_info(entry)
        self._attr_native_unit_of_measurement = currency(coordinator, description)
        if description.device_class == SensorDeviceClass.ENUM:
            self._attr_options = ha("enabled_modes", cfg=coordinator.cfg)

    @property
    def available(self) -> bool:
        # Unavailable until the first plan, which runs after setup.
        return super().available and self.coordinator.data is not None

    @property
    def native_value(self) -> Any:
        return self.entity_description.value(self.coordinator)

    @property
    def extra_state_attributes(self) -> dict[str, Any] | None:
        data = self.coordinator.data
        if data is None:
            return None
        key = self.entity_description.key
        iso = lambda t: dt_util.utc_from_timestamp(t).isoformat()  # noqa: E731
        if key == "planned_mode":
            return {
                "plan": [
                    {
                        "start": iso(s["start"]),
                        "mode": s["mode"],
                        "pv_off": s["pv_off"],
                        "no_export": s["no_export"],
                        "soc": round(s["soc_pct"], 1),
                        "battery_w": round(s["battery_w"]),
                        "grid_w": round(s["grid_w"]),
                        "pv_w": round(s["pv_w"]),
                        "load_w": round(s["load_w"]),
                        "import_price": round(s["import_price"], 4),
                        "export_price": round(s["export_price"], 4),
                        "price_known": s["price_known"],
                    }
                    for s in data["slots"]
                ],
                "warnings": data["warnings"],
                "inputs": data.get("inputs", {}),
                "engine_version": data["version"],
            }
        if key in ("pv_forecast_24h", "load_forecast_24h"):
            field = "pv_w" if key == "pv_forecast_24h" else "load_w"
            return {"forecast": [{"start": iso(s["start"]), "w": round(s[field])} for s in data["slots"]]}
        if key == "agreement_today":
            return {"compared": self.coordinator.compared, "agreed": self.coordinator.agree}
        return None


class ControlSensor(CoordinatorEntity[VulpesCoordinator], SensorEntity):
    """Whether Vulpes controls the battery: off, following the plan, or in fallback."""

    _attr_has_entity_name = True
    _attr_translation_key = "control"
    _attr_device_class = SensorDeviceClass.ENUM
    _attr_options = C["control_states"]

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_control"
        self._attr_device_info = device_info(entry)

    @property
    def available(self) -> bool:
        return True  # most useful exactly when planning fails

    @property
    def native_value(self) -> str:
        c = self.coordinator
        return ha("control_state", active=c.active, last=c.last_command, error=c.last_error)

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        c = self.coordinator
        attrs: dict[str, Any] = {"script": c.control_script, "last_command": c.last_command}
        if c.last_error:
            attrs["error"] = c.last_error
        return attrs


class StatusSensor(CoordinatorEntity[VulpesCoordinator], SensorEntity):
    """Is Vulpes working: ok, degraded or problem, with every check in the attributes."""

    _attr_has_entity_name = True
    _attr_translation_key = "status"
    _attr_device_class = SensorDeviceClass.ENUM
    _attr_options = ["ok", "degraded", "problem"]

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_status"
        self._attr_device_info = device_info(entry)

    @property
    def available(self) -> bool:
        return True  # most useful exactly when planning fails

    @property
    def native_value(self) -> str:
        return self.coordinator.health["status"]

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        h = self.coordinator.health
        return {
            "checks": h["checks"],
            "problems": h["problems"],
            "review_warnings": (self.coordinator.review or {}).get("warnings", []),
        }


class _TimestampSensor(CoordinatorEntity[VulpesCoordinator], SensorEntity):
    _attr_has_entity_name = True
    _attr_device_class = SensorDeviceClass.TIMESTAMP
    _attr_entity_category = EntityCategory.DIAGNOSTIC
    _key = ""

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_translation_key = self._key
        self._attr_unique_id = f"{entry.entry_id}_{self._key}"
        self._attr_device_info = device_info(entry)

    @property
    def available(self) -> bool:
        return True

    def _ts(self) -> float | None:
        raise NotImplementedError

    @property
    def native_value(self) -> Any:
        ts = self._ts()
        return dt_util.utc_from_timestamp(ts) if ts is not None else None


class LastPlanSensor(_TimestampSensor):
    """When the last plan was made."""

    _key = "last_plan"

    def _ts(self) -> float | None:
        return self.coordinator.last_success_ts


class EnergyDataSensor(_TimestampSensor):
    """The end of the newest hour of energy statistics in the history file."""

    _key = "newest_energy_data"

    def _ts(self) -> float | None:
        return self.coordinator.energy_until


class ReviewSensor(CoordinatorEntity[VulpesCoordinator], SensorEntity):
    """A performance figure from the review of the history file."""

    _attr_has_entity_name = True
    entity_description: ReviewSensorDescription

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry, description) -> None:
        super().__init__(coordinator)
        self.entity_description = description
        self._attr_unique_id = f"{entry.entry_id}_{description.key}"
        self._attr_device_info = device_info(entry)
        self._attr_native_unit_of_measurement = currency(coordinator, description)

    @property
    def available(self) -> bool:
        # The review comes from the history file, not from the latest plan.
        return self.coordinator.review is not None

    @property
    def native_value(self) -> Any:
        return self.entity_description.value(self.coordinator)

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        return self.entity_description.attrs(self.coordinator)


class SolarCalibrationSensor(SensorEntity):
    """How much the latest solar calibration improves the forecast (%), with the proposal."""

    _attr_has_entity_name = True
    _attr_translation_key = "solar_calibration"
    _attr_entity_category = EntityCategory.DIAGNOSTIC
    _attr_native_unit_of_measurement = PERCENTAGE
    _attr_suggested_display_precision = 0
    _unrecorded_attributes = frozenset({"proposed", "fits", "score_current", "score_proposed", "warnings"})

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry) -> None:
        self._calibration = coordinator.calibration
        self._attr_unique_id = f"{entry.entry_id}_solar_calibration"
        self._attr_device_info = device_info(entry)

    async def async_added_to_hass(self) -> None:
        self.async_on_remove(self._calibration.async_add_listener(self.async_write_ha_state))

    @property
    def native_value(self) -> float | None:
        return self._calibration.improvement()

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        c = self._calibration
        attrs: dict[str, Any] = {"status": c.status, "last_run": c.last_run}
        if c.error:
            attrs["error"] = c.error
        result = c.result
        if result:
            attrs.update(
                held_out=result.get("held_out"),
                proposed=[
                    {"azimuth": round(p["azimuth"]), "tilt": round(p["tilt"]), "kwp": round(p["pdc0"] / 1000, 2)}
                    for p in result["planes"]
                ],
                fits=result.get("fits"),
                score_current=result.get("score_current"),
                score_proposed=result.get("score_proposed"),
                warnings=result.get("warnings"),
            )
        return attrs
