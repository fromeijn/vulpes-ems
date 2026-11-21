"""Vulpes EMS: forecasting and ESS mode optimization for Home Assistant.

Setup returns at once and the first plan runs in the background once Home
Assistant has started, so its startup never waits for the network.

By default the integration runs in shadow mode: it only reads Home Assistant
data and publishes its own entities with the plan. Control is handed over
explicitly: the user picks a control script and turns the Active switch on,
after which Vulpes runs that script every 15 minutes (see `docs/control.md`).
The `update_plan` action plans at once, without waiting for the quarter hour.
All logic lives in the Rust core (`vulpes_ems`); this package is a thin shell.
"""

from __future__ import annotations

import logging
import shutil

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant, ServiceCall, callback
from homeassistant.exceptions import ServiceValidationError
from homeassistant.helpers import config_validation as cv
from homeassistant.helpers.start import async_at_started
from homeassistant.helpers.typing import ConfigType

from .calibration import SolarCalibration
from .const import DOMAIN
from .coordinator import VulpesCoordinator
from .core import C, ha

PLATFORMS = [Platform.SENSOR, Platform.BINARY_SENSOR, Platform.BUTTON, Platform.SWITCH]
SERVICE_UPDATE_PLAN = "update_plan"
# Set up from the UI only; `async_setup` is there for the action.
CONFIG_SCHEMA = cv.config_entry_only_config_schema(DOMAIN)

_LOGGER = logging.getLogger(__name__)

type VulpesConfigEntry = ConfigEntry[VulpesCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Register the `update_plan` action; it exists whether or not the entry is loaded."""

    async def update_plan(call: ServiceCall) -> None:
        entries: list[VulpesConfigEntry] = hass.config_entries.async_loaded_entries(DOMAIN)
        if not entries:
            raise ServiceValidationError(translation_domain=DOMAIN, translation_key="not_loaded")
        # A failing plan is logged and shows on the Status sensor, as for a scheduled run.
        await entries[0].runtime_data.async_update_plan()

    hass.services.async_register(DOMAIN, SERVICE_UPDATE_PLAN, update_plan)
    return True


async def async_setup_entry(hass: HomeAssistant, entry: VulpesConfigEntry) -> bool:
    """Set up Vulpes EMS from a config entry.

    Setup doesn't wait for the first plan: that needs the network, and Home
    Assistant's startup shouldn't. The plan starts once Home Assistant has
    started. The entities exist at once and show the plan when it's ready;
    the Status sensor says it's waiting until then.
    """
    coordinator = VulpesCoordinator(hass, entry)
    await coordinator.calibration.async_load()
    entry.async_on_unload(coordinator.calibration.async_unload)
    entry.runtime_data = coordinator
    coordinator.update_health()
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    entry.async_on_unload(entry.add_update_listener(_async_reload))

    @callback
    def _start(_hass: HomeAssistant) -> None:
        # After Home Assistant has started, so the SOC and price sensors of other
        # integrations have their states; at once on a reload. The task is cancelled on
        # unload, and the coordinator also stops between steps once it is closed.
        coordinator.start_schedule()
        entry.async_create_background_task(hass, coordinator.async_first_run(), "vulpes_ems first plan")

    entry.async_on_unload(async_at_started(hass, _start))
    return True


async def async_migrate_entry(hass: HomeAssistant, entry: VulpesConfigEntry) -> bool:
    """Bring older settings to the current version (1: user-named modes; 2: a net metering switch)."""
    current = C["config_version"]
    if entry.version > current:
        return False
    if entry.version < current:
        old = entry.version
        hass.config_entries.async_update_entry(
            entry,
            data=ha("migrate", cfg=dict(entry.data), version=old),
            options=ha("migrate", cfg=dict(entry.options), version=old),
            version=current,
        )
        _LOGGER.info("Migrated Vulpes EMS settings from version %d to %d", old, current)
    return True


async def _async_reload(hass: HomeAssistant, entry: VulpesConfigEntry) -> None:
    await hass.config_entries.async_reload(entry.entry_id)


async def async_unload_entry(hass: HomeAssistant, entry: VulpesConfigEntry) -> bool:
    """Unload a config entry; while Active, hand control back to the inverter first."""
    await entry.runtime_data.async_release()
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)


async def async_remove_entry(hass: HomeAssistant, entry: VulpesConfigEntry) -> None:
    """Delete the stored solar calibration and the history file with the entry."""
    await SolarCalibration(hass, entry).async_remove()
    folder = hass.config.path(DOMAIN, entry.entry_id)
    await hass.async_add_executor_job(lambda: shutil.rmtree(folder, ignore_errors=True))
