# Active control

Vulpes starts in shadow mode: it plans and publishes sensors, but changes nothing. To let it control the battery, you give it **one script** and turn on its **Active** switch. Vulpes never calls inverter services itself. The script translates Vulpes's modes into whatever your inverter needs, so any inverter that Home Assistant can control will work.

## How it works

1. In the modes step, tick the modes your system can do and pick a **control script**.
2. Vulpes adds two entities: the **Active** switch (off by default) and the **Battery control** sensor (`off`, `following_plan`, `fallback`, `script_error`, or `waiting` while the first plan after a restart runs).
3. While Active is on, Vulpes runs the script with the variables below:
   - every 15 minutes, 20 s after the quarter hour, right after planning;
   - immediately when you turn Active on;
   - when the `vulpes_ems.update_plan` action is called (see [Update the plan now](#update-the-plan-now)).

   The switch keeps its state across restarts. After a restart Vulpes applies the first plan as soon as it is ready, usually within seconds of Home Assistant having started; it sends no safe command before it, and the Battery control sensor shows `waiting` until then. If that first plan fails, the safe command (`fallback: true`) is sent, as for any failed plan.
4. When you turn Active off, Vulpes sends the safe command once, with `fallback: false`, so your inverter returns to normal self-consumption. The same happens when the integration is unloaded while Active is on: for example when you change its settings, clear the script, or disable or delete the entry. After a settings change the switch comes back on and Vulpes applies the plan again straight away.

Vulpes waits for the script to finish, for at most 60 seconds. So keep it short: no delays or long waits.

**Turn off your own battery automations before you turn Active on.** Otherwise they fight with Vulpes.

## Update the plan now

Vulpes plans 20 s after every quarter hour. The action `vulpes_ems.update_plan` plans at once: it reads the prices and the state of charge as they are now, updates the plan and the sensors, and, while Active is on, runs the control script with the new plan. It has no fields. It also works in shadow mode.

A typical use is to plan as soon as tomorrow's prices arrive. Trigger on the attribute of your price sensor that changes at that moment; its name depends on the price integration:

```yaml
automation:
  - alias: Vulpes plans when tomorrow's prices arrive
    triggers:
      - trigger: state
        entity_id: sensor.electricity_price
        attribute: tomorrow_valid
        to: true
    actions:
      - action: vulpes_ems.update_plan
```

- **Cooldown:** the first call plans at once. Calls during that run or in the 30 seconds after it become one more run when the 30 seconds are over, so a sensor that updates a few times in a row gives two plans at most. Such a call returns before its plan is made. If a run takes longer than 30 seconds, a call made during it is dropped and the next quarter hour picks the change up.
- **Don't trigger on every change of the price sensor.** Each plan fetches the weather forecast from a free service, and each run with Active on calls your script.
- **The plan still starts at the quarter hour.** A plan made at 13:18 covers 13:15 to 13:30 as a whole slot, with the state of charge of 13:18. The mode for that slot can change, so your script may be called with another mode for the rest of it.
- A failing plan is not an error of the action: it shows on the Status sensor, as for a scheduled run.

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
- **Errors:** if the script or one of its actions fails (for example the inverter is offline), the error is logged and shown in the `error` attribute of the Battery control sensor. The next slot tries again.
- **Inverter failsafe:** if Home Assistant itself stops, Vulpes can't send anything. Use the inverter's own failsafe or timeout where it has one, set longer than 15 minutes, so that it returns to normal operation by itself.
- **Persistent settings:** settings that the inverter's failsafe doesn't revert (such as an export switch) stay as the last command left them.

## Example: Solis hybrid inverter with `solis_modbus`

The `solis_modbus` integration has a remote dispatch action with an inverter-side failsafe. Replace the entity IDs with your own and `normal_export_w` with your usual export limit. Make sure each power stays within your inverter's limits and Vulpes's battery and grid settings.

```yaml
vulpes_control:
  alias: Vulpes control
  mode: restart
  sequence:
    # Zero export through the inverter's feed-in limit, which the failsafe doesn't revert.
    # Written only when it changes: it is a persistent inverter setting.
    - variables:
        normal_export_w: 5000
        export_w: "{{ 0 if no_export else normal_export_w }}"
    - if: "{{ states('number.solis_backflow_power') | int(-1) != export_w | int }}"
      then:
        - action: number.set_value
          target:
            entity_id: number.solis_backflow_power
          data:
            value: "{{ export_w }}"
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
                allow_grid_charge: false
                soc_min: "{{ soc_min | int }}"
                pv_shutdown: "{{ pv_off }}"
                failsafe_minutes: 20
        # Needs Remote Dispatch V02; see the notes for V01.
        - conditions: "{{ mode == 'hold' }}"
          sequence:
            - action: solis_modbus.solis_dispatch
              data:
                mode: self_consumption
                disable_discharge: true
                allow_grid_charge: false
                pv_shutdown: "{{ pv_off }}"
                failsafe_minutes: 20
        - conditions: "{{ mode == 'idle' }}"
          sequence:
            - action: solis_modbus.solis_dispatch
              data:
                mode: battery_hold
                allow_grid_charge: false
                pv_shutdown: "{{ pv_off }}"
                failsafe_minutes: 20
        # Needs Remote Dispatch V02.
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
- **Script ID:** Vulpes runs the script through its own action, which Home Assistant names after the script's ID (`vulpes_control` above), not after its entity ID. If you rename the entity (for example to `script.vulpes_apply`), the action keeps its old name and Vulpes reports "Action script.vulpes_apply not found". Keep the entity ID and the script ID the same.
- **Firmware version:** the `self_consumption` mode and `disable_discharge` need Remote Dispatch V02 or later. The "Remote Dispatch Function Map" sensor of `solis_modbus` shows the version (1 = V01). On V01 `solis_modbus` rejects those calls ("The requested features require Remote Dispatch V02"), so:
  - leave **solar curtailment** off in the modes step, or the `self_consume` + `pv_off` branch fails;
  - do `hold` with `grid_import` at 0 W and the lower SOC limit at the current state of charge, rounded down. The battery keeps the meter at zero, so solar surplus still charges it, and the limit stops it from discharging. This has not yet been checked on a V01 inverter: in sunshine, check that the battery charges from the surplus and never discharges.

    ```yaml
            - conditions: "{{ mode == 'hold' }}"
              sequence:
                - action: solis_modbus.solis_dispatch
                  data:
                    mode: grid_import
                    power_watts: 0
                    allow_grid_charge: false
                    soc_min: >-
                      {{ [states('sensor.solis_battery_soc') | float(soc_max) | int,
                          soc_max | int] | min }}
                    soc_max: "{{ soc_max | int }}"
                    pv_shutdown: "{{ pv_off }}"
                    failsafe_minutes: 20
    ```
- **Zero export:** on some models the "allow export" switch doesn't stop export in self-use. The feed-in limit (`Backflow Power`, with the grid feed-in limit switch on) does. Check it once: with `no_export: true` the export should drop to zero.
- The dispatch action leaves `pv_shutdown` and `allow_grid_charge` unchanged when they're omitted, so the script always passes `pv_shutdown`, and every mode other than `charge` turns grid charging off.
- Turn off other automations that call `solis_dispatch` or `solis_dispatch_stop`; they cancel Vulpes's dispatch.
- After the first `solis_dispatch_stop`, check that the inverter's normal settings are back, including solar power.
