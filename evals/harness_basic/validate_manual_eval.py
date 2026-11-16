#!/usr/bin/env python3
"""Fail closed before paid evaluation setup unless both arms select Luna."""
import json
from pathlib import Path
import tomllib

TARGET = "openai/gpt-6-luna"


def validate(manifest, config):
    presets = config["presets"]
    for key in ("focused_preset", "control_preset"):
        name = manifest["monitor"][key]
        if presets[name].get("targets") != [TARGET]:
            raise ValueError(f"{name}: paid evaluation paused; select only {TARGET}. GPT-5.5 is not permitted.")


if __name__ == "__main__":
    root = Path(__file__).parent
    validate(json.loads((root / "search_efficiency_baseline.json").read_text()),
             tomllib.loads((root / "mira.toml").read_text()))
