"""Shared Rust compiler-cache and Windows linker environment policy."""

from __future__ import annotations

import os
import shutil
import subprocess
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
        if not env.get("SCCACHE_BASEDIRS"):
            updates["SCCACHE_BASEDIRS"] = os.path.abspath(repo_root)
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


def prepare_shared_sccache(
    *, env: Mapping[str, str] | None = None, cwd: str | Path | None = None
) -> None:
    """Start the shared compiler cache before entering a command's job.

    Never restart an existing server: other lanes may still be using it.
    Failure is fatal rather than letting Cargo auto-start a server inside the
    job that will be terminated as soon as this lane's command finishes.
    """
    environment = os.environ if env is None else env
    wrapper = environment.get("RUSTC_WRAPPER", "")
    if not is_sccache_wrapper(wrapper):
        return
    executable = shutil.which(wrapper, path=environment.get("PATH")) or wrapper
    result = subprocess.run(
        [executable, "--start-server"],
        env=environment,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        timeout=10,
        check=False,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    # --show-stats succeeds with synthetic empty stats even without a server.
    # --start-server is safe under concurrent lane startup: only the first
    # caller can bind the address; a live server is never stopped or reset.
    if result.returncode == 2 and any(
        message in result.stderr
        for message in ("Address in use", "Address already in use")
    ):
        return
    result.check_returncode()


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
        if candidate.is_file():
            return str(candidate)
    return None
