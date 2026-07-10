"""Shared Rust package discovery helpers for local validation scripts."""

from __future__ import annotations

import os
from pathlib import Path
import tomllib


CARGO_MANIFEST = "Cargo.toml"


def _normalized_path(path: Path) -> str:
    return os.path.normcase(str(path.resolve(strict=False)))


def _is_within_or_same(path: Path, root: Path) -> bool:
    normalized_root = _normalized_path(root)
    try:
        return (
            os.path.commonpath([_normalized_path(path), normalized_root])
            == normalized_root
        )
    except ValueError:
        return False


def package_search_start(path: Path, *, assume_file: bool = False) -> Path:
    if assume_file:
        return path.parent
    if path.is_dir():
        return path
    if path.suffix:
        return path.parent
    return path.parent if path.exists() else path


def nearest_package_root(
    path: Path,
    *,
    repo_root: Path | None = None,
    package_root_cache: dict[Path, Path | None] | None = None,
    assume_file: bool = False,
) -> Path | None:
    current = package_search_start(path, assume_file=assume_file)
    if package_root_cache is not None and current in package_root_cache:
        return package_root_cache[current]

    visited: list[Path] = []
    repo_bound = repo_root.resolve(strict=False) if repo_root is not None else None
    if repo_bound is not None and not _is_within_or_same(current, repo_bound):
        if package_root_cache is not None:
            package_root_cache[current] = None
        return None

    codex_rs_root = (repo_root / "codex-rs") if repo_root is not None else None
    result: Path | None = None
    while current.name and current != current.parent:
        visited.append(current)
        manifest = current / CARGO_MANIFEST
        if manifest.is_file():
            result = current
            break
        if current.name == "codex-rs" or current == codex_rs_root:
            break
        if repo_bound is not None and _normalized_path(current) == _normalized_path(
            repo_bound
        ):
            break
        current = current.parent

    if package_root_cache is not None:
        for visited_dir in visited:
            package_root_cache[visited_dir] = result
    return result


def package_name(manifest: Path) -> str | None:
    try:
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return None
    package = data.get("package")
    if isinstance(package, dict):
        name = package.get("name")
        if isinstance(name, str) and name:
            return name
    return None
