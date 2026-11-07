"""Vulpes EMS: forecasting and ESS mode optimization for Home Assistant.

Shadow mode: the integration only reads Home Assistant data and publishes its
own entities with the plan. It never changes any other entity.
"""

from __future__ import annotations

import logging

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant

from .coordinator import VulpesCoordinator
from .modes import migrate_v1

PLATFORMS = [Platform.SENSOR, Platform.BINARY_SENSOR]

_LOGGER = logging.getLogger(__name__)

type VulpesConfigEntry = ConfigEntry[VulpesCoordinator]


async def async_setup_entry(hass: HomeAssistant, entry: VulpesConfigEntry) -> bool:
    """Set up Vulpes EMS from a config entry."""
    coordinator = VulpesCoordinator(hass, entry)
    await coordinator.async_config_entry_first_refresh()
    coordinator.start_schedule()
    entry.runtime_data = coordinator
    entry.async_on_unload(entry.add_update_listener(_async_reload))
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_migrate_entry(hass: HomeAssistant, entry: VulpesConfigEntry) -> bool:
    """Version 1 (0.1.0) had user-named modes; version 2 uses Vulpes modes."""
    if entry.version > 2:
        return False
    if entry.version == 1:
        hass.config_entries.async_update_entry(
            entry, data=migrate_v1(dict(entry.data)), options=migrate_v1(dict(entry.options)), version=2
        )
        _LOGGER.info("Migrated Vulpes EMS settings to Vulpes modes")
    return True


async def _async_reload(hass: HomeAssistant, entry: VulpesConfigEntry) -> None:
    await hass.config_entries.async_reload(entry.entry_id)


async def async_unload_entry(hass: HomeAssistant, entry: VulpesConfigEntry) -> bool:
    """Unload a config entry."""
    entry.runtime_data.stop_schedule()
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
