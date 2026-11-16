"""Calls into the Rust core (`vulpes_ems`): JSON in, JSON out.

All logic lives in Rust (`crates/vulpes-ha`); this module only converts
Home Assistant objects to JSON. `ha()` runs a pure op that takes
microseconds, so it may run on the event loop; `plan_ha` and
`calibrate_ha` fetch weather and run in the executor.
"""

from __future__ import annotations

from datetime import date, datetime
import json
from typing import Any

import vulpes_ems


def _default(value: Any) -> Any:
    if isinstance(value, (datetime, date)):
        return value.isoformat()
    if isinstance(value, (set, frozenset, tuple)):
        return list(value)
    return str(value)


def dumps(value: Any) -> str:
    """JSON for the Rust core; datetimes become ISO strings."""
    return json.dumps(value, default=_default)


def ha(op: str, **request: Any) -> Any:
    """Run a pure op of the Rust adapter and return its result."""
    return json.loads(vulpes_ems.ha(op, dumps(request)))


def stat_rows(stats: dict[str, list[dict[str, Any]]], ids: list[str] | set[str], field: str) -> dict[str, list]:
    """Recorder statistics rows -> `[start, value]` pairs for each of `ids` (empty when it has none)."""

    def start(row: dict[str, Any]) -> Any:
        value = row.get("start")
        return value.timestamp() if isinstance(value, datetime) else value

    return {i: [[start(r), r.get(field)] for r in stats.get(i, [])] for i in ids}


# Form options, defaults and history windows, as the core defines them.
C: dict[str, Any] = ha("constants")
