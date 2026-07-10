"""Fetch the patched zsh fork used by shell_zsh_fork."""

from functools import cache
from pathlib import Path

from .dotslash import fetch_dotslash_executable
from .targets import REPO_ROOT
from .targets import TARGET_SPECS
from .targets import TargetSpec
from .targets import resolve_input_path


ZSH_MANIFEST = REPO_ROOT / "scripts" / "codex_package" / "codex-zsh"
ZSH_RESOURCE_PATH = Path("zsh") / "bin" / "zsh"
ZSH_ARTIFACT_LABEL = "codex-zsh"
ZSH_DEST_NAME = "zsh"
ZSH_CACHE_SUFFIX = "zsh"
ZSH_DOTSLASH_PLATFORMS = frozenset(
    {
        "linux-aarch64",
        "linux-x86_64",
        "macos-aarch64",
        "macos-x86_64",
    }
)


def resolve_zsh_bin(
    spec: TargetSpec,
    explicit_path: Path | None = None,
    *,
    manifest_path: Path | None = None,
) -> Path | None:
    if not supports_zsh(spec):
        return None
    if explicit_path is not None and manifest_path is not None:
        raise RuntimeError("--zsh-bin and --zsh-manifest cannot be used together.")
    if explicit_path is not None:
        return resolve_explicit_zsh_bin(explicit_path)
    return resolve_zsh_bin_for_target(
        spec.target,
        manifest_path or ZSH_MANIFEST,
        missing_ok=manifest_path is None,
    )


def supports_zsh(spec: TargetSpec) -> bool:
    return not spec.is_windows and spec.dotslash_platform in ZSH_DOTSLASH_PLATFORMS


@cache
def resolve_zsh_bin_for_target(
    target: str,
    manifest_path: Path = ZSH_MANIFEST,
    *,
    missing_ok: bool = True,
) -> Path | None:
    spec = TARGET_SPECS[target]
    if not supports_zsh(spec):
        return None
    return fetch_dotslash_executable(
        spec,
        manifest_path=manifest_path,
        artifact_label=ZSH_ARTIFACT_LABEL,
        cache_key=zsh_cache_key(target),
        dest_name=ZSH_DEST_NAME,
        missing_ok=missing_ok,
    )


def resolve_explicit_zsh_bin(path: Path) -> Path:
    return resolve_input_path(path, "zsh executable", "--zsh-bin")


def zsh_cache_key(target: str) -> str:
    return f"{target}-{ZSH_CACHE_SUFFIX}"
