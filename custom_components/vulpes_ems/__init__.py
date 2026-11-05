"""Vulpes EMS: forecasting and ESS mode optimization for Home Assistant.

The heavy lifting (forecasts, optimizer, storage) lives in the Rust core,
shipped as the `vulpes-ems` wheel and imported as `vulpes_ems`.
"""

from __future__ import annotations

import logging

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant

_LOGGER = logging.getLogger(__name__)


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Set up Vulpes EMS from a config entry."""
    import vulpes_ems  # noqa: PLC0415  (wheel installed from manifest requirements)

    _LOGGER.info("Vulpes EMS core %s loaded", vulpes_ems.__version__)
    entry.runtime_data = None
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Unload a config entry."""
    return True
