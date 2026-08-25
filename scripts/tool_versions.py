"""Shared tool versions for local scripts and drift checks."""

from __future__ import annotations

import json
import tomllib
from functools import cache
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
RUSTFMT_TOOLCHAIN = "nightly-2025-09-18"


@cache
def scripts_ruff_requirement() -> str:
    data = tomllib.loads(
        (REPO_ROOT / "scripts" / "pyproject.toml").read_text(encoding="utf-8")
    )
    for dependency in data.get("project", {}).get("dependencies", []):
        if isinstance(dependency, str) and dependency.startswith("ruff"):
            return dependency
    raise RuntimeError("scripts/pyproject.toml must declare a ruff dependency")


@cache
def cargo_lane_patterns() -> dict[str, object]:
    pattern_path = Path(__file__).with_name("cargo_lane_patterns.json")
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


_CARGO_LANE_PATTERNS = cargo_lane_patterns()
LANE_PATH_PATTERN = str(_CARGO_LANE_PATTERNS["lane_path_pattern"])
SCRIPT_LANE_PATTERN = str(_CARGO_LANE_PATTERNS["script_lane_pattern"])
JUST_LANE_PATTERN = str(_CARGO_LANE_PATTERNS["just_lane_pattern"])
JUST_FIXED_LANE_PATTERN = str(_CARGO_LANE_PATTERNS["just_fixed_lane_pattern"])
JUST_FIXED_LANE_NAMES = dict(_CARGO_LANE_PATTERNS["just_fixed_lane_names"])
