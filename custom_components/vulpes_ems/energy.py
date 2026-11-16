"""Energy platform: the solar forecast for Home Assistant's Energy dashboard."""

from __future__ import annotations

from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.util import dt as dt_util


async def async_get_solar_forecast(hass: HomeAssistant, config_entry_id: str) -> dict[str, dict[str, float]] | None:
    """The plan's hourly solar forecast (Wh per hour, before curtailment), keyed by the hour's start."""
    entry = hass.config_entries.async_get_entry(config_entry_id)
    if entry is None or entry.state is not ConfigEntryState.LOADED:
        return None
    hours = (entry.runtime_data.data or {}).get("pv_hourly")
    if not hours:
        return None
    return {"wh_hours": {dt_util.utc_from_timestamp(start).isoformat(): wh for start, wh in hours}}
