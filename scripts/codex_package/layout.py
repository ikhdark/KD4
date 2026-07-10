"""Canonical Codex package directory layout."""

import json
import inspect
import os
import platform
import shutil
import stat
from pathlib import Path
from pathlib import PureWindowsPath

from .targets import PackageInputs
from .targets import PackageVariant
from .targets import TargetSpec
from .zsh import ZSH_RESOURCE_PATH


LAYOUT_VERSION = 1
APPLY_PATCH_ALIASES = ("apply_patch", "applypatch")
CODEX_CORE_APPLY_PATCH_ARG1 = "--codex-run-as-apply-patch"
MANAGED_PACKAGE_PATHS = (
    Path("bin"),
    Path("codex-resources"),
    Path("codex-path"),
    Path("codex-package.json"),
)


def prepare_package_dir(package_dir: Path, *, force: bool, reuse: bool = False) -> None:
    validate_package_dir_destination(package_dir, force=force, reuse=reuse)
    if package_dir.exists():
        if reuse:
            clean_managed_package_paths(package_dir)
        elif any(package_dir.iterdir()):
            remove_tree_allow_readonly(package_dir)

    package_dir.mkdir(parents=True, exist_ok=True)


def validate_package_dir_destination(
    package_dir: Path,
    *,
    force: bool,
    reuse: bool = False,
) -> None:
    if not package_dir.exists():
        return
    if not package_dir.is_dir():
        raise RuntimeError(
            f"Package output exists and is not a directory: {package_dir}"
        )
    if any(package_dir.iterdir()) and not (force or reuse):
        raise RuntimeError(
            f"Package output directory is not empty: {package_dir}. "
            "Pass --force to replace it."
        )


def clean_managed_package_paths(package_dir: Path) -> None:
    for relative_path in MANAGED_PACKAGE_PATHS:
        path = package_dir / relative_path
        if path.is_dir():
            remove_tree_allow_readonly(path)
        else:
            path.unlink(missing_ok=True)


def remove_tree_allow_readonly(path: Path) -> None:
    # Windows rmtree aborts on read-only files (e.g. git pack files); clear
    # the attribute and retry.
    def _retry_after_chmod(func, failed_path):
        os.chmod(failed_path, stat.S_IWRITE)
        func(failed_path)

    def _onexc(func, failed_path, _exc):
        _retry_after_chmod(func, failed_path)

    def _onerror(func, failed_path, _exc_info):
        _retry_after_chmod(func, failed_path)

    if rmtree_supports_onexc():
        shutil.rmtree(path, onexc=_onexc)
    else:
        shutil.rmtree(path, onerror=_onerror)


def rmtree_supports_onexc() -> bool:
    return "onexc" in inspect.signature(shutil.rmtree).parameters


def build_package_dir(
    package_dir: Path,
    version: str,
    variant: PackageVariant,
    spec: TargetSpec,
    inputs: PackageInputs,
) -> None:
    bin_dir = package_dir / "bin"
    resources_dir = package_dir / "codex-resources"
    path_dir = package_dir / "codex-path"
    bin_dir.mkdir(exist_ok=True)
    resources_dir.mkdir(exist_ok=True)
    path_dir.mkdir(exist_ok=True)

    entrypoint_name = variant.entrypoint_name(spec)
    copy_executable(
        inputs.entrypoint_bin,
        bin_dir / entrypoint_name,
        is_windows=spec.is_windows,
    )
    copy_executable(
        inputs.code_mode_host_bin,
        bin_dir / spec.code_mode_host_name,
        is_windows=spec.is_windows,
    )
    copy_executable(
        inputs.rg_bin,
        path_dir / spec.rg_name,
        is_windows=spec.is_windows,
        prefer_hardlink=True,
    )
    if spec.is_windows:
        for alias in APPLY_PATCH_ALIASES:
            write_windows_apply_patch_alias(
                path_dir / f"{alias}.bat",
                PureWindowsPath("..") / "bin" / entrypoint_name,
            )

    if inputs.zsh_bin is not None:
        copy_executable(
            inputs.zsh_bin,
            resources_dir / ZSH_RESOURCE_PATH,
            is_windows=False,
        )

    if inputs.bwrap_bin is not None:
        copy_executable(inputs.bwrap_bin, resources_dir / "bwrap", is_windows=False)

    if inputs.codex_command_runner_bin is not None:
        copy_executable(
            inputs.codex_command_runner_bin,
            resources_dir / "codex-command-runner.exe",
            is_windows=True,
        )

    if inputs.codex_windows_sandbox_setup_bin is not None:
        copy_executable(
            inputs.codex_windows_sandbox_setup_bin,
            resources_dir / "codex-windows-sandbox-setup.exe",
            is_windows=True,
        )

    metadata = {
        "layoutVersion": LAYOUT_VERSION,
        "version": version,
        "target": spec.target,
        "variant": variant.name,
        "entrypoint": f"bin/{entrypoint_name}",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
    }
    write_json(package_dir / "codex-package.json", metadata)


def validate_package_dir(
    package_dir: Path,
    variant: PackageVariant,
    spec: TargetSpec,
    *,
    expected_version: str | None = None,
    include_zsh: bool,
    fast: bool = False,
) -> None:
    required_dirs = [
        Path("bin"),
        Path("codex-resources"),
        Path("codex-path"),
    ]
    for relative_dir in required_dirs:
        path = package_dir / relative_dir
        if not path.is_dir():
            raise RuntimeError(f"Missing package directory: {relative_dir}")

    metadata_path = package_dir / "codex-package.json"
    if not metadata_path.is_file():
        raise RuntimeError("Missing package metadata: codex-package.json")

    with open(metadata_path, encoding="utf-8") as fh:
        metadata = json.load(fh)

    version = metadata.get("version")
    if not isinstance(version, str) or not version:
        raise RuntimeError(
            f"Invalid package metadata field 'version': expected a non-empty string, got {version!r}"
        )

    expected_metadata = {
        "layoutVersion": LAYOUT_VERSION,
        "target": spec.target,
        "variant": variant.name,
        "entrypoint": f"bin/{variant.entrypoint_name(spec)}",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
    }
    if expected_version is not None:
        expected_metadata["version"] = expected_version
    for key, expected in expected_metadata.items():
        actual = metadata.get(key)
        if actual != expected:
            raise RuntimeError(
                f"Invalid package metadata field {key!r}: expected {expected!r}, got {actual!r}"
            )

    required_files = [
        Path("bin") / variant.entrypoint_name(spec),
        Path("bin") / spec.code_mode_host_name,
        Path("codex-path") / spec.rg_name,
    ]
    executable_files = list(required_files)

    if include_zsh:
        zsh_path = Path("codex-resources") / ZSH_RESOURCE_PATH
        required_files.append(zsh_path)
        executable_files.append(zsh_path)

    if spec.is_linux:
        required_files.append(Path("codex-resources") / "bwrap")
        executable_files.append(Path("codex-resources") / "bwrap")

    if spec.is_windows:
        required_files.extend(
            [
                Path("codex-resources") / "codex-command-runner.exe",
                Path("codex-resources") / "codex-windows-sandbox-setup.exe",
                *[Path("codex-path") / f"{alias}.bat" for alias in APPLY_PATCH_ALIASES],
            ]
        )

    for relative_file in required_files:
        path = package_dir / relative_file
        if not path.is_file():
            raise RuntimeError(f"Missing package file: {relative_file}")

    if spec.is_windows:
        expected_alias_text = windows_apply_patch_alias_text(
            PureWindowsPath("..") / "bin" / variant.entrypoint_name(spec)
        )
        for alias in APPLY_PATCH_ALIASES:
            relative_file = Path("codex-path") / f"{alias}.bat"
            actual = (package_dir / relative_file).read_text(encoding="utf-8")
            if actual != expected_alias_text:
                raise RuntimeError(f"Invalid package file contents: {relative_file}")

    if not spec.is_windows and not fast:
        for relative_file in executable_files:
            path = package_dir / relative_file
            if not is_executable(path):
                raise RuntimeError(f"Package file is not executable: {relative_file}")


def copy_executable(
    src: Path,
    dest: Path,
    *,
    is_windows: bool,
    prefer_hardlink: bool | None = None,
) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    copy_file_for_staging(
        src,
        dest,
        prefer_hardlink=is_windows if prefer_hardlink is None else prefer_hardlink,
    )
    if not is_windows:
        mode = dest.stat().st_mode
        dest.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def write_windows_apply_patch_alias(
    path: Path, entrypoint_relative_path: PureWindowsPath
) -> None:
    path.write_text(
        windows_apply_patch_alias_text(entrypoint_relative_path),
        encoding="utf-8",
    )


def windows_apply_patch_alias_text(entrypoint_relative_path: PureWindowsPath) -> str:
    return "\n".join(
        [
            "@echo off",
            f'"%~dp0{entrypoint_relative_path}" {CODEX_CORE_APPLY_PATCH_ARG1} %*',
            "",
        ]
    )


def copy_file_for_staging(src: Path, dest: Path, *, prefer_hardlink: bool) -> None:
    dest.unlink(missing_ok=True)
    if prefer_hardlink:
        try:
            os.link(src, dest)
            return
        except OSError:
            pass
    shutil.copyfile(src, dest)


def write_json(path: Path, value: object) -> None:
    with open(path, "w", encoding="utf-8") as out:
        json.dump(value, out, indent=2)
        out.write("\n")


def is_executable(path: Path) -> bool:
    if platform.system().lower() == "windows":
        return path.is_file()

    return bool(path.stat().st_mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))
