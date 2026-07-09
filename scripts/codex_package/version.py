"""Version discovery for Codex packages."""

from functools import lru_cache
from pathlib import Path

from .targets import REPO_ROOT


def _default_cargo_toml() -> Path:
    return REPO_ROOT / "codex-rs" / "Cargo.toml"


@lru_cache(maxsize=1)
def read_workspace_version(cargo_toml: Path | None = None) -> str:
    return _read_workspace_version_uncached(cargo_toml or _default_cargo_toml())


def _read_workspace_version_uncached(cargo_toml: Path) -> str:
    try:
        import tomllib
    except ModuleNotFoundError:
        pass
    else:
        data = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
        version = data.get("workspace", {}).get("package", {}).get("version")
        if isinstance(version, str):
            return version

    in_workspace_package = False
    with cargo_toml.open(encoding="utf-8") as fh:
        for line in fh:
            stripped = line.strip()
            if stripped == "[workspace.package]":
                in_workspace_package = True
                continue

            if in_workspace_package and stripped.startswith("["):
                break

            if in_workspace_package:
                version = parse_version_assignment(stripped)
                if version is not None:
                    return version

    raise RuntimeError(f"Could not find [workspace.package].version in {cargo_toml}")


def parse_version_assignment(line: str) -> str | None:
    if not line.startswith("version"):
        return None
    key, separator, value = line.partition("=")
    if separator != "=" or key.strip() != "version":
        return None
    value = value.strip()
    if len(value) < 2 or value[0] not in {'"', "'"}:
        return None
    quote = value[0]
    closing = value.find(quote, 1)
    if closing < 0:
        return None
    trailer = value[closing + 1 :].strip()
    if trailer and not trailer.startswith("#"):
        return None
    return value[1:closing]
