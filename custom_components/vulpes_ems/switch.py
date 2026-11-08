"""Switch: hand battery control to Vulpes (runs the control script every 15 minutes)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.switch import SwitchEntity
from homeassistant.const import STATE_ON
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.restore_state import RestoreEntity

from . import VulpesConfigEntry
from .coordinator import VulpesCoordinator
from .sensor import device_info, remove_stale


async def async_setup_entry(
    hass: HomeAssistant, entry: VulpesConfigEntry, async_add_entities: AddConfigEntryEntitiesCallback
) -> None:
    coordinator = entry.runtime_data
    if coordinator.control_script:
        async_add_entities([ActiveSwitch(coordinator, entry)])
    else:
        remove_stale(hass, entry, "switch", {"active"})


class ActiveSwitch(SwitchEntity, RestoreEntity):
    """On: Vulpes runs the control script every slot. Off (default): shadow mode."""

    _attr_has_entity_name = True
    _attr_translation_key = "active"

    def __init__(self, coordinator: VulpesCoordinator, entry: VulpesConfigEntry) -> None:
        self._coordinator = coordinator
        self._attr_unique_id = f"{entry.entry_id}_active"
        self._attr_device_info = device_info(entry)

    async def async_added_to_hass(self) -> None:
        last = await self.async_get_last_state()
        if last is not None and last.state == STATE_ON:
            await self._coordinator.async_set_active(True)

    @property
    def is_on(self) -> bool:
        return self._coordinator.active

    async def async_turn_on(self, **kwargs: Any) -> None:
        await self._coordinator.async_set_active(True)
        self.async_write_ha_state()

    async def async_turn_off(self, **kwargs: Any) -> None:
        await self._coordinator.async_set_active(False)
        self.async_write_ha_state()
