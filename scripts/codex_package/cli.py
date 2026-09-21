"""Command-line interface for building Codex package directories."""

import argparse
import json
import os
import re
import shutil
import tempfile
import uuid
from collections.abc import Iterator
from contextvars import ContextVar
from scripts.process_owner import OwnedThreadPoolExecutor as ThreadPoolExecutor
from contextlib import contextmanager, ExitStack
from pathlib import Path
from time import perf_counter

from .archive import activate_archive
from .archive import package_entries
from .archive import resolve_zstd_command
from .archive import validate_archive_output
from .archive import write_archive
from scripts.stage_npm_archives import exclusive_file_lock
from .cargo import package_build_lease
from .cargo import SourceBuildOutputs
from .cargo import build_source_binaries
from .cargo import cargo_package_target_dir
from .cargo import cargo_profile_output_dir
from .cargo import source_build_stamp_matches
from .cargo import source_tree_fingerprint
from .cargo import validate_source_outputs
from .layout import build_package_dir
from .layout import prepare_package_dir
from .layout import remove_tree_allow_readonly
from .layout import validate_package_dir_destination
from .layout import validate_package_dir
from .layout import validate_package_input_roles
from .layout import sha256_file
from .ripgrep import resolve_rg_bin
from .targets import PACKAGE_VARIANTS
from .targets import REPO_ROOT
from .targets import SUPPORTED_TARGETS
from .targets import SUPPORTED_VARIANTS
from .targets import TARGET_SPECS
from .targets import PackageInputs
from .targets import PackageVariant
from .targets import TargetSpec
from .targets import default_target
from .targets import resolve_input_path
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
            ".tar.gz, .tgz, .tar.zst, .zip. Archives require validation."
        ),
    )
    parser.add_argument(
        "--release-dir",
        type=Path,
        help=(
            "Emit the installer-owned codex-package-<target>.tar.gz asset plus "
            "checksum and provenance manifests into this directory."
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
        default="release",
        help="Cargo profile for source-built package artifacts.",
    )
    parser.add_argument(
        "--release-version",
        help=(
            "Authoritative semantic version embedded into source-built release "
            "binaries and package metadata. Required for distributable output."
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
        "--code-mode-host-bin",
        type=Path,
        help=(
            "Optional prebuilt codex-code-mode-host executable. If omitted, "
            "the host is built with Cargo."
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
        "--reuse-source-builds",
        action="store_true",
        help=(
            "Reuse Cargo package binaries when source, recipe, and output "
            "fingerprints match; build missing or stale outputs."
        ),
    )
    parser.add_argument(
        "--skip-build-if-present",
        action="store_true",
        help=(
            "Skip Cargo only when source, recipe, and output fingerprints match; "
            "otherwise fail."
        ),
    )
    parser.add_argument(
        "--force-source-rebuild",
        action="store_true",
        help="Invoke Cargo even when a reusable package-build stamp exists.",
    )
    parser.add_argument(
        "--skip-validate",
        action="store_true",
        help="Skip package layout validation after copying files.",
    )
    parser.add_argument(
        "--reuse-package-dir",
        action="store_true",
        help="Allow replacing an existing package directory. Existing contents are discarded.",
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
    release_dir = getattr(args, "release_dir", None)
    if release_dir is not None:
        if variant.name != "codex":
            raise RuntimeError("--release-dir supports only the codex package variant")
        release_archive = release_dir.resolve() / f"codex-package-{spec.target}.tar.gz"
        args.archive_output = [*args.archive_output, release_archive]
    package_dir_arg = getattr(args, "package_dir", None)
    package_dir = (
        package_dir_arg.resolve()
        if package_dir_arg is not None
        else Path(tempfile.mkdtemp(prefix="codex-package-")).resolve()
    )
    validate_cli_request(args, spec, package_dir)

    output_paths = [package_dir, *(path.resolve() for path in args.archive_output)]
    if release_dir is not None:
        output_paths += [
            release_dir.resolve() / f"codex-package_{spec.target}_PROVENANCE.json",
            release_dir.resolve() / "codex-package_SHA256SUMS",
        ]
    with (
        publication_transaction(output_paths),
        package_build_lease(spec, args.cargo_profile),
    ):
        timings = getattr(args, "timings", False)
        with timed_step("inputs", timings):
            version, inputs = resolve_package_inputs(args, spec, variant)
            validate_package_input_roles(inputs)
        reuse_package_dir = getattr(args, "reuse_package_dir", False)
        with staged_package_destination(
            package_dir, reuse_existing=reuse_package_dir, force=args.force
        ) as staged_package_dir:
            with timed_step("package-dir", timings):
                prepare_package_dir(
                    staged_package_dir,
                    force=True,
                    reuse=reuse_package_dir,
                )
                build_identity = {
                    "packagingSource": source_tree_fingerprint(),
                }
                build_package_dir(
                    staged_package_dir,
                    version,
                    variant,
                    spec,
                    inputs,
                    build_identity=build_identity,
                )
            if not getattr(args, "skip_validate", False):
                with timed_step("validate", timings):
                    validate_package_dir(
                        staged_package_dir,
                        variant,
                        spec,
                        expected_version=version,
                    )

            archive_entries = None
            if args.archive_output:
                with timed_step("archive-entries", timings):
                    archive_entries = package_entries(staged_package_dir)
            archive_paths = [output.resolve() for output in args.archive_output]
            if archive_paths:
                with timed_step("archives", timings):
                    write_archives_atomically(
                        staged_package_dir,
                        archive_paths,
                        force=args.force,
                        entries=archive_entries,
                        compression=getattr(args, "archive_compression", "default"),
                    )
            for archive_path in archive_paths:
                print(f"Built Codex package archive at {archive_path}")
            if release_dir is not None:
                write_release_manifests(
                    release_dir.resolve(),
                    staged_package_dir,
                    [
                        path
                        for path in archive_paths
                        if path.parent == release_dir.resolve()
                    ],
                )

        print(f"Built Codex package directory at {package_dir}")
        return 0


def pe_machine_for_target(spec):
    return 0xAA64 if spec.target.startswith("aarch64") else 0x8664



_publication = ContextVar("package_publication", default=None)


def _remove_output(path):
    if path.is_dir() and not path.is_symlink():
        remove_tree_allow_readonly(path)
    else:
        path.unlink(missing_ok=True)


class Publication:
    def __init__(self, paths):
        self.paths = set(paths)
        self.backups = {}

    def activate(self, source, destination, *, force):
        destination = destination.resolve()
        if destination not in self.paths or destination in self.backups:
            raise RuntimeError(f"unexpected publication destination: {destination}")
        backup = None
        if destination.exists():
            if not force and not (
                destination.is_dir() and not any(destination.iterdir())
            ):
                raise RuntimeError(f"output already exists: {destination}")
            backup = destination.with_name(
                f".{destination.name}.backup-{uuid.uuid4().hex}"
            )
        # Register before mutation, including an interrupt between rename and activation.
        self.backups[destination] = backup
        if backup is not None:
            destination.replace(backup)
        if source.is_dir():
            source.rename(destination)
        else:
            activate_archive(source, destination, force=force)

    def rollback(self):
        for destination, backup in reversed(list(self.backups.items())):
            if backup is None:
                _remove_output(destination)
            elif backup.exists():
                _remove_output(destination)
                backup.replace(destination)
            # A missing backup means the original rename never completed.

    def discard_backups(self):
        for backup in self.backups.values():
            if backup is not None and backup.exists():
                try:
                    _remove_output(backup)
                except OSError as error:
                    print(
                        f"warning: committed output backup retained at {backup}: {error}"
                    )


@contextmanager
def publication_transaction(paths):
    paths = sorted(set(path.resolve() for path in paths), key=str)
    current = _publication.get()
    if current is not None:
        if not set(paths).issubset(current.paths):
            raise RuntimeError("nested publication must use the owned output set")
        yield current
        return
    with ExitStack() as stack:
        for path in paths:
            stack.enter_context(
                exclusive_file_lock(path.with_name("." + path.name + ".publish.lock"))
            )
        current = Publication(paths)
        token = _publication.set(current)
        try:
            yield current
        except BaseException:
            current.rollback()
            raise
        else:
            current.discard_backups()
        finally:
            _publication.reset(token)


@contextmanager
def staged_package_destination(
    package_dir: Path, *, reuse_existing: bool, force: bool = False
) -> Iterator[Path]:
    """Build beside the destination and activate only after successful validation."""
    package_dir.parent.mkdir(parents=True, exist_ok=True)
    staging_root = Path(
        tempfile.mkdtemp(prefix=f".{package_dir.name}.staging-", dir=package_dir.parent)
    )
    staged_dir = staging_root / package_dir.name
    backup_dir: Path | None = None
    committed = False
    try:
        yield staged_dir

        validate_package_dir_destination(package_dir, force=force, reuse=reuse_existing)
        if _publication.get() is not None:
            _publication.get().activate(
                staged_dir, package_dir, force=force or reuse_existing
            )
            committed = True
            return
        if not (force or reuse_existing):
            # rmdir fails if another writer populated the previously empty directory.
            if package_dir.exists():
                package_dir.rmdir()
            staged_dir.rename(package_dir)
            committed = True
            return
        if package_dir.exists():
            backup_dir = package_dir.with_name(
                f".{package_dir.name}.backup-{uuid.uuid4().hex}"
            )
            package_dir.replace(backup_dir)
        try:
            staged_dir.replace(package_dir)
        except BaseException:
            if backup_dir is not None and backup_dir.exists():
                backup_dir.replace(package_dir)
            raise
        committed = True
        if backup_dir is not None and backup_dir.exists():
            try:
                remove_tree_allow_readonly(backup_dir)
            except OSError as exc:
                print(
                    f"warning: package was committed but backup cleanup failed: "
                    f"{backup_dir}: {exc}"
                )
    finally:
        if staging_root.exists():
            try:
                remove_tree_allow_readonly(staging_root)
            except OSError as exc:
                if not committed:
                    raise
                print(f"warning: staging cleanup failed: {staging_root}: {exc}")


def write_archives_atomically(
    package_dir: Path,
    archive_paths: list[Path],
    *,
    force: bool,
    entries: list[Path] | None,
    compression: str,
) -> None:
    """Generate every archive before replacing any requested destination."""
    encoded = {}
    staged: list[tuple[Path, Path, Path]] = []
    backups: list[tuple[Path, Path]] = []
    activated: list[tuple[Path, Path]] = []
    try:
        for archive_path in archive_paths:
            archive_path.parent.mkdir(parents=True, exist_ok=True)
            staging_root = Path(
                tempfile.mkdtemp(
                    prefix=f".{archive_path.name}.staging-", dir=archive_path.parent
                )
            )
            staged_path = staging_root / archive_path.name
            staged.append((archive_path, staged_path, staging_root))
            _, _, kind = validate_archive_output(
                package_dir, staged_path, force=True, compression=compression
            )
            key = (kind, compression)
            if key in encoded:
                shutil.copyfile(encoded[key], staged_path)
            else:
                write_archive(
                    package_dir,
                    staged_path,
                    force=True,
                    entries=entries,
                    compression=compression,
                )
                encoded[key] = staged_path

        if _publication.get() is not None:
            for archive_path, staged_path, _ in staged:
                _publication.get().activate(staged_path, archive_path, force=force)
            return
        for archive_path, staged_path, _ in staged:
            if archive_path.exists():
                if not force:
                    raise RuntimeError(f"Archive output already exists: {archive_path}")
                backup_path = archive_path.with_name(
                    f".{archive_path.name}.backup-{uuid.uuid4().hex}"
                )
                archive_path.replace(backup_path)
                backups.append((archive_path, backup_path))
            activate_archive(staged_path, archive_path, force=force)
            activated.append((archive_path, staged_path))
    except BaseException:
        for archive_path, staged_path in reversed(activated):
            if archive_path.exists():
                archive_path.replace(staged_path)
        for archive_path, backup_path in reversed(backups):
            if backup_path.exists():
                backup_path.replace(archive_path)
        raise
    else:
        for _, backup_path in backups:
            try:
                backup_path.unlink(missing_ok=True)
            except OSError as exc:
                print(
                    f"warning: archives were committed but backup cleanup failed: "
                    f"{backup_path}: {exc}"
                )
    finally:
        for _, _, staging_root in staged:
            if staging_root.exists():
                try:
                    remove_tree_allow_readonly(staging_root)
                except OSError as exc:
                    print(
                        f"warning: archive staging cleanup failed: {staging_root}: {exc}"
                    )


def resolve_package_inputs(
    args: argparse.Namespace,
    spec: TargetSpec,
    variant: PackageVariant,
) -> tuple[str, PackageInputs]:
    version = getattr(args, "release_version", None) or read_workspace_version()
    # Validate explicit local inputs before starting the expensive source build.
    rg_bin = resolve_rg_bin(spec, args.rg_bin) if args.rg_bin is not None else None
    if rg_bin is not None:
        from .layout import pe_machine

        machine = pe_machine(rg_bin)
        if machine is not None and machine != pe_machine_for_target(spec):
            raise RuntimeError(
                "ripgrep executable architecture does not match package target"
            )
    with ThreadPoolExecutor(max_workers=2) as executor:
        # Copy context includes the held build lease; the owner waits for every
        # worker to stop before releasing it or removing staging files.
        source_future = executor.submit(resolve_source_outputs, args, spec, variant)
        rg_future = (
            executor.submit(resolve_rg_bin, spec, None) if rg_bin is None else None
        )
        source_outputs = source_future.result()
        if rg_future is not None:
            rg_bin = rg_future.result()
    return (
        version,
        PackageInputs(
            entrypoint_bin=source_outputs.entrypoint_bin,
            code_mode_host_bin=source_outputs.code_mode_host_bin,
            rg_bin=rg_bin,
            codex_command_runner_bin=source_outputs.codex_command_runner_bin,
            codex_windows_sandbox_setup_bin=source_outputs.codex_windows_sandbox_setup_bin,
        ),
    )


def validate_cli_request(
    args: argparse.Namespace,
    spec: TargetSpec,
    package_dir: Path,
) -> None:
    command_runner_bin = getattr(args, "codex_command_runner_bin", None)
    sandbox_setup_bin = getattr(args, "codex_windows_sandbox_setup_bin", None)

    if getattr(args, "skip_build_if_present", False):
        ignored_source_flags = [
            flag
            for flag, value in [
                ("--entrypoint-bin", getattr(args, "entrypoint_bin", None)),
                (
                    "--code-mode-host-bin",
                    getattr(args, "code_mode_host_bin", None),
                ),
                ("--codex-command-runner-bin", command_runner_bin),
                ("--codex-windows-sandbox-setup-bin", sandbox_setup_bin),
            ]
            if value is not None
        ]
        if ignored_source_flags:
            raise RuntimeError(
                "--skip-build-if-present cannot be combined with source binary "
                f"overrides: {', '.join(ignored_source_flags)}"
            )
        if getattr(args, "reuse_source_builds", False):
            raise RuntimeError(
                "--skip-build-if-present cannot be combined with --reuse-source-builds."
            )
        if getattr(args, "force_source_rebuild", False):
            raise RuntimeError(
                "--skip-build-if-present cannot be combined with --force-source-rebuild."
            )

    force = bool(getattr(args, "force", False))
    reuse_package_dir = bool(getattr(args, "reuse_package_dir", False))
    validate_package_dir_destination(
        package_dir,
        force=force,
        reuse=reuse_package_dir,
    )

    compression = getattr(args, "archive_compression", "default")
    release_version = getattr(args, "release_version", None)
    if (
        release_version is not None
        and re.fullmatch(
            r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
            r"(?:-(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
            r"(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*)?"
            r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?",
            release_version,
        )
        is None
    ):
        raise RuntimeError("--release-version must be a semantic version")
    if getattr(args, "archive_output", []) and getattr(args, "skip_validate", False):
        raise RuntimeError("--skip-validate cannot be used with --archive-output")
    if getattr(args, "archive_output", []) and not release_version:
        raise RuntimeError("Distributable archives require --release-version")
    if (
        getattr(args, "archive_output", [])
        and spec.target != default_target()
        and getattr(args, "entrypoint_bin", None) is not None
    ):
        raise RuntimeError(
            "Cross-target prebuilt entrypoints have no verifiable release-version evidence; build the distributable from source"
        )
    if (
        getattr(args, "archive_output", [])
        and getattr(args, "cargo_profile", "release") != "release"
    ):
        raise RuntimeError("Distributable archives require --cargo-profile release")
    seen_outputs: set[Path] = set()
    needs_zstd = False
    for archive_output in getattr(args, "archive_output", []):
        _, resolved_output, archive_format = validate_archive_output(
            package_dir,
            archive_output,
            force=force,
            compression=compression,
        )
        if resolved_output in seen_outputs:
            raise RuntimeError(
                f"Archive output was specified more than once: {resolved_output}"
            )
        seen_outputs.add(resolved_output)
        needs_zstd |= archive_format == "tar.zst"
    if needs_zstd:
        resolve_zstd_command()
    for name in ("LICENSE", "NOTICE"):
        if not (REPO_ROOT / name).is_file():
            raise RuntimeError(f"Required package input is missing: {name}")


def resolve_source_outputs(
    args: argparse.Namespace,
    spec: TargetSpec,
    variant: PackageVariant,
) -> SourceBuildOutputs:
    if getattr(args, "skip_build_if_present", False):
        outputs = source_outputs_from_existing(spec, variant, args.cargo_profile)
        validate_source_outputs(outputs)
        target_dir = cargo_package_target_dir(spec, args.cargo_profile)
        if not source_build_stamp_matches(
            target_dir,
            spec=spec,
            profile=args.cargo_profile,
            variant=variant,
            outputs=outputs,
            cargo=args.cargo,
            release_version=getattr(args, "release_version", None),
        ):
            raise RuntimeError(
                "--skip-build-if-present found binaries, but their content or "
                "source-build recipe stamp is missing or stale. Rebuild without "
                "--skip-build-if-present."
            )
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
        code_mode_host_bin=resolve_optional_input_path(
            getattr(args, "code_mode_host_bin", None),
            "prebuilt code-mode host executable",
            "--code-mode-host-bin",
        ),
        codex_command_runner_bin=resolve_optional_input_path(
            args.codex_command_runner_bin,
            "prebuilt Windows codex-command-runner.exe executable",
            "--codex-command-runner-bin",
        ),
        codex_windows_sandbox_setup_bin=resolve_optional_input_path(
            args.codex_windows_sandbox_setup_bin,
            "prebuilt Windows codex-windows-sandbox-setup.exe executable",
            "--codex-windows-sandbox-setup-bin",
        ),
        reuse_existing=getattr(args, "reuse_source_builds", False),
        force_rebuild=getattr(args, "force_source_rebuild", False),
        release_version=getattr(args, "release_version", None),
    )


def write_release_manifests(release_dir, package_dir, archive_paths):
    metadata = json.loads(
        (package_dir / "codex-package.json").read_text(encoding="utf-8")
    )
    paths = [
        release_dir / "codex-package_SHA256SUMS",
        release_dir / f"codex-package_{metadata['target']}_PROVENANCE.json",
    ]
    with publication_transaction(paths):
        _write_release_manifests(release_dir, package_dir, archive_paths)


def _write_release_manifests(
    release_dir: Path, package_dir: Path, archive_paths: list[Path]
) -> None:
    release_dir.mkdir(parents=True, exist_ok=True)
    artifacts = [
        {
            "name": path.name,
            "size": path.stat().st_size,
            "sha256": sha256_file(path),
        }
        for path in sorted(archive_paths)
    ]
    if not artifacts:
        raise RuntimeError("release output did not produce an installer archive")
    metadata = json.loads(
        (package_dir / "codex-package.json").read_text(encoding="utf-8")
    )
    provenance = {
        "schemaVersion": 1,
        "sourceRepository": "https://github.com/ikhdark/KD4",
        "version": metadata["version"],
        "target": metadata["target"],
        "bundleId": metadata["bundleId"],
        "buildIdentity": metadata["buildIdentity"],
        "artifacts": artifacts,
    }
    write_text_atomically(
        release_dir / f"codex-package_{metadata['target']}_PROVENANCE.json",
        json.dumps(provenance, sort_keys=True, indent=2) + "\n",
    )
    checksum_path = release_dir / "codex-package_SHA256SUMS"
    checksum_entries: dict[str, str] = {}
    if checksum_path.is_file():
        for line in checksum_path.read_text(encoding="utf-8").splitlines():
            parts = line.split(None, 1)
            if len(parts) == 2 and len(parts[0]) == 64:
                checksum_entries[parts[1].strip()] = parts[0].lower()
    checksum_entries.update({item["name"]: item["sha256"] for item in artifacts})
    write_text_atomically(
        checksum_path,
        "".join(
            f"{digest}  {name}\n" for name, digest in sorted(checksum_entries.items())
        ),
    )


def write_text_atomically(path: Path, contents: str) -> None:
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
    try:
        temporary.write_text(contents, encoding="utf-8", newline="\n")
        if _publication.get() is not None:
            _publication.get().activate(temporary, path, force=True)
        else:
            os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def source_outputs_from_existing(
    spec: TargetSpec,
    variant: PackageVariant,
    profile: str,
) -> SourceBuildOutputs:
    # Look where this tool's own builds write (the package lane), not the
    # shared cargo target dir — otherwise --skip-build-if-present misses the
    # previous package build or, worse, picks up stale dev-lane binaries.
    output_dir = cargo_profile_output_dir(
        spec, profile, target_dir=cargo_package_target_dir(spec, profile)
    )
    return SourceBuildOutputs(
        entrypoint_bin=output_dir / variant.entrypoint_name(spec),
        code_mode_host_bin=output_dir / spec.code_mode_host_name,
        codex_command_runner_bin=output_dir / "codex-command-runner.exe",
        codex_windows_sandbox_setup_bin=output_dir / "codex-windows-sandbox-setup.exe",
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
