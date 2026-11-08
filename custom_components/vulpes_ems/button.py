"""Button: calibrate the solar arrays from recorder history."""

from __future__ import annotations

from homeassistant.components.button import ButtonEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback

from . import VulpesConfigEntry
from .calibration import SolarCalibration
from .sensor import device_info


async def async_setup_entry(
    hass: HomeAssistant, entry: VulpesConfigEntry, async_add_entities: AddConfigEntryEntitiesCallback
) -> None:
    async_add_entities([CalibrateSolarButton(entry.runtime_data.calibration, entry)])


class CalibrateSolarButton(ButtonEntity):
    """Fit the solar arrays to history; the result is proposed in the options flow."""

    _attr_has_entity_name = True
    _attr_translation_key = "calibrate_solar"
    _attr_entity_category = EntityCategory.CONFIG

    def __init__(self, calibration: SolarCalibration, entry: VulpesConfigEntry) -> None:
        self._calibration = calibration
        self._attr_unique_id = f"{entry.entry_id}_calibrate_solar"
        self._attr_device_info = device_info(entry)

    async def async_press(self) -> None:
        await self._calibration.async_run()
