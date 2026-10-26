"""Shared Rust compiler-cache and Windows linker environment policy."""

from __future__ import annotations

import os
import shutil
import socket
import subprocess
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path

SCCACHE_CACHE_SIZE_ENV_VAR = "CODEX_SCCACHE_CACHE_SIZE"
DEFAULT_SCCACHE_CACHE_SIZE = "80G"
SCCACHE_DEFAULT_SERVER_PORT = 4226
# The kernel completes a live loopback handshake in well under a millisecond;
# the timeout is a kernel wait, so a starved below-normal caller still sees it
# connected. It bounds only the no-server case, where Windows retries a refused
# connect for ~2 s: a miss costs this much before the usual `--start-server`.
SCCACHE_SERVER_PROBE_TIMEOUT_SECONDS = 0.05
WINDOWS_LLVM_LLD_LINK_DEFAULT = Path("C:/Program Files/LLVM/bin/lld-link.exe")
_SCOOP_LLVM_LLD_LINK = Path("apps/llvm/current/bin/lld-link.exe")
# Keep core compilation in its existing warm lane family.
CORE_BUILD_PACKAGES = ("codex-core",)


def local_rust_env(
    env: Mapping[str, str],
    *,
    repo_root: Path | None = None,
    which: Callable[[str], str | None] = shutil.which,
) -> dict[str, str]:
    """Shared, probe-free defaults for Just, lane and direct Cargo entrypoints."""
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


def sccache_server_address(env: Mapping[str, str]) -> tuple[str, int] | None:
    """The loopback TCP address an sccache client contacts, if unambiguous.

    sccache clients connect to 127.0.0.1 on SCCACHE_SERVER_PORT (default 4226).
    A Unix socket or a value sccache might parse differently returns None.
    """
    if env.get("SCCACHE_SERVER_UDS"):
        return None
    raw = env.get("SCCACHE_SERVER_PORT")
    if raw is None:
        return "127.0.0.1", SCCACHE_DEFAULT_SERVER_PORT
    if not (raw.isascii() and raw.isdigit() and len(raw) <= 5):
        return None
    if not 0 < int(raw) < 65536:
        return None
    return "127.0.0.1", int(raw)


def sccache_server_reachable(
    env: Mapping[str, str],
    *,
    timeout: float = SCCACHE_SERVER_PROBE_TIMEOUT_SECONDS,
) -> bool:
    """Whether a server already listens where this environment's clients connect.

    This is the evidence `--start-server` accepts as "Address in use", observed
    in-process: no client or server process is spawned, so a caller starved at
    below-normal priority cannot time out waiting for one. The connection is
    closed without a request; it neither stops nor reconfigures the server.
    """
    address = sccache_server_address(env)
    if address is None:
        return False
    try:
        with socket.create_connection(address, timeout=timeout):
            return True
    except OSError:
        return False


def prepare_shared_sccache(
    *, env: Mapping[str, str] | None = None, cwd: str | Path | None = None
) -> None:
    """Start the shared compiler cache before entering a command's job.

    Never restart an existing server: other lanes may still be using it. A
    reachable server needs no `--start-server` spawn. A failed or timed-out
    start raises unless a concurrent start made the server reachable meanwhile.
    Callers then disable the wrapper for that one command (the Rust runner and
    `process_owner.run_owned`), so Cargo cannot auto-start a server inside a
    job that is terminated as soon as the lane's command finishes.
    """
    environment = os.environ if env is None else env
    wrapper = environment.get("RUSTC_WRAPPER", "")
    if not is_sccache_wrapper(wrapper):
        return
    if sccache_server_reachable(environment):
        return
    executable = shutil.which(wrapper, path=environment.get("PATH")) or wrapper
    try:
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
    except subprocess.SubprocessError:
        # Another lane may have won the bind, or this server finished starting
        # after its client gave up. Use it rather than report a failure.
        if sccache_server_reachable(environment):
            return
        raise


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
