"""Canonical Codex package directory layout."""

import hashlib
import inspect
import json
import os
import platform
import re
import shutil
import stat
import struct
import subprocess
from pathlib import Path
from pathlib import PureWindowsPath

from .targets import REPO_ROOT
from .targets import PackageInputs
from .targets import PackageVariant
from .targets import TargetSpec


LAYOUT_VERSION = 2
APPLY_PATCH_ALIASES = ("apply_patch", "applypatch")
CODEX_CORE_APPLY_PATCH_ARG1 = "--codex-run-as-apply-patch"
MANAGED_PACKAGE_PATHS = (
    Path("bin"),
    Path("codex-resources"),
    Path("codex-path"),
    Path("codex-package.json"),
    Path("LICENSE"),
    Path("NOTICE"),
)


def prepare_package_dir(package_dir: Path, *, force: bool, reuse: bool = False) -> None:
    validate_package_dir_destination(package_dir, force=force, reuse=reuse)
    if package_dir.exists():
        if reuse:
            clean_package_dir(package_dir)
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


def clean_package_dir(package_dir: Path) -> None:
    for path in package_dir.iterdir():
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


def validate_package_input_roles(inputs: PackageInputs) -> None:
    """Reject one executable being assigned to multiple package roles."""
    role_paths = (
        ("entrypoint", inputs.entrypoint_bin),
        ("code-mode host", inputs.code_mode_host_bin),
        ("ripgrep", inputs.rg_bin),
        ("Windows command runner", inputs.codex_command_runner_bin),
        ("Windows sandbox setup", inputs.codex_windows_sandbox_setup_bin),
    )
    resolved: list[tuple[str, Path]] = []
    for role, path in role_paths:
        if path is None:
            continue
        canonical = path.resolve(strict=True)
        for prior_role, prior_path in resolved:
            if canonical == prior_path or canonical.samefile(prior_path):
                raise RuntimeError(
                    f"Package roles must use distinct executables: {role} and "
                    f"{prior_role} both resolve to {canonical}"
                )
        resolved.append((role, canonical))


def build_package_dir(
    package_dir: Path,
    version: str,
    variant: PackageVariant,
    spec: TargetSpec,
    inputs: PackageInputs,
    *,
    build_identity: dict[str, object] | None = None,
) -> None:
    validate_package_input_roles(inputs)
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
    )
    copy_executable(
        inputs.code_mode_host_bin,
        bin_dir / spec.code_mode_host_name,
    )
    copy_executable(
        inputs.rg_bin,
        path_dir / spec.rg_name,
        prefer_hardlink=True,
    )
    for alias in APPLY_PATCH_ALIASES:
        write_windows_apply_patch_alias(
            path_dir / f"{alias}.bat",
            PureWindowsPath("..") / "bin" / entrypoint_name,
        )

    if inputs.codex_command_runner_bin is not None:
        copy_executable(
            inputs.codex_command_runner_bin,
            resources_dir / "codex-command-runner.exe",
        )

    if inputs.codex_windows_sandbox_setup_bin is not None:
        copy_executable(
            inputs.codex_windows_sandbox_setup_bin,
            resources_dir / "codex-windows-sandbox-setup.exe",
        )

    shutil.copyfile(REPO_ROOT / "LICENSE", package_dir / "LICENSE")
    shutil.copyfile(REPO_ROOT / "NOTICE", package_dir / "NOTICE")

    files = package_file_inventory(package_dir, variant=variant, spec=spec)
    bundle_id = hashlib.sha256(
        json.dumps(files, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()

    metadata = {
        "layoutVersion": LAYOUT_VERSION,
        "version": version,
        "target": spec.target,
        "variant": variant.name,
        "entrypoint": f"bin/{entrypoint_name}",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
        "bundleId": bundle_id,
        "buildIdentity": build_identity or {"status": "unavailable"},
        "files": files,
    }
    write_json(package_dir / "codex-package.json", metadata)


def validate_package_dir(
    package_dir: Path,
    variant: PackageVariant,
    spec: TargetSpec,
    *,
    expected_version: str | None = None,
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

    files = metadata.get("files")
    if not isinstance(files, list) or not files:
        raise RuntimeError("Invalid package metadata field 'files'")
    declared_paths: set[str] = set()
    for entry in files:
        if not isinstance(entry, dict):
            raise RuntimeError("Invalid package file inventory entry")
        relative = entry.get("path")
        if not isinstance(relative, str) or not relative or relative in declared_paths:
            raise RuntimeError(f"Invalid package file inventory path: {relative!r}")
        relative_path = Path(relative)
        if relative_path.is_absolute() or ".." in relative_path.parts:
            raise RuntimeError(f"Unsafe package file inventory path: {relative}")
        declared_paths.add(relative)
        path = package_dir / relative_path
        if not path.is_file():
            raise RuntimeError(f"Missing package file: {relative}")
        actual_size = path.stat().st_size
        actual_sha256 = sha256_file(path)
        if entry.get("size") != actual_size or entry.get("sha256") != actual_sha256:
            raise RuntimeError(f"Package file digest mismatch: {relative}")

    actual_paths = {
        path.relative_to(package_dir).as_posix()
        for path in package_dir.rglob("*")
        if path.is_file() and path.name != "codex-package.json"
    }
    if actual_paths != declared_paths:
        unexpected = sorted(actual_paths - declared_paths)
        missing = sorted(declared_paths - actual_paths)
        raise RuntimeError(
            f"Package inventory mismatch: unexpected={unexpected}, missing={missing}"
        )

    expected_bundle_id = hashlib.sha256(
        json.dumps(files, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()
    if metadata.get("bundleId") != expected_bundle_id:
        raise RuntimeError("Invalid package metadata field 'bundleId'")
    if (
        not isinstance(metadata.get("buildIdentity"), dict)
        or not metadata["buildIdentity"]
    ):
        raise RuntimeError("Invalid package metadata field 'buildIdentity'")

    validate_pe_targets(package_dir, files, spec)
    validate_host_entrypoint_version(
        package_dir / str(metadata["entrypoint"]), spec, version
    )

    expected_alias_text = windows_apply_patch_alias_text(
        PureWindowsPath("..") / "bin" / variant.entrypoint_name(spec)
    )
    for alias in APPLY_PATCH_ALIASES:
        relative_file = Path("codex-path") / f"{alias}.bat"
        actual = (package_dir / relative_file).read_text(encoding="utf-8")
        if actual != expected_alias_text:
            raise RuntimeError(f"Invalid package file contents: {relative_file}")


def copy_executable(
    src: Path,
    dest: Path,
    *,
    prefer_hardlink: bool | None = None,
) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    copy_file_for_staging(
        src,
        dest,
        prefer_hardlink=False,
    )


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
    _ = prefer_hardlink
    dest.unlink(missing_ok=True)
    shutil.copyfile(src, dest)


def write_json(path: Path, value: object) -> None:
    with open(path, "w", encoding="utf-8") as out:
        json.dump(value, out, indent=2)
        out.write("\n")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def package_file_inventory(
    package_dir: Path, *, variant: PackageVariant, spec: TargetSpec
) -> list[dict[str, object]]:
    roles = {
        f"bin/{variant.entrypoint_name(spec)}": "entrypoint",
        f"bin/{spec.code_mode_host_name}": "code-mode-host",
        "codex-resources/codex-command-runner.exe": "command-runner",
        "codex-resources/codex-windows-sandbox-setup.exe": "sandbox-setup",
        f"codex-path/{spec.rg_name}": "ripgrep",
        "codex-path/apply_patch.bat": "apply-patch-alias",
        "codex-path/applypatch.bat": "apply-patch-alias",
        "LICENSE": "license",
        "NOTICE": "notice",
    }
    inventory = []
    for path in sorted(package_dir.rglob("*")):
        if not path.is_file() or path.name == "codex-package.json":
            continue
        relative = path.relative_to(package_dir).as_posix()
        inventory.append(
            {
                "path": relative,
                "role": roles.get(relative, "resource"),
                "size": path.stat().st_size,
                "sha256": sha256_file(path),
            }
        )
    return inventory


def pe_machine(path: Path) -> int | None:
    with path.open("rb") as file:
        if file.read(2) != b"MZ":
            return None
        file.seek(0x3C)
        offset_bytes = file.read(4)
        if len(offset_bytes) != 4:
            raise RuntimeError(f"Invalid PE executable: {path}")
        file.seek(struct.unpack("<I", offset_bytes)[0])
        if file.read(4) != b"PE\0\0":
            raise RuntimeError(f"Invalid PE executable: {path}")
        machine = file.read(2)
        if len(machine) != 2:
            raise RuntimeError(f"Invalid PE executable: {path}")
        return struct.unpack("<H", machine)[0]


def validate_pe_targets(
    package_dir: Path, files: list[dict[str, object]], spec: TargetSpec
) -> None:
    expected_machine = {
        "x86_64-pc-windows-msvc": 0x8664,
        "aarch64-pc-windows-msvc": 0xAA64,
    }.get(spec.target)
    if expected_machine is None:
        return
    for entry in files:
        relative = str(entry["path"])
        if not relative.lower().endswith(".exe"):
            continue
        machine = pe_machine(package_dir / relative)
        if machine is not None and machine != expected_machine:
            raise RuntimeError(
                f"Package executable target mismatch: {relative} has PE machine "
                f"0x{machine:04x}, expected 0x{expected_machine:04x}"
            )


def validate_host_entrypoint_version(
    entrypoint: Path, spec: TargetSpec, expected_version: str
) -> None:
    host_target = {
        "amd64": "x86_64-pc-windows-msvc",
        "x86_64": "x86_64-pc-windows-msvc",
        "arm64": "aarch64-pc-windows-msvc",
        "aarch64": "aarch64-pc-windows-msvc",
    }.get(platform.machine().lower())
    if os.name != "nt" or host_target != spec.target or pe_machine(entrypoint) is None:
        return
    try:
        completed = subprocess.run(
            [entrypoint, "--version"],
            check=True,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(
            f"Packaged entrypoint failed --version: {entrypoint}"
        ) from error
    version_match = re.search(r"([0-9][0-9A-Za-z.+-]*)\s*$", completed.stdout.strip())
    actual_version = version_match.group(1) if version_match else None
    if actual_version != expected_version:
        raise RuntimeError(
            "Packaged entrypoint version mismatch: "
            f"expected {expected_version}, got {actual_version!r}"
        )
