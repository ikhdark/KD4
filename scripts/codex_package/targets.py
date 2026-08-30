"""Supported package targets and default binary discovery."""

import platform
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
    dotslash_platform: str
    rg_name: str = field(init=False)
    code_mode_host_name: str = field(init=False)

    def __post_init__(self) -> None:
        object.__setattr__(self, "rg_name", "rg.exe")
        object.__setattr__(
            self,
            "code_mode_host_name",
            f"{CODE_MODE_HOST_STEM}.exe",
        )


@dataclass(frozen=True, slots=True)
class ReleaseTarget:
    target: str
    platform_label: str
    host_system: str
    host_machine: str

    @property
    def package_asset_prefix(self) -> str:
        return f"codex-package-{self.target}"


@dataclass(frozen=True, slots=True)
class NpmTarget:
    package: str
    npm_name: str
    npm_tag: str
    node_platform: str
    node_arch: str
    executable_name: str

    @property
    def node_platform_key(self) -> str:
        return f"{self.node_platform}-{self.node_arch}"


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

        return f"{self.executable_stem}.exe"


@dataclass(frozen=True, slots=True)
class PackageInputs:
    entrypoint_bin: Path
    code_mode_host_bin: Path
    rg_bin: Path
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
    "x86_64-pc-windows-msvc": TargetSpec(
        target="x86_64-pc-windows-msvc",
        dotslash_platform="windows-x86_64",
    ),
    "aarch64-pc-windows-msvc": TargetSpec(
        target="aarch64-pc-windows-msvc",
        dotslash_platform="windows-aarch64",
    ),
}


RELEASE_TARGETS: dict[str, ReleaseTarget] = {
    "x86_64-pc-windows-msvc": ReleaseTarget(
        target="x86_64-pc-windows-msvc",
        platform_label="Windows (x64)",
        host_system="windows",
        host_machine="x86_64",
    ),
    "aarch64-pc-windows-msvc": ReleaseTarget(
        target="aarch64-pc-windows-msvc",
        platform_label="Windows (ARM64)",
        host_system="windows",
        host_machine="aarch64",
    ),
}
NPM_TARGETS: dict[str, NpmTarget] = {
    "x86_64-pc-windows-msvc": NpmTarget(
        package="codex-win32-x64",
        npm_name="@openai/codex-win32-x64",
        npm_tag="win32-x64",
        node_platform="win32",
        node_arch="x64",
        executable_name="codex.exe",
    ),
    "aarch64-pc-windows-msvc": NpmTarget(
        package="codex-win32-arm64",
        npm_name="@openai/codex-win32-arm64",
        npm_tag="win32-arm64",
        node_platform="win32",
        node_arch="arm64",
        executable_name="codex.exe",
    ),
}
BINARY_TARGETS: tuple[str, ...] = tuple(RELEASE_TARGETS)
SUPPORTED_TARGETS: tuple[str, ...] = tuple(sorted(TARGET_SPECS))
SUPPORTED_VARIANTS: tuple[str, ...] = tuple(sorted(PACKAGE_VARIANTS))
PACKAGE_ENTRYPOINT_NAMES: dict[str, dict[str, str]] = {
    variant_name: {
        target_name: f"{variant.executable_stem}.exe"
        for target_name, spec in TARGET_SPECS.items()
    }
    for variant_name, variant in PACKAGE_VARIANTS.items()
}


HOST_RELEASE_TARGETS: dict[str, str] = {
    release.host_machine: target for target, release in RELEASE_TARGETS.items()
}


@cache
def default_target() -> str:
    machine_name = platform.machine()
    machine = normalize_machine(machine_name)
    target = HOST_RELEASE_TARGETS.get(machine)
    if target is None:
        supported = ", ".join(SUPPORTED_TARGETS)
        raise RuntimeError(
            f"Unsupported Windows architecture {machine_name}. "
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
    return path.is_file()


def normalize_machine(machine: str) -> str:
    normalized = machine.lower()
    return MACHINE_ALIASES.get(normalized, normalized)
