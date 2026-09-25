#!/usr/bin/env python3
"""Windows PowerShell launcher for `just` recipes.

This keeps recipe bodies as normal shell snippets while giving the justfile one
placeholder, `{args}`, for forwarding variadic recipe arguments.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from hashlib import sha256
from collections.abc import Callable
from collections.abc import Mapping
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import rust_tool_env as shared_rust_tool_env  # noqa: E402


ARGS_TOKEN = "{args}"
STDERR_NULL_TOKEN = "{stderr-null}"
POWERSHELL_ARGS = "@($args | Select-Object -Skip 1)"
# This placeholder must be the final token in a recipe snippet because the
# PowerShell expansion also exits with the command's last exit code.
POWERSHELL_STDERR_NULL = "2>$null; exit $LASTEXITCODE"
PROBE_CACHE_TTL_SECONDS = 60 * 60
TOOL_RUN_TIMEOUT_SECONDS = 2.0
PYTHON_CPU_COUNT_ENV_VAR = "PYTHON_CPU_COUNT"
MAX_DEFAULT_PYTHON_CPU_COUNT = 30
DISABLE_SCRIPT_VENV_VALUES = frozenset({"1", "true", "yes", "on"})
RUST_COMMAND_PATTERN = re.compile(
    r"(?<![\w.-])(?:cargo|rustc|rustup)(?![\w.-])", re.IGNORECASE
)


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
        repo_root=repo_root,
        cache_dir=cache_dir,
    )
    if python_updates:
        os.environ.update(python_updates)
        os.environ.pop("PYTHONHOME", None)
    which = memoized_which(shutil.which)
    if command_needs_rust_tooling(command):
        os.environ.update(
            rust_tool_env(
                os.environ,
                which=which,
                cache_dir=cache_dir,
                repo_root=repo_root,
            )
        )

    try:
        return run_powershell(
            command, recipe_name, recipe_args, which=which, cache_dir=cache_dir
        )
    except ValueError as exc:
        print(f"just shell adapter: {exc}", file=sys.stderr)
        return 1


def command_needs_rust_tooling(command: str) -> bool:
    """Return whether a recipe command needs Rust compiler/cache setup."""

    return bool(
        RUST_COMMAND_PATTERN.search(command)
        or "rust_build_status.py" in command
        or "rust_test_runner.py" in command
        or "cargo-lane" in command
    )


def rust_tool_env(
    env: Mapping[str, str],
    *,
    which: Callable[[str], str | None],
    cache_dir: Path | None = None,
    repo_root: Path | None = None,
) -> dict[str, str]:
    return shared_rust_tool_env.local_rust_env(env, which=which, repo_root=repo_root)


def python_tool_env(
    env: Mapping[str, str],
    *,
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
    bin_dir = venv / "Scripts"
    python_exe = bin_dir / "python.exe"
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
    logical_cpus = os.cpu_count() or 1
    return {
        PYTHON_CPU_COUNT_ENV_VAR: str(min(MAX_DEFAULT_PYTHON_CPU_COUNT, logical_cpus))
    }


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
    cached_result = read_cached_tool_run(command, cache_dir)
    if cached_result is not None:
        return cached_result

    result = tool_runs(command)
    if result is None:
        # A timeout/launch failure is transient (machine under load, AV
        # rescan). Let the real invocation proceed and report its own error if
        # the tool is actually unusable; do not cache an inconclusive probe.
        return True
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


def read_cached_tool_run(
    command: list[str],
    cache_dir: Path | None,
    *,
    ttl_seconds: float = PROBE_CACHE_TTL_SECONDS,
) -> bool | None:
    if cache_dir is None:
        return None
    path = tool_run_cache_path(command, cache_dir)
    try:
        age = time.time() - path.stat().st_mtime
        if not -0.001 <= age <= ttl_seconds:
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
    temporary_path: Path | None = None
    try:
        cache_dir.mkdir(parents=True, exist_ok=True)
        destination = tool_run_cache_path(command, cache_dir)
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=cache_dir,
            prefix=f".{destination.name}.",
            suffix=".tmp",
            delete=False,
        ) as temporary:
            temporary.write("ok" if result else "fail")
            temporary_path = Path(temporary.name)
        os.replace(temporary_path, destination)
        temporary_path = None
    except OSError:
        return
    finally:
        if temporary_path is not None:
            try:
                temporary_path.unlink()
            except OSError:
                pass


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
            age = time.time() - path.stat().st_mtime
            if -0.001 <= age <= PROBE_CACHE_TTL_SECONDS:
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
            "Install PowerShell 7.5 or newer, then rerun the recipe.",
            file=stderr,
        )
        return 1
    if not powershell_supports_command_with_args(
        pwsh, can_run=can_run, cache_dir=cache_dir
    ):
        print(
            "PowerShell 7.5 or newer is required for Windows just recipes. "
            "Upgrade pwsh, then rerun the recipe.",
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
                "-NonInteractive",
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
        "-NonInteractive",
        "-Command",
        "if ($PSVersionTable.PSVersion -lt [version]'7.5') { exit 1 }",
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
