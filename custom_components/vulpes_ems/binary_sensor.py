"""Shadow-mode binary sensor: does the current mode match the plan?"""

from __future__ import annotations

from homeassistant.components.binary_sensor import BinarySensorEntity
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import VulpesConfigEntry
from .coordinator import VulpesCoordinator
from .sensor import device_info


async def async_setup_entry(
    hass: HomeAssistant, entry: VulpesConfigEntry, async_add_entities: AddConfigEntryEntitiesCallback
) -> None:
    async_add_entities([AgreesWithPlan(entry.runtime_data, entry)])


class AgreesWithPlan(CoordinatorEntity[VulpesCoordinator], BinarySensorEntity):
    """On when the current ESS mode equals the planned mode."""

    _attr_has_entity_name = True
    _attr_translation_key = "agrees_with_plan"

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_agrees_with_plan"
        self._attr_device_info = device_info(entry)

    @property
    def is_on(self) -> bool | None:
        return self.coordinator.agrees_now
