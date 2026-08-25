#!/usr/bin/env python3

import importlib.util
import shutil
import tomllib
from pathlib import Path
from types import ModuleType


REPO_ROOT = Path(__file__).resolve().parents[1]


def powershell() -> str | None:
    # Prefer Windows PowerShell 5.1: the justfile invokes these scripts via
    # `powershell -NoProfile -File ...`, so tests should exercise the same
    # host (5.1 has stricter native-stderr and StrictMode semantics).
    return shutil.which("powershell") or shutil.which("pwsh")


def pwsh_only() -> str | None:
    # invoke-rust-perf-env.ps1 runs under pwsh 7.4+ in production (recipes
    # invoke it inline in the just-shell pwsh session), and its -NoSccache
    # proof depends on pwsh's empty-env-var semantics, so its tests must not
    # fall back to Windows PowerShell 5.1.
    return shutil.which("pwsh")


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


def load_script_module(filename: str, module_name: str) -> ModuleType:
    path = REPO_ROOT / "scripts" / filename
    spec = importlib.util.spec_from_file_location(module_name, path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_just_shell_module() -> ModuleType:
    return load_script_module("just-shell.py", "just_shell")


def load_format_module() -> ModuleType:
    return load_script_module("format.py", "format_script")


def load_root_maintenance_module() -> ModuleType:
    return load_script_module("root_maintenance.py", "root_maintenance")


def load_toml(path: Path) -> dict[str, object]:
    return tomllib.loads(path.read_text(encoding="utf-8"))
