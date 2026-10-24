"""Formatter version and shared Cargo lane naming."""

from __future__ import annotations

import json
from functools import cache
from pathlib import Path

RUSTFMT_TOOLCHAIN = "nightly-2025-09-18"
@cache
def cargo_lane_patterns() -> dict[str, object]:
    pattern_path = (
        Path(__file__).resolve().parents[1] / "build" / "cargo_lane_patterns.json"
    )
    data = json.loads(pattern_path.read_text(encoding="utf-8"))
    required_patterns = (
        "lane_path_pattern",
        "script_lane_pattern",
        "just_lane_pattern",
        "just_fixed_lane_pattern",
    )
    for name in required_patterns:
        if not isinstance(data.get(name), str) or not data[name]:
            raise RuntimeError(f"{pattern_path} must define a non-empty {name}")
    fixed_lane_names = data.get("just_fixed_lane_names")
    if not isinstance(fixed_lane_names, dict) or not all(
        isinstance(name, str) and isinstance(lane, str)
        for name, lane in fixed_lane_names.items()
    ):
        raise RuntimeError(
            f"{pattern_path} must define a string-to-string just_fixed_lane_names map"
        )
    return data


if __name__ == "__main__":
    print(RUSTFMT_TOOLCHAIN)
