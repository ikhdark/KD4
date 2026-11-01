"""Version discovery for Codex packages."""

from functools import cache
from pathlib import Path
import tomllib

from .targets import REPO_ROOT


@cache
def read_workspace_version(cargo_toml: Path | None = None) -> str:
    cargo_toml = cargo_toml or REPO_ROOT / "codex-rs" / "Cargo.toml"
    data = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
    version = data.get("workspace", {}).get("package", {}).get("version")
    if isinstance(version, str) and version.strip():
        return version

    raise RuntimeError(f"Could not find [workspace.package].version in {cargo_toml}")
