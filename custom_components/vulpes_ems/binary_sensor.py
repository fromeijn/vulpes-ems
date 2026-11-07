"""Binary sensors: planned solar curtailment, and whether the current mode matches the plan."""

from __future__ import annotations

from homeassistant.components.binary_sensor import BinarySensorEntity
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import VulpesConfigEntry
from .coordinator import VulpesCoordinator
from .modes import CONF_MODE_ENTITY, CONF_PV_CURTAIL
from .sensor import device_info, remove_stale


async def async_setup_entry(
    hass: HomeAssistant, entry: VulpesConfigEntry, async_add_entities: AddConfigEntryEntitiesCallback
) -> None:
    coordinator = entry.runtime_data
    entities: list[BinarySensorEntity] = []
    if coordinator.cfg.get(CONF_PV_CURTAIL):
        entities.append(PvOffPlanned(coordinator, entry))
    else:
        remove_stale(hass, entry, "binary_sensor", {"pv_off_planned"})
    if coordinator.cfg.get(CONF_MODE_ENTITY):
        entities.append(AgreesWithPlan(coordinator, entry))
    else:
        remove_stale(hass, entry, "binary_sensor", {"agrees_with_plan"})
    async_add_entities(entities)


class PvOffPlanned(CoordinatorEntity[VulpesCoordinator], BinarySensorEntity):
    """On when the plan switches solar off now."""

    _attr_has_entity_name = True
    _attr_translation_key = "pv_off_planned"

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_pv_off_planned"
        self._attr_device_info = device_info(entry)

    @property
    def is_on(self) -> bool | None:
        return self.coordinator.data["summary"].get("pv_off_now")


class AgreesWithPlan(CoordinatorEntity[VulpesCoordinator], BinarySensorEntity):
    """On when the user's mode entity is in the state mapped to the planned mode."""

    _attr_has_entity_name = True
    _attr_translation_key = "agrees_with_plan"

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_agrees_with_plan"
        self._attr_device_info = device_info(entry)

    @property
    def is_on(self) -> bool | None:
        return self.coordinator.agrees_now
