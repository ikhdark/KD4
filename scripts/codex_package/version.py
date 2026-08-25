"""Version discovery for Codex packages."""

from functools import cache
from pathlib import Path
import tomllib

from .targets import REPO_ROOT


def _default_cargo_toml() -> Path:
    return REPO_ROOT / "codex-rs" / "Cargo.toml"


@cache
def read_workspace_version(cargo_toml: Path | None = None) -> str:
    return _read_workspace_version_uncached(cargo_toml or _default_cargo_toml())


def _read_workspace_version_uncached(cargo_toml: Path) -> str:
    data = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
    version = data.get("workspace", {}).get("package", {}).get("version")
    if isinstance(version, str):
        return version

    raise RuntimeError(f"Could not find [workspace.package].version in {cargo_toml}")
