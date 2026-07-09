"""Command-line interface for building Codex package directories."""

import argparse
import tempfile
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from pathlib import Path
from time import perf_counter

from .archive import package_entries
from .archive import write_archive
from .cargo import SourceBuildOutputs
from .cargo import build_source_binaries
from .cargo import cargo_package_target_dir
from .cargo import cargo_profile_output_dir
from .cargo import validate_source_outputs
from .layout import build_package_dir
from .layout import prepare_package_dir
from .layout import validate_package_dir
from .ripgrep import resolve_rg_bin
from .targets import PACKAGE_VARIANTS
from .targets import SUPPORTED_TARGETS
from .targets import SUPPORTED_VARIANTS
from .targets import TARGET_SPECS
from .targets import PackageInputs
from .targets import default_target
from .targets import resolve_input_path
from .zsh import resolve_zsh_bin
from .zsh import supports_zsh
from .version import read_workspace_version


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build a canonical Codex package directory and optional archive.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--target",
        default=argparse.SUPPRESS,
        choices=SUPPORTED_TARGETS,
        help=(
            "Rust target triple for the package. Defaults to the release target "
            "for this host platform."
        ),
    )
    parser.add_argument(
        "--variant",
        choices=SUPPORTED_VARIANTS,
        default="codex",
        help="Package variant to build.",
    )
    parser.add_argument(
        "--package-dir",
        type=Path,
        default=argparse.SUPPRESS,
        help=(
            "Output directory to create as the package root. Defaults to a new temporary directory."
        ),
    )
    parser.add_argument(
        "--archive-output",
        type=Path,
        action="append",
        default=[],
        help=(
            "Optional archive output path. May be repeated. Supported suffixes: "
            ".tar.gz, .tgz, .tar.zst, .zip."
        ),
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Replace an existing package directory or archive output.",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="Cargo executable to use for source-built package artifacts.",
    )
    parser.add_argument(
        "--cargo-profile",
        default="dev-small",
        help=(
            "Cargo profile for source-built package artifacts. Use release for release packages."
        ),
    )
    parser.add_argument(
        "--entrypoint-bin",
        type=Path,
        help=(
            "Optional prebuilt entrypoint executable for the selected package "
            "variant. If omitted, the entrypoint is built with Cargo."
        ),
    )
    parser.add_argument(
        "--bwrap-bin",
        type=Path,
        help=(
            "Optional prebuilt Linux bwrap executable. If omitted for Linux "
            "targets, bwrap is built with Cargo."
        ),
    )
    parser.add_argument(
        "--codex-command-runner-bin",
        type=Path,
        help=(
            "Optional prebuilt Windows codex-command-runner.exe executable. "
            "If omitted for Windows targets, codex-command-runner is built "
            "with Cargo."
        ),
    )
    parser.add_argument(
        "--codex-windows-sandbox-setup-bin",
        type=Path,
        help=(
            "Optional prebuilt Windows codex-windows-sandbox-setup.exe "
            "executable. If omitted for Windows targets, "
            "codex-windows-sandbox-setup is built with Cargo."
        ),
    )
    parser.add_argument(
        "--rg-bin",
        type=Path,
        help=(
            "Optional local ripgrep executable override instead of fetching from "
            "scripts/codex_package/rg."
        ),
    )
    parser.add_argument(
        "--zsh-bin",
        type=Path,
        help=(
            "Optional local patched zsh executable override instead of fetching from "
            "scripts/codex_package/codex-zsh."
        ),
    )
    parser.add_argument(
        "--reuse-source-builds",
        action="store_true",
        help=(
            "Reuse already-built Cargo package binaries from the package target "
            "lane when all expected outputs exist."
        ),
    )
    parser.add_argument(
        "--skip-build-if-present",
        action="store_true",
        help=(
            "Skip Cargo when all expected source-built binaries already exist for "
            "the selected target/profile."
        ),
    )
    parser.add_argument(
        "--force-source-rebuild",
        action="store_true",
        help="Force Cargo package binary rebuilds even with --reuse-source-builds.",
    )
    parser.add_argument(
        "--skip-validate",
        action="store_true",
        help="Skip package layout validation after copying files.",
    )
    parser.add_argument(
        "--fast-validate",
        action="store_true",
        help="Run validation without slower executable-bit checks.",
    )
    parser.add_argument(
        "--reuse-package-dir",
        action="store_true",
        help="Allow copying into an existing non-empty package directory.",
    )
    parser.add_argument(
        "--archive-compression",
        choices=["default", "fast", "none"],
        default="fast",
        help="Compression effort for generated archives.",
    )
    parser.add_argument(
        "--timings",
        action="store_true",
        help="Print coarse timing spans for package build phases.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    spec = TARGET_SPECS[getattr(args, "target", None) or default_target()]
    variant = PACKAGE_VARIANTS[args.variant]
    package_dir_arg = getattr(args, "package_dir", None)
    package_dir = (
        package_dir_arg.resolve()
        if package_dir_arg is not None
        else Path(tempfile.mkdtemp(prefix="codex-package-")).resolve()
    )

    timings = getattr(args, "timings", False)
    with timed_step("inputs", timings):
        version, inputs = resolve_package_inputs(args, spec, variant)
    with timed_step("package-dir", timings):
        prepare_package_dir(
            package_dir,
            force=args.force,
            reuse=getattr(args, "reuse_package_dir", False),
        )
        build_package_dir(package_dir, version, variant, spec, inputs)
    if not getattr(args, "skip_validate", False):
        with timed_step("validate", timings):
            validate_package_dir(
                package_dir,
                variant,
                spec,
                expected_version=version,
                include_zsh=inputs.zsh_bin is not None,
                fast=getattr(args, "fast_validate", False),
            )

    archive_entries = None
    if args.archive_output:
        with timed_step("archive-entries", timings):
            archive_entries = package_entries(package_dir)
    for archive_output in args.archive_output:
        archive_path = archive_output.resolve()
        with timed_step(f"archive {archive_path.name}", timings):
            write_archive(
                package_dir,
                archive_path,
                force=args.force,
                entries=archive_entries,
                compression=getattr(args, "archive_compression", "default"),
            )
        print(f"Built Codex package archive at {archive_path}")

    print(f"Built Codex package directory at {package_dir}")
    return 0


def resolve_package_inputs(
    args: argparse.Namespace,
    spec,
    variant,
) -> tuple[str, PackageInputs]:
    zsh_bin = None
    zsh_bin_arg = getattr(args, "zsh_bin", None)
    resolve_zsh = supports_zsh(spec)
    with ThreadPoolExecutor(max_workers=4) as executor:
        source_outputs_future = executor.submit(
            resolve_source_outputs, args, spec, variant
        )
        version_future = executor.submit(read_workspace_version)
        rg_future = executor.submit(resolve_rg_bin, spec, args.rg_bin)
        zsh_future = (
            executor.submit(resolve_zsh_bin, spec)
            if resolve_zsh and zsh_bin_arg is None
            else None
        )
        if resolve_zsh and zsh_bin_arg is not None:
            zsh_bin = resolve_zsh_bin(spec, zsh_bin_arg)
        source_outputs = source_outputs_future.result()
        version = version_future.result()
        rg_bin = rg_future.result()
        if zsh_future is not None:
            zsh_bin = zsh_future.result()
    return (
        version,
        PackageInputs(
            entrypoint_bin=source_outputs.entrypoint_bin,
            rg_bin=rg_bin,
            zsh_bin=zsh_bin,
            bwrap_bin=source_outputs.bwrap_bin,
            codex_command_runner_bin=source_outputs.codex_command_runner_bin,
            codex_windows_sandbox_setup_bin=source_outputs.codex_windows_sandbox_setup_bin,
        ),
    )


def resolve_source_outputs(
    args: argparse.Namespace,
    spec,
    variant,
) -> SourceBuildOutputs:
    if getattr(args, "skip_build_if_present", False):
        outputs = source_outputs_from_existing(spec, variant, args.cargo_profile)
        validate_source_outputs(outputs)
        return outputs

    return build_source_binaries(
        spec,
        variant,
        cargo=args.cargo,
        profile=args.cargo_profile,
        entrypoint_bin=resolve_optional_input_path(
            args.entrypoint_bin,
            "prebuilt entrypoint executable",
            "--entrypoint-bin",
        ),
        bwrap_bin=resolve_optional_input_path(
            args.bwrap_bin if spec.is_linux else None,
            "prebuilt Linux bwrap executable",
            "--bwrap-bin",
        ),
        codex_command_runner_bin=resolve_optional_input_path(
            args.codex_command_runner_bin if spec.is_windows else None,
            "prebuilt Windows codex-command-runner.exe executable",
            "--codex-command-runner-bin",
        ),
        codex_windows_sandbox_setup_bin=resolve_optional_input_path(
            args.codex_windows_sandbox_setup_bin if spec.is_windows else None,
            "prebuilt Windows codex-windows-sandbox-setup.exe executable",
            "--codex-windows-sandbox-setup-bin",
        ),
        reuse_existing=getattr(args, "reuse_source_builds", False),
        force_rebuild=getattr(args, "force_source_rebuild", False),
    )


def source_outputs_from_existing(spec, variant, profile: str) -> SourceBuildOutputs:
    # Look where this tool's own builds write (the package lane), not the
    # shared cargo target dir — otherwise --skip-build-if-present misses the
    # previous package build or, worse, picks up stale dev-lane binaries.
    output_dir = cargo_profile_output_dir(
        spec, profile, target_dir=cargo_package_target_dir(spec, profile)
    )
    return SourceBuildOutputs(
        entrypoint_bin=output_dir / variant.entrypoint_name(spec),
        bwrap_bin=output_dir / "bwrap" if spec.is_linux else None,
        codex_command_runner_bin=(
            output_dir / "codex-command-runner.exe" if spec.is_windows else None
        ),
        codex_windows_sandbox_setup_bin=(
            output_dir / "codex-windows-sandbox-setup.exe" if spec.is_windows else None
        ),
    )


@contextmanager
def timed_step(label: str, enabled: bool) -> Iterator[None]:
    started = perf_counter()
    try:
        yield
    finally:
        if enabled:
            elapsed = perf_counter() - started
            print(f"Timing {label}: {elapsed:.3f}s")


def resolve_optional_input_path(
    explicit_path: Path | None,
    description: str,
    flag_name: str,
) -> Path | None:
    if explicit_path is None:
        return None

    return resolve_input_path(explicit_path, description, flag_name)
