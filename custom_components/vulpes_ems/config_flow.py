"""Config flow for Vulpes EMS (entity/plane/mode setup arrives in later phases)."""

from __future__ import annotations

from typing import Any

from homeassistant.config_entries import ConfigFlow, ConfigFlowResult

from .const import DOMAIN


class VulpesEmsConfigFlow(ConfigFlow, domain=DOMAIN):
    """Handle a config flow for Vulpes EMS."""

    VERSION = 1

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        """Create the (single) entry."""
        if user_input is not None:
            return self.async_create_entry(title="Vulpes EMS", data={})
        return self.async_show_form(step_id="user")
