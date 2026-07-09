"""Shared tool versions for local scripts and drift checks."""

from __future__ import annotations

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


LANE_NAME_PATTERN = r"[A-Za-z0-9_.-]+"
LANE_PATH_PATTERN = rf"target[\\/]+lanes[\\/]+({LANE_NAME_PATTERN})"
SCRIPT_LANE_PATTERN = rf"(?:^|\s)-Lane\s+({LANE_NAME_PATTERN})(?:\s|$)"
JUST_LANE_PATTERN = (
    rf"\b(?:test-lane(?:-fast)?|cargo-lane(?:-(?:home|isolated-home))?|"
    rf"test-lane-package|check-lane|clippy-lane|watch-lane|coverage-lane|"
    rf"fix-lane)\s+({LANE_NAME_PATTERN})\b"
)
JUST_FIXED_LANE_PATTERN = r"\b(test-lane-main|cargo-lane-main|release-lane)\b"
JUST_FIXED_LANE_NAMES = {
    "test-lane-main": "main",
    "cargo-lane-main": "main",
    "release-lane": "release",
}
