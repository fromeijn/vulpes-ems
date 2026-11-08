"""Consistency checks for the integration's translations."""

import json
from pathlib import Path
import re

_dir = Path(__file__).parents[1] / "custom_components" / "vulpes_ems"
STRINGS = json.loads((_dir / "strings.json").read_text())
FLOW = (_dir / "config_flow.py").read_text()


def test_strings_and_english_translation_are_identical():
    assert (_dir / "strings.json").read_text() == (_dir / "translations" / "en.json").read_text()


def test_options_steps_have_the_same_labels_as_setup():
    config, options = STRINGS["config"]["step"], STRINGS["options"]["step"]
    shared = {"sources": "user"} | {k: k for k in config if k != "user"}
    for opt, cfg in shared.items():
        assert opt in options, opt
        for key in ("data", "data_description", "menu_options"):
            assert options[opt].get(key, {}).keys() == config[cfg].get(key, {}).keys(), (opt, key)


def test_options_menu_lists_every_section():
    menu = re.search(r"OPTIONS_MENU = \[(.*?)\]", FLOW).group(1)
    sections = re.findall(r'"(\w+)"', menu)
    assert sections == ["sources", "battery", "tariff", "modes", "solar"]
    assert list(STRINGS["options"]["step"]["init"]["menu_options"]) == sections


def test_every_error_and_abort_has_a_string():
    errors = set(re.findall(r'errors\[[^\]]+\] = "(\w+)"', FLOW)) | {"no_modes"}
    checks = (_dir / "setup_checks.py").read_text()
    errors |= set(re.findall(r'\{"base": "(\w+)"\}', checks)) | {"charge_w_too_high", "discharge_w_too_high"}
    aborts = set(re.findall(r'async_abort\(reason="(\w+)"\)', FLOW))
    for section in ("config", "options"):
        assert errors <= STRINGS[section]["error"].keys(), (section, errors - STRINGS[section]["error"].keys())
    assert aborts <= STRINGS["options"]["abort"].keys()
    assert aborts - {"no_calibration"} <= STRINGS["config"]["abort"].keys()
