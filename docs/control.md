# Active control

Vulpes starts in shadow mode: it plans and publishes sensors, but changes nothing. To let it control the battery, you give it **one script** and turn on its **Active** switch. Vulpes never calls inverter services itself. The script translates Vulpes's modes into whatever your inverter needs, so any inverter that Home Assistant can control will work.

## How it works

1. In the modes step, tick the modes your system can do and pick a **control script**.
2. Vulpes adds two entities: the **Active** switch (off by default) and the **Battery control** sensor (`off`, `following_plan` or `fallback`).
3. While Active is on, Vulpes runs `script.turn_on` with the variables below:
   - every 15 minutes, 20 s after the quarter hour, right after planning;
   - immediately when you turn Active on.

   The switch keeps its state across restarts.
4. When you turn Active off, Vulpes sends the safe command once, with `fallback: false`, so your inverter returns to normal self-consumption.

**Turn off your own battery automations before you turn Active on.** Otherwise they fight with Vulpes.

## Script variables

| Variable | Type | Meaning |
|---|---|---|
| `mode` | string | `charge`, `hold`, `self_consume`, `idle` or `discharge` |
| `pv_off` | bool | Switch solar off entirely |
| `no_export` | bool | Block export (zero export): solar may still power the house and charge the battery |
| `power_control` | string | `grid_setpoint` or `battery_power`, as set in the modes step |
| `power_w` | int | Charge or discharge power from the modes step, unsigned; 0 for the other modes |
| `soc_min`, `soc_max` | float | Battery limits from the battery step, in % |
| `planned_grid_w`, `planned_battery_w` | int or none | What the plan expects for this slot (grid positive = import, battery positive = charging); none in the safe command |
| `fallback` | bool | True when Vulpes has no usable plan and sends the safe command |
| `until` | string | End of the slot (ISO 8601, UTC) |

The mode values stay in English whatever the Home Assistant language: they are an interface, not labels.

| Mode | Meaning |
|---|---|
| `charge` | Charge from the grid. With `grid_setpoint`, the grid import is held at `power_w`; with `battery_power`, the battery charges at `power_w`. |
| `hold` | Never discharge; solar surplus may still charge the battery. |
| `self_consume` | The battery keeps the grid at zero: the inverter's normal self-use mode. |
| `idle` | The battery neither charges nor discharges. |
| `discharge` | Discharge to the grid. With `grid_setpoint`, the grid export is held at `power_w`; with `battery_power`, the battery discharges at `power_w`. |

## Safety

- **Safe command:** `mode: self_consume`, `pv_off: false`, `no_export: false`, `fallback: true`. Vulpes sends it instead of the plan in these cases:
  - planning failed (for example the SOC sensor is unavailable);
  - the plan is more than 30 minutes old;
  - the plan has no slot for the current time.
- **Errors:** a failing script is logged and shown in the `error` attribute of the Battery control sensor. The next slot tries again.
- **Inverter failsafe:** if Home Assistant itself stops, Vulpes can't send anything. Use the inverter's own failsafe or timeout where it has one, set longer than 15 minutes, so that it returns to normal operation by itself.
- **Persistent settings:** settings that the inverter's failsafe doesn't revert (such as an export switch) stay as the last command left them.

## Example: Solis hybrid inverter with `solis_modbus`

The `solis_modbus` integration has a remote dispatch action with an inverter-side failsafe. Replace the switch entity with your own. Make sure each power stays within your inverter's limits and Vulpes's battery and grid settings.

```yaml
vulpes_apply:
  alias: Vulpes control
  mode: restart
  sequence:
    # Zero export via the inverter's export switch (not reverted by the failsafe).
    - action: "switch.turn_{{ 'off' if no_export else 'on' }}"
      target:
        entity_id: switch.solis_allow_export
    - choose:
        - conditions: "{{ mode == 'charge' }}"
          sequence:
            - action: solis_modbus.solis_dispatch
              data:
                mode: "{{ 'grid_import' if power_control == 'grid_setpoint' else 'battery_charge' }}"
                power_watts: "{{ power_w }}"
                allow_grid_charge: true
                soc_max: "{{ soc_max | int }}"
                pv_shutdown: "{{ pv_off }}"
                failsafe_minutes: 20
        - conditions: "{{ mode == 'discharge' }}"
          sequence:
            - action: solis_modbus.solis_dispatch
              data:
                mode: "{{ 'grid_export' if power_control == 'grid_setpoint' else 'battery_discharge' }}"
                power_watts: "{{ power_w }}"
                soc_min: "{{ soc_min | int }}"
                pv_shutdown: "{{ pv_off }}"
                failsafe_minutes: 20
        - conditions: "{{ mode == 'hold' }}"
          sequence:
            - action: solis_modbus.solis_dispatch
              data:
                mode: self_consumption
                disable_discharge: true
                pv_shutdown: "{{ pv_off }}"
                failsafe_minutes: 20
        - conditions: "{{ mode == 'idle' }}"
          sequence:
            - action: solis_modbus.solis_dispatch
              data:
                mode: battery_hold
                pv_shutdown: "{{ pv_off }}"
                failsafe_minutes: 20
        - conditions: "{{ mode == 'self_consume' and pv_off }}"
          sequence:
            - action: solis_modbus.solis_dispatch
              data:
                mode: self_consumption
                pv_shutdown: true
                failsafe_minutes: 20
      # self_consume: back to the inverter's own self-use mode.
      default:
        - action: solis_modbus.solis_dispatch_stop
```

Notes:
- The dispatch action leaves `pv_shutdown` unchanged when it's omitted, so the script always passes it.
- After the first `solis_dispatch_stop`, check that the inverter's normal settings are back, including solar power.
