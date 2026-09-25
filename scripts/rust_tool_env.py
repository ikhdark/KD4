"""Shared Rust compiler-cache and Windows linker environment policy."""

from __future__ import annotations

import os
import shutil
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path


SCCACHE_CACHE_SIZE_ENV_VAR = "CODEX_SCCACHE_CACHE_SIZE"
DEFAULT_SCCACHE_CACHE_SIZE = "80G"
WINDOWS_LLVM_LLD_LINK_DEFAULT = Path("C:/Program Files/LLVM/bin/lld-link.exe")
_SCOOP_LLVM_LLD_LINK = Path("apps/llvm/current/bin/lld-link.exe")


def local_rust_env(
    env: Mapping[str, str],
    *,
    repo_root: Path | None = None,
    which: Callable[[str], str | None] = shutil.which,
) -> dict[str, str]:
    """Shared, probe-free defaults for Just, lane and direct test entrypoints."""
    if env.get("CI", "").lower() not in ("", "0", "false", "no"):
        return {}
    updates: dict[str, str] = {}
    if not env.get("CARGO_NET_GIT_FETCH_WITH_CLI"):
        updates["CARGO_NET_GIT_FETCH_WITH_CLI"] = "true"
    wrapper = env.get("RUSTC_WRAPPER")
    if wrapper is None:
        wrapper = which("sccache")
        if wrapper:
            updates["RUSTC_WRAPPER"] = wrapper
    if wrapper and is_sccache_wrapper(wrapper) and repo_root is not None:
        if not env.get("SCCACHE_BASEDIR"):
            updates["SCCACHE_BASEDIR"] = os.path.abspath(repo_root)
        if not env.get("SCCACHE_CACHE_SIZE"):
            updates["SCCACHE_CACHE_SIZE"] = sccache_cache_size(env)
    missing_linkers = [
        f"CARGO_TARGET_{target}_PC_WINDOWS_MSVC_LINKER"
        for target in ("X86_64", "AARCH64")
        if not env.get(f"CARGO_TARGET_{target}_PC_WINDOWS_MSVC_LINKER")
    ]
    if missing_linkers and (linker := find_windows_lld_link(env, which=which)):
        updates.update(dict.fromkeys(missing_linkers, linker))
    return updates


def cargo_package_specs(args: Sequence[str]) -> list[str]:
    """Read Cargo package selectors before the compiler/test separator."""
    packages: list[str] = []
    tokens = iter(args)
    for token in tokens:
        if token == "--":
            break
        spec = None
        if token in {"-p", "--package"}:
            spec = next(tokens, None)
            if spec == "--":
                break
        elif token.startswith("--package="):
            spec = token[len("--package=") :]
        elif token.startswith("-p"):
            spec = token[2:].removeprefix("=")
        if spec and not spec.startswith("-"):
            packages.append(spec)
    return packages


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
