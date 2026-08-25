"""Shared Rust compiler-cache and Windows linker environment policy."""

from __future__ import annotations

import os
from collections.abc import Callable, Mapping
from pathlib import Path


SCCACHE_CACHE_SIZE_ENV_VAR = "CODEX_SCCACHE_CACHE_SIZE"
DEFAULT_SCCACHE_CACHE_SIZE = "80G"
WINDOWS_LLVM_LLD_LINK_DEFAULT = Path("C:/Program Files/LLVM/bin/lld-link.exe")
_SCOOP_LLVM_LLD_LINK = Path("apps/llvm/current/bin/lld-link.exe")


def is_sccache_wrapper(value: str) -> bool:
    leaf = value.replace("\\", "/").rsplit("/", 1)[-1].casefold()
    return value.casefold() in {"sccache", "sccache.exe"} or leaf in {
        "sccache",
        "sccache.exe",
    }


def sccache_cache_size(env: Mapping[str, str]) -> str:
    override = (env.get(SCCACHE_CACHE_SIZE_ENV_VAR) or "").strip()
    return override or DEFAULT_SCCACHE_CACHE_SIZE


def windows_lld_link_fallbacks(
    env: Mapping[str, str],
    *,
    default_path: Path = WINDOWS_LLVM_LLD_LINK_DEFAULT,
) -> tuple[Path, ...]:
    candidates: list[Path] = []
    scoop = env.get("SCOOP")
    if scoop:
        candidates.append(Path(scoop) / _SCOOP_LLVM_LLD_LINK)
    user_profile = env.get("USERPROFILE")
    if user_profile:
        candidates.append(Path(user_profile) / "scoop" / _SCOOP_LLVM_LLD_LINK)
    candidates.append(default_path)

    seen: set[str] = set()
    unique: list[Path] = []
    for candidate in candidates:
        key = os.path.normcase(os.path.normpath(str(candidate)))
        if key not in seen:
            seen.add(key)
            unique.append(candidate)
    return tuple(unique)


def find_windows_lld_link(
    env: Mapping[str, str],
    *,
    which: Callable[[str], str | None],
    default_path: Path = WINDOWS_LLVM_LLD_LINK_DEFAULT,
) -> str | None:
    on_path = which("lld-link")
    if on_path:
        return on_path
    for candidate in windows_lld_link_fallbacks(env, default_path=default_path):
        if candidate.exists():
            return str(candidate)
    return None
