#!/usr/bin/env python3
"""Cross-platform shell launcher for `just` recipes.

This keeps recipe bodies as normal shell snippets while giving the justfile one
portable placeholder, `{args}`, for forwarding variadic recipe arguments.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time
from hashlib import sha256
from collections.abc import Callable
from collections.abc import Mapping
from pathlib import Path


ARGS_TOKEN = "{args}"
STDERR_NULL_TOKEN = "{stderr-null}"
POWERSHELL_ARGS = "@($args | Select-Object -Skip 1)"
# This placeholder must be the final token in a recipe snippet because the
# PowerShell expansion also exits with the command's last exit code.
POWERSHELL_STDERR_NULL = "2>$null; exit $LASTEXITCODE"
SH_ARGS = '"$@"'
SH_STDERR_NULL = "2>/dev/null"
PROBE_CACHE_TTL_SECONDS = 60 * 60
TOOL_RUN_TIMEOUT_SECONDS = 2.0
# sccache --show-stats exceeds 2s even against a warm server, and server
# cold-start can take ~10s; with a short timeout the success was never cached
# and every recipe line paid the full stall.
SCCACHE_PROBE_TIMEOUT_SECONDS = 15.0
CARGO_GIT_CLI_ENV_VAR = "CARGO_NET_GIT_FETCH_WITH_CLI"
PYTHON_CPU_COUNT_ENV_VAR = "PYTHON_CPU_COUNT"
DEFAULT_PYTHON_CPU_COUNT = "30"
# One shared local default with codex_package/cargo.py and common-rust-env.ps1;
# override everywhere with CODEX_SCCACHE_CACHE_SIZE.
SCCACHE_CACHE_SIZE_ENV_VAR = "CODEX_SCCACHE_CACHE_SIZE"
DEFAULT_SCCACHE_CACHE_SIZE = "80G"
LINUX_GNU_LINKER_ENV_VAR = "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER"
LINUX_GNU_RUSTFLAGS_ENV_VAR = "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"
WINDOWS_MSVC_LINKER_ENV_VAR = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"
WINDOWS_LLVM_LLD_LINK_DEFAULT = Path("C:/Program Files/LLVM/bin/lld-link.exe")
DISABLE_SCRIPT_VENV_VALUES = frozenset({"1", "true", "yes", "on"})
TOOL_RUN_RESULTS: dict[tuple[str, ...], bool] = {}


def main() -> int:
    if len(sys.argv) < 2:
        print("just shell adapter expected a recipe command.", file=sys.stderr)
        return 1

    command = sys.argv[1]
    recipe_name = sys.argv[2] if len(sys.argv) > 2 else ""
    recipe_args = sys.argv[3:]
    repo_root = Path(__file__).resolve().parents[1]
    cache_dir = probe_cache_dir(repo_root)
    os.environ.update(python_cpu_env(os.environ))
    python_updates = python_tool_env(
        os.environ,
        os_name=os.name,
        repo_root=repo_root,
        cache_dir=cache_dir,
    )
    if python_updates:
        os.environ.update(python_updates)
        os.environ.pop("PYTHONHOME", None)
    which = memoized_which(shutil.which)
    os.environ.update(
        rust_tool_env(
            os.environ,
            os_name=os.name,
            which=which,
            cache_dir=cache_dir,
            repo_root=repo_root,
        )
    )
    ensure_sccache_server_env(os.environ, which=which, cache_dir=cache_dir)

    try:
        if os.name == "nt":
            return run_powershell(
                command, recipe_name, recipe_args, which=which, cache_dir=cache_dir
            )
        return run_sh(command, recipe_name, recipe_args)
    except ValueError as exc:
        print(f"just shell adapter: {exc}", file=sys.stderr)
        return 1


def rust_tool_env(
    env: Mapping[str, str],
    *,
    os_name: str,
    which: Callable[[str], str | None],
    can_run: Callable[[list[str]], bool] | None = None,
    cache_dir: Path | None = None,
    repo_root: Path | None = None,
    platform_name: str | None = None,
) -> dict[str, str]:
    if is_ci(env):
        return {}

    updates: dict[str, str] = {}
    if not env.get(CARGO_GIT_CLI_ENV_VAR):
        updates[CARGO_GIT_CLI_ENV_VAR] = "true"
    rustc_wrapper = env.get("RUSTC_WRAPPER")
    if not rustc_wrapper:
        sccache = which("sccache")
        if sccache:
            updates["RUSTC_WRAPPER"] = sccache
            if repo_root is not None:
                if not env.get("SCCACHE_BASEDIR"):
                    updates["SCCACHE_BASEDIR"] = sccache_basedir(repo_root)
                if not env.get("SCCACHE_CACHE_SIZE"):
                    updates["SCCACHE_CACHE_SIZE"] = sccache_cache_size(env)
    elif is_sccache_wrapper(rustc_wrapper) and repo_root is not None:
        # Respect explicit user sccache configuration instead of clobbering
        # it (and then restarting their server against their own settings).
        if not env.get("SCCACHE_BASEDIR"):
            updates["SCCACHE_BASEDIR"] = sccache_basedir(repo_root)
        if not env.get("SCCACHE_CACHE_SIZE"):
            updates["SCCACHE_CACHE_SIZE"] = sccache_cache_size(env)

    if os_name == "nt":
        updates.update(windows_msvc_linker_env(env, which=which))
        return updates

    updates.update(
        linux_gnu_linker_env(
            env,
            which=which,
            platform_name=platform_name or sys.platform,
        )
    )
    return updates


def windows_msvc_linker_env(
    env: Mapping[str, str],
    *,
    which: Callable[[str], str | None],
) -> dict[str, str]:
    if env.get(WINDOWS_MSVC_LINKER_ENV_VAR):
        return {}

    lld_link = which("lld-link")
    if not lld_link:
        lld_link = first_existing_windows_lld_link(env)
        if not lld_link:
            return {}

    return {WINDOWS_MSVC_LINKER_ENV_VAR: lld_link}


def first_existing_windows_lld_link(env: Mapping[str, str]) -> str | None:
    for candidate in windows_lld_link_fallbacks(env):
        if candidate.exists():
            return str(candidate)
    return None


def windows_lld_link_fallbacks(env: Mapping[str, str]) -> list[Path]:
    candidates: list[Path] = []
    scoop = env.get("SCOOP")
    if scoop:
        candidates.append(
            Path(scoop) / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
        )
    user_profile = env.get("USERPROFILE")
    if user_profile:
        candidates.append(
            Path(user_profile)
            / "scoop"
            / "apps"
            / "llvm"
            / "current"
            / "bin"
            / "lld-link.exe"
        )
    candidates.append(WINDOWS_LLVM_LLD_LINK_DEFAULT)

    seen: set[str] = set()
    unique: list[Path] = []
    for candidate in candidates:
        key = normalize_path_for_compare(str(candidate))
        if key not in seen:
            seen.add(key)
            unique.append(candidate)
    return unique


def sccache_basedir(repo_root: Path) -> str:
    return os.path.abspath(repo_root)


def is_sccache_wrapper(value: str) -> bool:
    leaf = os.path.basename(value).lower()
    return value.lower() in {"sccache", "sccache.exe"} or leaf in {
        "sccache",
        "sccache.exe",
    }


def sccache_cache_size(env: Mapping[str, str]) -> str:
    override = (env.get(SCCACHE_CACHE_SIZE_ENV_VAR) or "").strip()
    return override or DEFAULT_SCCACHE_CACHE_SIZE


def expected_sccache_stats_cache_size(value: str) -> str:
    if value.endswith("G") and value[:-1].isdigit():
        return f"{value[:-1]} GiB"
    return value


def sccache_stats_max_cache_size(stdout: str) -> str | None:
    for line in stdout.splitlines():
        label, _, value = line.partition("Max cache size")
        if not label and value:
            return value.strip()
    return None


def ensure_sccache_server_env(
    env: Mapping[str, str],
    *,
    which: Callable[[str], str | None],
    run: Callable[..., subprocess.CompletedProcess[str]] = subprocess.run,
    cache_dir: Path | None = None,
) -> bool:
    wrapper = env.get("RUSTC_WRAPPER")
    cache_size = env.get("SCCACHE_CACHE_SIZE")
    if not wrapper or not cache_size or not is_sccache_wrapper(wrapper):
        return False

    sccache = wrapper if os.path.isabs(wrapper) else which("sccache")
    if not sccache:
        return False

    cache_key = sccache_env_ok_cache_key(sccache, cache_size)
    if read_cached_tool_run(cache_key, cache_dir) is True:
        return False

    try:
        stats = run(
            [sccache, "--show-stats"],
            env=dict(env),
            text=True,
            capture_output=True,
            timeout=SCCACHE_PROBE_TIMEOUT_SECONDS,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    if stats.returncode != 0:
        return False

    actual_cache_size = sccache_stats_max_cache_size(stats.stdout)
    if actual_cache_size is None:
        return False
    if actual_cache_size == expected_sccache_stats_cache_size(cache_size):
        write_cached_tool_run(cache_key, cache_dir, True)
        return False

    try:
        # Stopping a server that disappeared between the stats probe and this
        # call is harmless. Starting and then observing the requested size are
        # the operations that prove the restart actually succeeded.
        run(
            [sccache, "--stop-server"],
            env=dict(env),
            text=True,
            capture_output=True,
            timeout=SCCACHE_PROBE_TIMEOUT_SECONDS,
            check=False,
        )
        started = run(
            [sccache, "--start-server"],
            env=dict(env),
            text=True,
            capture_output=True,
            timeout=SCCACHE_PROBE_TIMEOUT_SECONDS,
            check=False,
        )
        if started.returncode != 0:
            return False
        verified = run(
            [sccache, "--show-stats"],
            env=dict(env),
            text=True,
            capture_output=True,
            timeout=SCCACHE_PROBE_TIMEOUT_SECONDS,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    if verified.returncode != 0 or sccache_stats_max_cache_size(
        verified.stdout
    ) != expected_sccache_stats_cache_size(cache_size):
        return False
    write_cached_tool_run(cache_key, cache_dir, True)
    return True


def sccache_env_ok_cache_key(sccache: str, cache_size: str) -> list[str]:
    return [
        sccache,
        "--show-stats",
        f"max-cache-size={expected_sccache_stats_cache_size(cache_size)}",
    ]


def linux_gnu_linker_env(
    env: Mapping[str, str],
    *,
    which: Callable[[str], str | None],
    platform_name: str,
) -> dict[str, str]:
    if not platform_name.startswith("linux"):
        return {}
    if env.get(LINUX_GNU_LINKER_ENV_VAR) or env.get(LINUX_GNU_RUSTFLAGS_ENV_VAR):
        return {}

    clang = which("clang")
    if not clang:
        return {}

    if which("mold"):
        linker = "mold"
    elif which("ld.lld"):
        linker = "lld"
    else:
        return {}

    return {
        LINUX_GNU_LINKER_ENV_VAR: clang,
        LINUX_GNU_RUSTFLAGS_ENV_VAR: f"-C link-arg=-fuse-ld={linker}",
    }


def python_tool_env(
    env: Mapping[str, str],
    *,
    os_name: str,
    repo_root: Path,
    cache_dir: Path | None = None,
    stderr=sys.stderr,
) -> dict[str, str]:
    if is_ci(env):
        return {}
    if env.get("VIRTUAL_ENV"):
        return {}
    if env.get("CODEXKD_DISABLE_SCRIPT_VENV", "").lower() in DISABLE_SCRIPT_VENV_VALUES:
        return {}

    venv = repo_root / "scripts" / ".venv"
    bin_dir = venv / ("Scripts" if os_name == "nt" else "bin")
    python_exe = bin_dir / ("python.exe" if os_name == "nt" else "python")
    if not python_exe.is_file():
        if (repo_root / "scripts" / "uv.lock").exists():
            warn_once(
                "scripts-venv-missing",
                "scripts/.venv is missing; run `uv sync --directory scripts` "
                "before Python-backed just recipes.",
                cache_dir=cache_dir,
                stderr=stderr,
            )
        return {}

    return {
        "PATH": prepend_path(env.get("PATH", ""), str(bin_dir)),
        "VIRTUAL_ENV": str(venv),
        "VIRTUAL_ENV_DISABLE_PROMPT": "1",
    }


def python_cpu_env(env: Mapping[str, str]) -> dict[str, str]:
    if is_ci(env) or env.get(PYTHON_CPU_COUNT_ENV_VAR):
        return {}
    return {PYTHON_CPU_COUNT_ENV_VAR: DEFAULT_PYTHON_CPU_COUNT}


def prepend_path(path: str, entry: str) -> str:
    parts = [part for part in path.split(os.pathsep) if part]
    normalized_entry = normalize_path_for_compare(entry)
    filtered = [
        part for part in parts if normalize_path_for_compare(part) != normalized_entry
    ]
    return os.pathsep.join([entry, *filtered])


def normalize_path_for_compare(path: str) -> str:
    return os.path.normcase(os.path.normpath(path))


def cached_tool_runs(command: list[str], *, cache_dir: Path | None = None) -> bool:
    key = tuple(command)
    if key in TOOL_RUN_RESULTS:
        return TOOL_RUN_RESULTS[key]

    cached_result = read_cached_tool_run(command, cache_dir)
    if cached_result is not None:
        TOOL_RUN_RESULTS[key] = cached_result
        return cached_result

    result = tool_runs(command)
    if result is None:
        # A timeout/launch failure is transient (machine under load, AV
        # rescan); do not poison the 1-hour cache with "fail", which would
        # brick every Windows recipe with a misleading version error.
        TOOL_RUN_RESULTS[key] = False
        return False
    TOOL_RUN_RESULTS[key] = result
    write_cached_tool_run(command, cache_dir, result)
    return result


def tool_runs(
    command: list[str], *, timeout: float = TOOL_RUN_TIMEOUT_SECONDS
) -> bool | None:
    try:
        result = subprocess.run(
            command,
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return result.returncode == 0


def is_ci(env: Mapping[str, str]) -> bool:
    value = env.get("CI", "")
    return value.lower() not in ("", "0", "false", "no")


def probe_cache_dir(repo_root: Path) -> Path:
    return repo_root / "codex-rs" / "target" / "just-shell"


def tool_run_cache_path(command: list[str], cache_dir: Path) -> Path:
    label = sanitize_cache_part(Path(command[0]).name if command else "tool")
    digest = sha256(tool_run_cache_identity(command).encode("utf-8")).hexdigest()
    return cache_dir / f"{label}-{digest}.probe"


def tool_run_cache_identity(command: list[str]) -> str:
    parts = list(command)
    if command:
        executable = shutil.which(command[0]) or command[0]
        parts.append(f"resolved={executable}")
        try:
            stat = Path(executable).stat()
        except OSError:
            pass
        else:
            parts.append(f"mtime_ns={stat.st_mtime_ns}")
            parts.append(f"size={stat.st_size}")
    return "\0".join(parts)


def sanitize_cache_part(part: str) -> str:
    safe = []
    for char in part:
        if char.isalnum() or char in ("-", "_", "."):
            safe.append(char)
        else:
            safe.append("_")
    return "".join(safe).strip("_")[:64] or "empty"


def read_cached_tool_run(command: list[str], cache_dir: Path | None) -> bool | None:
    if cache_dir is None:
        return None
    path = tool_run_cache_path(command, cache_dir)
    try:
        if time.time() - path.stat().st_mtime > PROBE_CACHE_TTL_SECONDS:
            return None
        value = path.read_text(encoding="utf-8").strip()
    except OSError:
        return None
    if value == "ok":
        return True
    if value == "fail":
        return False
    return None


def write_cached_tool_run(
    command: list[str], cache_dir: Path | None, result: bool
) -> None:
    if cache_dir is None:
        return
    try:
        cache_dir.mkdir(parents=True, exist_ok=True)
        tool_run_cache_path(command, cache_dir).write_text(
            "ok" if result else "fail", encoding="utf-8"
        )
    except OSError:
        return


def warn_once(
    key: str,
    message: str,
    *,
    cache_dir: Path | None,
    stderr=sys.stderr,
) -> None:
    if cache_dir is not None:
        path = cache_dir / f"{sanitize_cache_part(key)}.warn"
        try:
            if time.time() - path.stat().st_mtime <= PROBE_CACHE_TTL_SECONDS:
                return
        except OSError:
            pass
        try:
            cache_dir.mkdir(parents=True, exist_ok=True)
            path.write_text("warned", encoding="utf-8")
        except OSError:
            pass
    print(message, file=stderr)


def memoized_which(
    which: Callable[[str], str | None],
) -> Callable[[str], str | None]:
    cache: dict[str, str | None] = {}

    def lookup(program: str) -> str | None:
        if program not in cache:
            cache[program] = which(program)
        return cache[program]

    return lookup


def run_sh(
    command: str,
    recipe_name: str,
    recipe_args: list[str],
    *,
    which: Callable[[str], str | None] = shutil.which,
    stderr=sys.stderr,
) -> int:
    sh = which("sh")
    if sh is None:
        print("POSIX shell ('sh') is required for just recipes.", file=stderr)
        return 1
    command = render_command(command, args=SH_ARGS, stderr_null=SH_STDERR_NULL)
    try:
        os.execv(sh, ["sh", "-cu", command, recipe_name, *recipe_args])
    except OSError as exc:
        print(f"Failed to launch POSIX shell ('sh'): {exc}", file=stderr)
        return 1


def run_powershell(
    command: str,
    recipe_name: str,
    recipe_args: list[str],
    *,
    which: Callable[[str], str | None] = shutil.which,
    can_run: Callable[[list[str]], bool] | None = None,
    cache_dir: Path | None = None,
    stderr=sys.stderr,
) -> int:
    pwsh = which("pwsh.exe") or which("pwsh")
    if pwsh is None:
        print(
            "PowerShell ('pwsh') is required for Windows just recipes. "
            "Run 'just install' to install it.",
            file=stderr,
        )
        return 1
    if not powershell_supports_command_with_args(
        pwsh, can_run=can_run, cache_dir=cache_dir
    ):
        print(
            "PowerShell 7.4 or newer is required for Windows just recipes. "
            "Upgrade pwsh or run 'just install'.",
            file=stderr,
        )
        return 1

    command = render_command(
        command, args=POWERSHELL_ARGS, stderr_null=POWERSHELL_STDERR_NULL
    )
    try:
        return subprocess.run(
            [
                pwsh,
                "-NoLogo",
                "-NoProfile",
                "-CommandWithArgs",
                command,
                recipe_name,
                *recipe_args,
            ],
            check=False,
        ).returncode
    except OSError as exc:
        print(f"Failed to launch PowerShell ('pwsh'): {exc}", file=stderr)
        return 1


def powershell_supports_command_with_args(
    pwsh: str,
    *,
    can_run: Callable[[list[str]], bool] | None = None,
    cache_dir: Path | None = None,
) -> bool:
    command = [
        pwsh,
        "-NoLogo",
        "-NoProfile",
        "-Command",
        "if ($PSVersionTable.PSVersion -lt [version]'7.4') { exit 1 }",
    ]
    if can_run is None:
        return cached_tool_runs(command, cache_dir=cache_dir)
    return can_run(command)


def render_command(command: str, *, args: str, stderr_null: str) -> str:
    if STDERR_NULL_TOKEN in command:
        stripped = command.rstrip()
        if command.count(STDERR_NULL_TOKEN) > 1 or not stripped.endswith(
            STDERR_NULL_TOKEN
        ):
            raise ValueError(f"{STDERR_NULL_TOKEN} must be the final token")
    return command.replace(ARGS_TOKEN, args).replace(STDERR_NULL_TOKEN, stderr_null)


if __name__ == "__main__":
    raise SystemExit(main())
