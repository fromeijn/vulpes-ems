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
from homeassistant.const import PERCENTAGE, UnitOfEnergy, UnitOfPower
from homeassistant.core import HomeAssistant
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from homeassistant.util import dt as dt_util

from . import VulpesConfigEntry
from .const import DOMAIN
from .coordinator import VulpesCoordinator
from .modes import CONF_MODE_ENTITY, enabled_modes


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
        key="load_forecast_24h",
        translation_key="load_forecast_24h",
        device_class=SensorDeviceClass.ENERGY,
        native_unit_of_measurement=UnitOfEnergy.KILO_WATT_HOUR,
        suggested_display_precision=1,
        value=lambda c: c.data["summary"]["load_kwh_24h"],
    ),
    VulpesSensorDescription(
        key="plan_cost",
        translation_key="plan_cost",
        device_class=SensorDeviceClass.MONETARY,
        native_unit_of_measurement="EUR",
        suggested_display_precision=2,
        value=lambda c: c.data["summary"]["cost"],
    ),
    VulpesSensorDescription(
        key="agreement_today",
        translation_key="agreement_today",
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
        suggested_display_precision=0,
        value=lambda c: round(100.0 * c.agree / c.compared, 1) if c.compared else None,
    ),
)


def remove_stale(hass: HomeAssistant, entry: VulpesConfigEntry, domain: str, keys: set[str]) -> None:
    """Remove this platform's entities for features that are now switched off."""
    registry = er.async_get(hass)
    for reg in er.async_entries_for_config_entry(registry, entry.entry_id):
        key = reg.unique_id.removeprefix(f"{entry.entry_id}_")
        if reg.domain == domain and key in keys:
            registry.async_remove(reg.entity_id)


def device_info(entry: VulpesConfigEntry) -> DeviceInfo:
    return DeviceInfo(identifiers={(DOMAIN, entry.entry_id)}, name="Vulpes EMS", manufacturer="vulpes-ems")


async def async_setup_entry(
    hass: HomeAssistant, entry: VulpesConfigEntry, async_add_entities: AddConfigEntryEntitiesCallback
) -> None:
    coordinator = entry.runtime_data
    compare = bool(coordinator.cfg.get(CONF_MODE_ENTITY))
    if not compare:
        remove_stale(hass, entry, "sensor", {"agreement_today"})
    async_add_entities(
        VulpesSensor(coordinator, entry, d) for d in SENSORS if compare or d.key != "agreement_today"
    )


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
        if description.device_class == SensorDeviceClass.ENUM:
            self._attr_options = enabled_modes(coordinator.cfg)

    @property
    def native_value(self) -> Any:
        return self.entity_description.value(self.coordinator)

    @property
    def extra_state_attributes(self) -> dict[str, Any] | None:
        data = self.coordinator.data
        key = self.entity_description.key
        iso = lambda t: dt_util.utc_from_timestamp(t).isoformat()  # noqa: E731
        if key == "planned_mode":
            return {
                "plan": [
                    {
                        "start": iso(s["start"]),
                        "mode": s["mode"],
                        "pv_off": s["pv_off"],
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
