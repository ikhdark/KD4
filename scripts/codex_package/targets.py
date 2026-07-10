"""Supported package targets and default binary discovery."""

import platform
import stat
from dataclasses import dataclass
from dataclasses import field
from functools import cache
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parents[1]
REPO_ROOT = SCRIPT_DIR.parent
CODE_MODE_HOST_STEM = "codex-code-mode-host"


MACHINE_ALIASES: dict[str, str] = {
    "amd64": "x86_64",
    "x86_64": "x86_64",
    "aarch64": "aarch64",
    "arm64": "aarch64",
}


@dataclass(frozen=True, slots=True)
class TargetSpec:
    target: str
    is_windows: bool
    is_linux: bool
    dotslash_platform: str
    exe_suffix: str = field(init=False)
    rg_name: str = field(init=False)
    code_mode_host_name: str = field(init=False)

    def __post_init__(self) -> None:
        exe_suffix = ".exe" if self.is_windows else ""
        object.__setattr__(self, "exe_suffix", exe_suffix)
        object.__setattr__(self, "rg_name", f"rg{exe_suffix}")
        object.__setattr__(
            self,
            "code_mode_host_name",
            f"{CODE_MODE_HOST_STEM}{exe_suffix}",
        )


@dataclass(frozen=True, slots=True)
class ReleaseTarget:
    target: str
    npm_tag: str
    platform_label: str
    host_system: str
    host_machine: str

    @property
    def package_asset_prefix(self) -> str:
        return f"codex-package-{self.target}"

    def legacy_npm_asset(self, version: str) -> str:
        return f"codex-npm-{self.npm_tag}-{version}.tgz"


@dataclass(frozen=True, slots=True)
class PackageVariant:
    name: str
    cargo_bin: str
    executable_stem: str

    def entrypoint_name(self, spec: TargetSpec) -> str:
        target_entrypoints = PACKAGE_ENTRYPOINT_NAMES.get(self.name)
        if target_entrypoints is not None:
            entrypoint = target_entrypoints.get(spec.target)
            if entrypoint is not None:
                return entrypoint

        return f"{self.executable_stem}{spec.exe_suffix}"


@dataclass(frozen=True, slots=True)
class PackageInputs:
    entrypoint_bin: Path
    code_mode_host_bin: Path
    rg_bin: Path
    zsh_bin: Path | None
    bwrap_bin: Path | None
    codex_command_runner_bin: Path | None
    codex_windows_sandbox_setup_bin: Path | None


PACKAGE_VARIANTS: dict[str, PackageVariant] = {
    "codex": PackageVariant(
        name="codex",
        cargo_bin="codex",
        executable_stem="codex",
    ),
    "codex-app-server": PackageVariant(
        name="codex-app-server",
        cargo_bin="codex-app-server",
        executable_stem="codex-app-server",
    ),
}


TARGET_SPECS: dict[str, TargetSpec] = {
    "x86_64-unknown-linux-gnu": TargetSpec(
        target="x86_64-unknown-linux-gnu",
        is_windows=False,
        is_linux=True,
        dotslash_platform="linux-x86_64",
    ),
    "x86_64-unknown-linux-musl": TargetSpec(
        target="x86_64-unknown-linux-musl",
        is_windows=False,
        is_linux=True,
        dotslash_platform="linux-x86_64",
    ),
    "aarch64-unknown-linux-gnu": TargetSpec(
        target="aarch64-unknown-linux-gnu",
        is_windows=False,
        is_linux=True,
        dotslash_platform="linux-aarch64",
    ),
    "aarch64-unknown-linux-musl": TargetSpec(
        target="aarch64-unknown-linux-musl",
        is_windows=False,
        is_linux=True,
        dotslash_platform="linux-aarch64",
    ),
    "x86_64-apple-darwin": TargetSpec(
        target="x86_64-apple-darwin",
        is_windows=False,
        is_linux=False,
        dotslash_platform="macos-x86_64",
    ),
    "aarch64-apple-darwin": TargetSpec(
        target="aarch64-apple-darwin",
        is_windows=False,
        is_linux=False,
        dotslash_platform="macos-aarch64",
    ),
    "x86_64-pc-windows-msvc": TargetSpec(
        target="x86_64-pc-windows-msvc",
        is_windows=True,
        is_linux=False,
        dotslash_platform="windows-x86_64",
    ),
    "aarch64-pc-windows-msvc": TargetSpec(
        target="aarch64-pc-windows-msvc",
        is_windows=True,
        is_linux=False,
        dotslash_platform="windows-aarch64",
    ),
}


RELEASE_TARGETS: dict[str, ReleaseTarget] = {
    "x86_64-unknown-linux-musl": ReleaseTarget(
        target="x86_64-unknown-linux-musl",
        npm_tag="linux-x64",
        platform_label="Linux (x64)",
        host_system="linux",
        host_machine="x86_64",
    ),
    "aarch64-unknown-linux-musl": ReleaseTarget(
        target="aarch64-unknown-linux-musl",
        npm_tag="linux-arm64",
        platform_label="Linux (ARM64)",
        host_system="linux",
        host_machine="aarch64",
    ),
    "x86_64-apple-darwin": ReleaseTarget(
        target="x86_64-apple-darwin",
        npm_tag="darwin-x64",
        platform_label="macOS (Intel)",
        host_system="darwin",
        host_machine="x86_64",
    ),
    "aarch64-apple-darwin": ReleaseTarget(
        target="aarch64-apple-darwin",
        npm_tag="darwin-arm64",
        platform_label="macOS (Apple Silicon)",
        host_system="darwin",
        host_machine="aarch64",
    ),
    "x86_64-pc-windows-msvc": ReleaseTarget(
        target="x86_64-pc-windows-msvc",
        npm_tag="win32-x64",
        platform_label="Windows (x64)",
        host_system="windows",
        host_machine="x86_64",
    ),
    "aarch64-pc-windows-msvc": ReleaseTarget(
        target="aarch64-pc-windows-msvc",
        npm_tag="win32-arm64",
        platform_label="Windows (ARM64)",
        host_system="windows",
        host_machine="aarch64",
    ),
}
BINARY_TARGETS: tuple[str, ...] = tuple(RELEASE_TARGETS)
SUPPORTED_TARGETS: tuple[str, ...] = tuple(sorted(TARGET_SPECS))
SUPPORTED_VARIANTS: tuple[str, ...] = tuple(sorted(PACKAGE_VARIANTS))
PACKAGE_ENTRYPOINT_NAMES: dict[str, dict[str, str]] = {
    variant_name: {
        target_name: f"{variant.executable_stem}{spec.exe_suffix}"
        for target_name, spec in TARGET_SPECS.items()
    }
    for variant_name, variant in PACKAGE_VARIANTS.items()
}


HOST_RELEASE_TARGETS: dict[tuple[str, str], str] = {
    (release.host_system, release.host_machine): target
    for target, release in RELEASE_TARGETS.items()
}


@cache
def default_target() -> str:
    system_name = platform.system()
    machine_name = platform.machine()
    system = system_name.lower()
    machine = normalize_machine(machine_name)
    target = HOST_RELEASE_TARGETS.get((system, machine))
    if target is None:
        supported = ", ".join(SUPPORTED_TARGETS)
        raise RuntimeError(
            f"Unsupported host platform {system_name}/{machine_name}. "
            f"Pass --target explicitly. Supported targets: {supported}"
        )
    return target


def resolve_input_path(
    explicit_path: Path | None,
    description: str,
    flag_name: str,
    *,
    canonicalize: bool = True,
) -> Path:
    if explicit_path is not None:
        path = explicit_path
        if not path.is_file():
            raise RuntimeError(f"{description} does not exist: {explicit_path}")
        if not is_executable(path):
            raise RuntimeError(f"{description} is not executable: {path}")
        if canonicalize:
            path = path.resolve()
        return path

    raise RuntimeError(f"Must specify {flag_name} for {description}.")


def is_executable(path: Path) -> bool:
    # On a Windows HOST, stat() never reports execute bits for extension-less
    # files, so a prebuilt Linux/mac binary staged cross-target would always
    # be rejected. Existence is the only meaningful check there (mirrors
    # layout.is_executable).
    if platform.system().lower() == "windows":
        return path.is_file()

    return bool(path.stat().st_mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))


def normalize_machine(machine: str) -> str:
    normalized = machine.lower()
    return MACHINE_ALIASES.get(normalized, normalized)
