#!/usr/bin/env python3
"""Stage one or more Codex npm packages for release."""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import contextmanager
from dataclasses import dataclass
from functools import cache
import hashlib
import importlib.util
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import uuid
from pathlib import Path, PurePosixPath
from typing import Sequence
from urllib.parse import urlparse
import zipfile


REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.codex_package.targets import BINARY_TARGETS  # noqa: E402

from scripts.stage_npm_archives import (  # noqa: E402
    archive_name_for_target,
    artifact_dir_for_target,
    binary_archive_path,
    cached_codex_package_archive,
    download_worker_count_for,
    exclusive_file_lock,
    extract_tar_data,
    extract_zstd_archive,
    hardlink_tree,
    install_binary_components,
    install_codex_package_archives,
    install_single_binary,
    install_single_codex_package_archive,
    is_relative_to,
    lock_owner_pid,
    materialize_cached_tree,
    process_is_running,
    validate_tar_members_for_legacy_python,
    worker_count_for,
)

__all__ = [
    "archive_name_for_target",
    "artifact_dir_for_target",
    "binary_archive_path",
    "cached_codex_package_archive",
    "download_worker_count_for",
    "exclusive_file_lock",
    "extract_tar_data",
    "extract_zstd_archive",
    "hardlink_tree",
    "install_binary_components",
    "install_codex_package_archives",
    "install_single_binary",
    "install_single_codex_package_archive",
    "is_relative_to",
    "lock_owner_pid",
    "materialize_cached_tree",
    "process_is_running",
    "validate_tar_members_for_legacy_python",
    "worker_count_for",
    "tarfile",
    "time",
]


BUILD_SCRIPT = REPO_ROOT / "codex-cli" / "scripts" / "build_npm_package.py"
DEFAULT_GITHUB_REPO = "ikhdark/KD4"
COMPLETE_MARKER = ".complete"
LOCK_POLL_SECONDS = 0.1
LOCK_STALE_SECONDS = 60 * 60
DEFAULT_GHA_DOWNLOAD_WORKERS = 8
VENDOR_COPY_MODES = ("auto", "copy", "hardlink")
MAX_CAPTURED_LOG_CHARS = 20_000


@dataclass(frozen=True, slots=True)
class BinaryComponent:
    artifact_prefix: str
    dest_dir: str
    binary_basename: str


@dataclass(frozen=True, slots=True)
class WorkflowArtifact:
    name: str
    size_in_bytes: int
    artifact_id: int
    archive_sha256: str | None


@dataclass(frozen=True, slots=True)
class WorkflowRunReference:
    github_repo: str
    run_id: str


@dataclass(frozen=True, slots=True)
class StagePackageResult:
    package: str
    pack_output: Path
    log: str


BINARY_COMPONENTS = {
    "codex-responses-api-proxy": BinaryComponent(
        artifact_prefix="codex-responses-api-proxy",
        dest_dir="codex-responses-api-proxy",
        binary_basename="codex-responses-api-proxy",
    ),
}


def _gha_enabled() -> bool:
    return os.environ.get("GITHUB_ACTIONS") == "true"


def _gha_escape(value: str) -> str:
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


@contextmanager
def _gha_group(title: str):
    if _gha_enabled():
        print(f"::group::{_gha_escape(title)}", flush=True)
    try:
        yield
    finally:
        if _gha_enabled():
            print("::endgroup::", flush=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--release-version",
        required=True,
        help="Version to stage (e.g. 0.1.0 or 0.1.0-alpha.1).",
    )
    parser.add_argument(
        "--package",
        dest="packages",
        action="append",
        required=True,
        help="Package name to stage. May be provided multiple times.",
    )
    parser.add_argument(
        "--workflow-url",
        help="Optional workflow URL to reuse for native artifacts.",
    )
    parser.add_argument(
        "--vendor-src",
        type=Path,
        help=(
            "Local canonical package tree containing one directory per target. "
            "Use this for repository-local releases without a GitHub workflow."
        ),
    )
    parser.add_argument(
        "--github-repo",
        default=os.environ.get("CODEX_STAGE_GITHUB_REPO"),
        help=(
            "GitHub repository to query for release artifacts, in owner/name form. "
            "Defaults to CODEX_STAGE_GITHUB_REPO, then the current gh repo, then ikhdark/KD4."
        ),
    )
    parser.add_argument(
        "--workflow-name",
        default=os.environ.get("CODEX_STAGE_WORKFLOW_NAME"),
        help=(
            "Explicit GitHub Actions workflow name/path to query when "
            "--workflow-url is not provided. There is no implicit workflow default."
        ),
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="Directory where npm tarballs should be written (default: dist/npm).",
    )
    parser.add_argument(
        "--keep-staging-dirs",
        action="store_true",
        help="Retain temporary staging directories instead of deleting them.",
    )
    parser.add_argument(
        "--max-download-workers",
        type=positive_int,
        default=None,
        help=(
            "Maximum parallel GitHub artifact downloads "
            f"(default: {DEFAULT_GHA_DOWNLOAD_WORKERS} on GitHub Actions, "
            "otherwise CPU count)."
        ),
    )
    parser.add_argument(
        "--max-stage-workers",
        type=positive_int,
        default=1,
        help="Maximum packages to stage in parallel (default: 1).",
    )
    parser.add_argument(
        "--cache-dir",
        type=Path,
        default=None,
        help=(
            "Persistent native artifact cache directory. By default a per-run "
            "temporary cache is used and deleted."
        ),
    )
    parser.add_argument(
        "--vendor-copy-mode",
        choices=VENDOR_COPY_MODES,
        default="auto",
        help=(
            "How cached vendor trees are materialized: auto uses hardlinks with "
            "copy fallback (default), copy always copies, hardlink requires links."
        ),
    )
    return parser.parse_args()


def resolve_github_repo(override: str | None) -> str:
    if override:
        return override
    try:
        repo = subprocess.check_output(
            ["gh", "repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"],
            cwd=REPO_ROOT,
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        repo = ""
    return repo or DEFAULT_GITHUB_REPO


def github_repo_cache_key(github_repo: str) -> str:
    return github_repo.replace("/", "__")


def parse_workflow_run_url(workflow_url: str) -> WorkflowRunReference:
    parsed = urlparse(workflow_url)
    parts = [part for part in parsed.path.split("/") if part]
    if (
        parsed.scheme.casefold() != "https"
        or parsed.netloc.casefold() != "github.com"
        or len(parts) != 5
        or parts[2] != "actions"
        or parts[3] != "runs"
        or not parts[4].isdigit()
    ):
        raise ValueError(
            "workflow URL must match "
            "https://github.com/<owner>/<repo>/actions/runs/<run-id>"
        )
    return WorkflowRunReference(
        github_repo=f"{parts[0]}/{parts[1]}",
        run_id=parts[4],
    )


def github_repo_from_workflow_url(workflow_url: str) -> str | None:
    try:
        return parse_workflow_run_url(workflow_url).github_repo
    except ValueError:
        return None


@cache
def load_build_module():
    spec = importlib.util.spec_from_file_location(
        "codex_build_npm_package", BUILD_SCRIPT
    )
    if spec is None or spec.loader is None:
        raise RuntimeError(f"Unable to load module from {BUILD_SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def package_native_components() -> dict[str, set[str]]:
    return getattr(load_build_module(), "PACKAGE_NATIVE_COMPONENTS", {})


def package_expansions() -> dict[str, list[str]]:
    return getattr(load_build_module(), "PACKAGE_EXPANSIONS", {})


def codex_platform_packages() -> dict[str, dict[str, str]]:
    return getattr(load_build_module(), "CODEX_PLATFORM_PACKAGES", {})


def package_target_filters() -> dict[str, set[str]]:
    raw_filters = getattr(load_build_module(), "PACKAGE_TARGET_FILTERS", {})
    return {
        package: {targets} if isinstance(targets, str) else set(targets)
        for package, targets in raw_filters.items()
    }


def codex_package_component() -> str:
    return getattr(load_build_module(), "CODEX_PACKAGE_COMPONENT", "codex-package")


def native_components_for_package(package: str) -> tuple[str, ...]:
    return tuple(sorted(package_native_components().get(package, [])))


def native_targets_for_package(package: str) -> tuple[str, ...]:
    target_filter = package_target_filters().get(package)
    if target_filter is None:
        return tuple(BINARY_TARGETS)
    return tuple(target for target in BINARY_TARGETS if target in target_filter)


def native_component_key_for_package(
    package: str,
) -> tuple[tuple[str, ...], tuple[str, ...]]:
    return native_components_for_package(package), native_targets_for_package(package)


def collect_native_component_sets(
    packages: list[str],
) -> list[tuple[tuple[str, ...], tuple[str, ...]]]:
    component_sets: list[tuple[tuple[str, ...], tuple[str, ...]]] = []
    seen: set[tuple[tuple[str, ...], tuple[str, ...]]] = set()
    for package in packages:
        key = native_component_key_for_package(package)
        components, _targets = key
        if not components or key in seen:
            continue
        seen.add(key)
        component_sets.append(key)
    return component_sets


def expand_packages(packages: list[str]) -> list[str]:
    expanded: list[str] = []
    for package in packages:
        for expanded_package in package_expansions().get(package, [package]):
            if expanded_package in expanded:
                continue
            expanded.append(expanded_package)
    return expanded


def resolve_release_workflow(
    version: str, github_repo: str, workflow_name: str
) -> dict:
    stdout = subprocess.check_output(
        [
            "gh",
            "run",
            "list",
            "--repo",
            github_repo,
            "--branch",
            f"rust-v{version}",
            "--json",
            "workflowName,url,headSha,status,conclusion",
            "--workflow",
            workflow_name,
            "--jq",
            "first(.[])",
        ],
        cwd=REPO_ROOT,
        text=True,
    )
    workflow = json.loads(stdout or "null")
    if not workflow:
        raise RuntimeError(
            f"Unable to find rust-release workflow for version {version}."
        )
    return workflow


def resolve_workflow_url(
    version: str, override: str | None, github_repo: str, workflow_name: str | None
) -> tuple[str, str | None]:
    if override:
        reference = parse_workflow_run_url(override)
        stdout = subprocess.check_output(
            [
                "gh",
                "run",
                "view",
                reference.run_id,
                "--repo",
                reference.github_repo,
                "--json",
                "url,headSha,status,conclusion",
            ],
            cwd=REPO_ROOT,
            text=True,
        )
        workflow = json.loads(stdout)
        workflow["url"] = override
    elif workflow_name:
        workflow = resolve_release_workflow(version, github_repo, workflow_name)
    else:
        raise ValueError(
            "--workflow-url is required unless an explicit --workflow-name or "
            "CODEX_STAGE_WORKFLOW_NAME is provided"
        )

    if workflow.get("status") != "completed" or workflow.get("conclusion") != "success":
        raise RuntimeError(
            "native artifact workflow must be completed successfully; "
            f"status={workflow.get('status')!r}, conclusion={workflow.get('conclusion')!r}"
        )
    return workflow["url"], workflow.get("headSha")


def workflow_id_from_url(workflow_url: str) -> str:
    return parse_workflow_run_url(workflow_url).run_id


def ensure_source_matches_workflow(
    expected_head_sha: str,
    *,
    repo_root: Path = REPO_ROOT,
    allow_mismatch: bool = False,
    owned_paths: Sequence[Path] = (),
) -> None:
    current_head_sha = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repo_root, text=True
    ).strip()
    dirty_output = subprocess.check_output(
        ["git", "status", "--porcelain=v1", "-z", "--untracked-files=all"],
        cwd=repo_root,
    )
    dirty_records = parse_porcelain_v1_z(dirty_output)
    owned_roots = [path.resolve() for path in owned_paths]
    repo_root = repo_root.resolve()
    if any(path == repo_root for path in owned_roots):
        raise ValueError(
            "script-owned source-validation path cannot be the repository root"
        )
    dirty_records = [
        paths
        for paths in dirty_records
        if not paths
        or not all(
            any(
                is_relative_to((repo_root / path).resolve(strict=False), owned_root)
                for owned_root in owned_roots
            )
            for path in paths
        )
    ]
    problems = []
    if current_head_sha.casefold() != expected_head_sha.casefold():
        problems.append(
            f"checkout HEAD {current_head_sha} != workflow HEAD {expected_head_sha}"
        )
    if dirty_records:
        problems.append("checkout has uncommitted changes")
    if not problems:
        return
    message = "; ".join(problems)
    if allow_mismatch:
        print(f"WARNING: allowing source/native mismatch: {message}", file=sys.stderr)
        return
    raise RuntimeError(f"refusing source/native mismatch: {message}")


def parse_porcelain_v1_z(output: bytes) -> list[tuple[Path, ...]]:
    fields = output.split(b"\0")
    records: list[tuple[Path, ...]] = []
    index = 0
    while index < len(fields):
        entry = fields[index]
        index += 1
        if not entry:
            continue
        if len(entry) < 4 or entry[2:3] != b" ":
            raise RuntimeError("unexpected git status --porcelain=v1 -z record")
        status = entry[:2]
        paths = [Path(os.fsdecode(entry[3:]))]
        if b"R" in status or b"C" in status:
            if index >= len(fields) or not fields[index]:
                raise RuntimeError("incomplete rename/copy record from git status")
            paths.append(Path(os.fsdecode(fields[index])))
            index += 1
        records.append(tuple(paths))
    return records


@cache
def list_workflow_artifacts(
    workflow_id: str, github_repo: str
) -> tuple[WorkflowArtifact, ...]:
    stdout = subprocess.check_output(
        [
            "gh",
            "api",
            f"repos/{github_repo}/actions/runs/{workflow_id}/artifacts",
            "--paginate",
            "--jq",
            '.artifacts[] | [.id, .name, .size_in_bytes, (.digest // "")] | @tsv',
        ],
        cwd=REPO_ROOT,
        text=True,
    )
    artifacts = []
    for line in stdout.splitlines():
        if not line.strip():
            continue
        artifact_id, name, size_in_bytes, digest = line.split("\t", 3)
        archive_sha256 = None
        if digest:
            match = re.fullmatch(r"sha256:([0-9a-fA-F]{64})", digest)
            if match is None:
                raise RuntimeError(
                    f"workflow artifact {name!r} has unsupported digest {digest!r}"
                )
            archive_sha256 = match.group(1).lower()
        artifacts.append(
            WorkflowArtifact(name, int(size_in_bytes), int(artifact_id), archive_sha256)
        )
    return tuple(artifacts)


def install_native_components(
    workflow_url: str,
    github_repo: str,
    components: set[str],
    targets: Sequence[str],
    vendor_root: Path,
    artifacts_dir: Path,
    *,
    extracted_cache_dir: Path | None = None,
    max_download_workers: int | None = None,
    vendor_copy_mode: str = "auto",
) -> None:
    if not components:
        return

    vendor_dir = vendor_root / "vendor"
    vendor_dir.mkdir(parents=True, exist_ok=True)

    workflow_id = workflow_id_from_url(workflow_url)
    print(f"Downloading native artifacts from workflow {workflow_id}...", flush=True)
    with _gha_group(f"Download native artifacts from workflow {workflow_id}"):
        artifacts_dir.mkdir(parents=True, exist_ok=True)
        install_from_workflow_artifacts(
            workflow_id,
            github_repo,
            artifacts_dir,
            sorted(components),
            targets,
            vendor_dir,
            extracted_cache_dir=extracted_cache_dir,
            max_download_workers=max_download_workers,
            vendor_copy_mode=vendor_copy_mode,
        )
    print(f"Installed native dependencies into {vendor_dir}", flush=True)


def install_from_workflow_artifacts(
    workflow_id: str,
    github_repo: str,
    artifacts_dir: Path,
    components: Sequence[str],
    targets: Sequence[str],
    vendor_dir: Path,
    *,
    extracted_cache_dir: Path | None = None,
    max_download_workers: int | None = None,
    vendor_copy_mode: str = "auto",
) -> None:
    unknown_components = sorted(
        set(components) - {codex_package_component()} - set(BINARY_COMPONENTS)
    )
    if unknown_components:
        raise ValueError(
            "unregistered native component(s): " + ", ".join(unknown_components)
        )
    artifacts = select_target_artifacts(workflow_id, github_repo, components, targets)
    download_artifacts(
        workflow_id, github_repo, artifacts_dir, artifacts, max_download_workers
    )
    if codex_package_component() in components:
        install_codex_package_archives(
            artifacts_dir,
            vendor_dir,
            targets,
            extracted_cache_dir,
            vendor_copy_mode=vendor_copy_mode,
        )
    install_binary_components(
        artifacts_dir,
        vendor_dir,
        [BINARY_COMPONENTS[name] for name in components if name in BINARY_COMPONENTS],
        targets,
    )


def select_target_artifacts(
    workflow_id: str,
    github_repo: str,
    components: Sequence[str],
    targets: Sequence[str] = BINARY_TARGETS,
) -> list[WorkflowArtifact]:
    needs_target_artifacts = codex_package_component() in components or any(
        component in BINARY_COMPONENTS for component in components
    )
    if not needs_target_artifacts:
        return []

    artifacts_by_name: dict[str, WorkflowArtifact] = {}
    for artifact in list_workflow_artifacts(workflow_id, github_repo):
        if artifact.name in artifacts_by_name:
            raise RuntimeError(f"duplicate workflow artifact name: {artifact.name}")
        artifacts_by_name[artifact.name] = artifact
    selected_artifacts: list[WorkflowArtifact] = []
    for target in targets:
        for artifact_name in [target, f"{target}-unsigned"]:
            artifact = artifacts_by_name.get(artifact_name)
            if artifact is not None:
                if artifact.archive_sha256 is None:
                    raise RuntimeError(
                        f"workflow artifact {artifact.name!r} has no authoritative sha256 digest"
                    )
                selected_artifacts.append(artifact)
                break
        else:
            raise FileNotFoundError(
                f"Expected workflow artifact not found for target {target}"
            )

    return selected_artifacts


def download_artifacts(
    workflow_id: str,
    github_repo: str,
    dest_dir: Path,
    artifacts: Sequence[WorkflowArtifact],
    max_workers: int | None = None,
) -> None:
    total_bytes = sum(artifact.size_in_bytes for artifact in artifacts)
    print(
        f"Downloading {len(artifacts)} artifacts ({format_bytes(total_bytes)})",
        flush=True,
    )
    if not artifacts:
        return

    worker_count = download_worker_count_for(len(artifacts), max_workers)
    with ThreadPoolExecutor(max_workers=worker_count) as executor:
        futures = [
            executor.submit(
                download_single_artifact, workflow_id, github_repo, dest_dir, artifact
            )
            for artifact in artifacts
        ]
        for future in as_completed(futures):
            future.result()


def download_single_artifact(
    _workflow_id: str, github_repo: str, dest_dir: Path, artifact: WorkflowArtifact
) -> None:
    if artifact.archive_sha256 is None:
        raise RuntimeError(
            f"workflow artifact {artifact.name!r} has no authoritative sha256 digest"
        )
    dest_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = dest_dir / artifact.name
    if artifact_is_complete(artifact_dir, artifact):
        print(
            f"  using cached {artifact.name} ({format_bytes(artifact.size_in_bytes)})",
            flush=True,
        )
        return

    lock_path = dest_dir / f".{artifact.name}.lock"
    with exclusive_file_lock(lock_path):
        if artifact_is_complete(artifact_dir, artifact):
            print(
                f"  using cached {artifact.name} ({format_bytes(artifact.size_in_bytes)})",
                flush=True,
            )
            return

        temp_dir = (
            dest_dir / f".{artifact.name}.tmp-{os.getpid()}-{threading.get_ident()}"
        )
        shutil.rmtree(temp_dir, ignore_errors=True)
        temp_dir.mkdir(parents=True, exist_ok=True)
        try:
            print(
                f"  downloading {artifact.name} ({format_bytes(artifact.size_in_bytes)})",
                flush=True,
            )
            archive_path = dest_dir / (
                f".{artifact.name}.{artifact.artifact_id}."
                f"{os.getpid()}-{threading.get_ident()}.zip"
            )
            try:
                with archive_path.open("wb") as archive_handle:
                    subprocess.run(
                        [
                            "gh",
                            "api",
                            f"repos/{github_repo}/actions/artifacts/"
                            f"{artifact.artifact_id}/zip",
                        ],
                        cwd=REPO_ROOT,
                        stdout=archive_handle,
                        check=True,
                    )
                    archive_handle.flush()
                    os.fsync(archive_handle.fileno())
                if archive_path.stat().st_size != artifact.size_in_bytes:
                    raise RuntimeError(
                        f"workflow artifact {artifact.name!r} size mismatch: "
                        f"expected {artifact.size_in_bytes}, got {archive_path.stat().st_size}"
                    )
                actual_archive_sha256 = file_sha256(archive_path)
                if actual_archive_sha256 != artifact.archive_sha256:
                    raise RuntimeError(
                        f"workflow artifact {artifact.name!r} sha256 mismatch: "
                        f"expected {artifact.archive_sha256}, got {actual_archive_sha256}"
                    )
                extract_artifact_zip(archive_path, temp_dir)
            finally:
                archive_path.unlink(missing_ok=True)
            write_complete_marker(temp_dir, artifact)
            replace_directory(temp_dir, artifact_dir)
        except Exception:
            shutil.rmtree(temp_dir, ignore_errors=True)
            raise


def artifact_is_complete(artifact_dir: Path, artifact: WorkflowArtifact) -> bool:
    marker_path = artifact_dir / COMPLETE_MARKER
    if not marker_path.is_file():
        return False
    try:
        marker = marker_path.read_text(encoding="utf-8")
    except OSError:
        return False
    expected_digest = next(
        (
            line.removeprefix("tree_sha256=")
            for line in marker.splitlines()
            if line.startswith("tree_sha256=")
        ),
        None,
    )
    if expected_digest is None:
        return False
    try:
        actual_digest = artifact_tree_digest(artifact_dir)
    except (OSError, RuntimeError):
        return False
    return (
        "version=2\n" in marker
        and f"name={artifact.name}\n" in marker
        and f"size_in_bytes={artifact.size_in_bytes}\n" in marker
        and f"artifact_id={artifact.artifact_id}\n" in marker
        and f"archive_sha256={artifact.archive_sha256}\n" in marker
        and actual_digest == expected_digest
    )


def artifact_tree_digest(artifact_dir: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(
        artifact_dir.rglob("*"),
        key=lambda item: item.relative_to(artifact_dir).as_posix(),
    ):
        if path.name == COMPLETE_MARKER:
            continue
        relative = path.relative_to(artifact_dir).as_posix().encode("utf-8")
        if path.is_symlink():
            raise RuntimeError(f"artifact cache contains a symlink: {path}")
        kind = b"d" if path.is_dir() else b"f" if path.is_file() else None
        if kind is None:
            raise RuntimeError(f"artifact cache contains a special entry: {path}")
        digest.update(kind)
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        if kind == b"f":
            with path.open("rb") as handle:
                while chunk := handle.read(1024 * 1024):
                    digest.update(chunk)
    return digest.hexdigest()


def write_complete_marker(artifact_dir: Path, artifact: WorkflowArtifact) -> None:
    marker_path = artifact_dir / COMPLETE_MARKER
    payload = (
        "version=2\n"
        f"name={artifact.name}\n"
        f"size_in_bytes={artifact.size_in_bytes}\n"
        f"artifact_id={artifact.artifact_id}\n"
        f"archive_sha256={artifact.archive_sha256}\n"
        f"tree_sha256={artifact_tree_digest(artifact_dir)}\n"
    )
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{COMPLETE_MARKER}.", suffix=".tmp", dir=artifact_dir, text=True
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        temporary.replace(marker_path)
    finally:
        temporary.unlink(missing_ok=True)


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def extract_artifact_zip(archive_path: Path, dest_dir: Path) -> None:
    seen: set[str] = set()
    with zipfile.ZipFile(archive_path) as archive:
        for member in archive.infolist():
            normalized_name = member.filename.replace("\\", "/")
            candidate = PurePosixPath(normalized_name)
            if (
                not normalized_name
                or candidate.is_absolute()
                or any(part in {"", ".", ".."} for part in candidate.parts)
                or normalized_name in seen
            ):
                raise RuntimeError(f"unsafe workflow artifact path: {member.filename}")
            seen.add(normalized_name)
            mode = member.external_attr >> 16
            file_type = stat.S_IFMT(mode)
            if file_type not in {0, stat.S_IFREG, stat.S_IFDIR}:
                raise RuntimeError(
                    f"workflow artifact contains a link or special file: {member.filename}"
                )
            destination = dest_dir.joinpath(*candidate.parts)
            if member.is_dir():
                destination.mkdir(parents=True, exist_ok=True)
                continue
            destination.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(member) as source, destination.open("wb") as output:
                shutil.copyfileobj(source, output)


def replace_directory(source: Path, destination: Path) -> None:
    backup = destination.with_name(f".{destination.name}.{uuid.uuid4().hex}.old")
    had_destination = destination.exists()
    if had_destination:
        destination.rename(backup)
    activated = False
    try:
        source.rename(destination)
        activated = True
    except BaseException:
        if had_destination and backup.exists() and not destination.exists():
            backup.rename(destination)
        raise
    finally:
        if activated and backup.exists():
            shutil.rmtree(backup, ignore_errors=True)


def format_bytes(size_in_bytes: int) -> str:
    value = float(size_in_bytes)
    for unit in ["B", "KiB", "MiB"]:
        if value < 1024:
            return f"{value:.1f} {unit}"
        value /= 1024
    return f"{value:.1f} GiB"


def format_command(cmd: list[str]) -> str:
    return "+ " + " ".join(cmd)


def run_command(cmd: list[str]) -> None:
    print(format_command(cmd), flush=True)
    subprocess.run(cmd, cwd=REPO_ROOT, check=True)


def run_command_capture(cmd: list[str]) -> str:
    result = subprocess.run(
        cmd,
        cwd=REPO_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        # node/npm emit UTF-8 unconditionally; Windows would otherwise decode
        # as cp1252 and can hard-crash on undecodable bytes.
        encoding="utf-8",
        errors="replace",
    )
    log = format_command(cmd) + "\n" + bounded_log(result.stdout or "")
    if result.returncode != 0:
        raise RuntimeError(
            f"Command failed with exit code {result.returncode}:\n{log.rstrip()}"
        )
    return log


def bounded_log(text: str, *, max_chars: int = MAX_CAPTURED_LOG_CHARS) -> str:
    if len(text) <= max_chars:
        return text
    half = max_chars // 2
    omitted = len(text) - (half * 2)
    return text[:half] + f"\n...[truncated {omitted} chars]...\n" + text[-half:]


def tarball_name_for_package(package: str, version: str) -> str:
    if package in codex_platform_packages():
        platform = package.removeprefix("codex-")
        return f"codex-npm-{platform}-{version}.tgz"
    return f"{package}-npm-{version}.tgz"


def build_stage_command(
    package: str,
    release_version: str,
    output_dir: Path,
    staging_dir: Path,
    vendor_src_by_native_key: dict[tuple[tuple[str, ...], tuple[str, ...]], Path],
) -> tuple[Path, list[str]]:
    pack_output = output_dir / tarball_name_for_package(package, release_version)
    # Launch via the running interpreter: CreateProcess cannot exec a .py file
    # directly on Windows (WinError 193), so shebang-exec only works on POSIX.
    cmd = [
        sys.executable,
        str(BUILD_SCRIPT),
        "--package",
        package,
        "--release-version",
        release_version,
        "--staging-dir",
        str(staging_dir),
        "--pack-output",
        str(pack_output),
    ]

    vendor_src = vendor_src_by_native_key.get(native_component_key_for_package(package))
    if vendor_src is not None:
        cmd.extend(["--vendor-src", str(vendor_src)])

    return pack_output, cmd


def stage_package(
    package: str,
    release_version: str,
    output_dir: Path,
    runner_temp: Path,
    vendor_src_by_native_key: dict[tuple[tuple[str, ...], tuple[str, ...]], Path],
    keep_staging_dirs: bool,
    *,
    capture_output: bool,
) -> StagePackageResult:
    staging_dir = Path(
        tempfile.mkdtemp(prefix=f"npm-stage-{package}-", dir=runner_temp)
    )
    pack_output, cmd = build_stage_command(
        package,
        release_version,
        output_dir,
        staging_dir,
        vendor_src_by_native_key,
    )
    log = f"Staging {package} in {staging_dir}\n"

    try:
        if capture_output:
            log += run_command_capture(cmd)
        else:
            print(log, end="", flush=True)
            run_command(cmd)
            log = ""
    finally:
        if not keep_staging_dirs:
            shutil.rmtree(staging_dir, ignore_errors=True)

    return StagePackageResult(package=package, pack_output=pack_output, log=log)


def stage_packages(
    packages: Sequence[str],
    release_version: str,
    output_dir: Path,
    runner_temp: Path,
    vendor_src_by_native_key: dict[tuple[tuple[str, ...], tuple[str, ...]], Path],
    keep_staging_dirs: bool,
    max_stage_workers: int | None,
) -> list[StagePackageResult]:
    worker_count = worker_count_for(len(packages), max_stage_workers)
    if worker_count == 1:
        return [
            stage_package(
                package,
                release_version,
                output_dir,
                runner_temp,
                vendor_src_by_native_key,
                keep_staging_dirs,
                capture_output=False,
            )
            for package in packages
        ]

    print(
        f"Staging {len(packages)} packages with {worker_count} workers",
        flush=True,
    )
    results_by_package: dict[str, StagePackageResult] = {}
    with ThreadPoolExecutor(max_workers=worker_count) as executor:
        futures = {
            executor.submit(
                stage_package,
                package,
                release_version,
                output_dir,
                runner_temp,
                vendor_src_by_native_key,
                keep_staging_dirs,
                capture_output=True,
            ): package
            for package in packages
        }
        for future in as_completed(futures):
            result = future.result()
            if result.log:
                print(
                    result.log,
                    end="" if result.log.endswith("\n") else "\n",
                    flush=True,
                )
            results_by_package[result.package] = result

    return [results_by_package[package] for package in packages]


def commit_staged_packages(
    results: Sequence[StagePackageResult], output_dir: Path
) -> list[StagePackageResult]:
    output_dir.mkdir(parents=True, exist_ok=True)
    committed: list[tuple[Path, Path | None, Path]] = []
    try:
        for result in results:
            source = result.pack_output
            destination = output_dir / source.name
            backup = output_dir / f".{source.name}.{uuid.uuid4().hex}.old"
            existing_backup: Path | None = None
            if destination.exists():
                destination.replace(backup)
                existing_backup = backup
            try:
                source.replace(destination)
            except Exception:
                if existing_backup is not None and not destination.exists():
                    existing_backup.replace(destination)
                raise
            committed.append((destination, existing_backup, source))
    except Exception:
        for destination, backup, source in reversed(committed):
            if destination.exists():
                destination.replace(source)
            if backup is not None and backup.exists():
                backup.replace(destination)
        raise

    for _destination, backup, _source in committed:
        if backup is None or not backup.exists():
            continue
        try:
            backup.unlink()
        except OSError as error:
            print(
                f"warning: package activation succeeded, but backup cleanup failed "
                f"for {backup}: {error}",
                file=sys.stderr,
            )

    destinations = {
        destination.name: destination for destination, _backup, _source in committed
    }
    return [
        StagePackageResult(
            package=result.package,
            pack_output=destinations[result.pack_output.name],
            log=result.log,
        )
        for result in results
    ]


def main() -> int:
    args = parse_args()
    vendor_src_arg = getattr(args, "vendor_src", None)

    output_dir = args.output_dir or (REPO_ROOT / "dist" / "npm")

    runner_temp = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir())).resolve()
    if is_relative_to(runner_temp, REPO_ROOT.resolve()):
        raise ValueError("RUNNER_TEMP must be outside the repository")
    if vendor_src_arg is not None and (args.workflow_url or args.workflow_name):
        raise ValueError("--vendor-src cannot be combined with workflow artifact options")
    workflow_repo = (
        github_repo_from_workflow_url(args.workflow_url) if args.workflow_url else None
    )
    if args.workflow_url and workflow_repo is None:
        raise ValueError(
            "could not derive a GitHub repository from --workflow-url; expected "
            "https://github.com/<owner>/<repo>/actions/runs/<run-id>"
        )
    if (
        workflow_repo is not None
        and args.github_repo is not None
        and workflow_repo.casefold() != args.github_repo.casefold()
    ):
        raise ValueError(
            f"--workflow-url belongs to {workflow_repo}, but --github-repo selects "
            f"{args.github_repo}"
        )
    github_repo = args.github_repo or workflow_repo
    if vendor_src_arg is None:
        github_repo = resolve_github_repo(github_repo)

    packages = expand_packages(list(args.packages))
    native_component_sets = collect_native_component_sets(packages)
    print("Expanded packages: " + ", ".join(packages), flush=True)
    if native_component_sets:
        component_sets = [
            "(" + ", ".join(components) + ") -> " + ", ".join(targets)
            for components, targets in native_component_sets
        ]
        print(
            "Native component sets: " + ", ".join(component_sets),
            flush=True,
        )
    vendor_src_by_native_key: dict[tuple[tuple[str, ...], tuple[str, ...]], Path] = {}
    vendor_temp_roots: list[Path] = []
    artifacts_temp_root: Path | None = None
    staged_output_root: Path | None = None
    cleanup_artifacts_root = False
    resolved_head_sha: str | None = None

    final_messages = []

    try:
        if native_component_sets:
            if vendor_src_arg is not None:
                vendor_src = vendor_src_arg.resolve(strict=True)
                if not vendor_src.is_dir():
                    raise RuntimeError(f"--vendor-src is not a directory: {vendor_src}")
                for components, targets in native_component_sets:
                    missing_targets = [
                        target for target in targets if not (vendor_src / target).is_dir()
                    ]
                    if missing_targets:
                        raise RuntimeError(
                            "--vendor-src is missing canonical targets: "
                            + ", ".join(missing_targets)
                        )
                    vendor_src_by_native_key[(components, targets)] = vendor_src
            else:
                assert github_repo is not None
                workflow_url, resolved_head_sha = resolve_workflow_url(
                    args.release_version,
                    args.workflow_url,
                    github_repo,
                    args.workflow_name,
                )
                if not resolved_head_sha:
                    raise RuntimeError("native artifact workflow did not report a head SHA")
                workflow_id = workflow_id_from_url(workflow_url)
                ensure_source_matches_workflow(resolved_head_sha)
                print(f"Using native artifacts from {workflow_url}", flush=True)
                if args.cache_dir is None:
                    artifacts_temp_root = Path(
                        tempfile.mkdtemp(prefix="npm-native-artifacts-", dir=runner_temp)
                    )
                    cleanup_artifacts_root = True
                    print(
                        f"Caching downloaded artifacts in {artifacts_temp_root}",
                        flush=True,
                    )
                else:
                    artifacts_temp_root = (
                        args.cache_dir / github_repo_cache_key(github_repo) / workflow_id
                    ).resolve()
                    artifacts_temp_root.mkdir(parents=True, exist_ok=True)
                    print(
                        f"Using persistent native artifact cache {artifacts_temp_root}",
                        flush=True,
                    )
                extracted_cache_dir = artifacts_temp_root / "_extracted-codex-packages"
                for components, targets in native_component_sets:
                    vendor_temp_root = Path(
                        tempfile.mkdtemp(prefix="npm-native-", dir=runner_temp)
                    )
                    vendor_temp_roots.append(vendor_temp_root)
                    print(
                        "Installing native components "
                        + ", ".join(components)
                        + " for targets "
                        + ", ".join(targets)
                        + f" into {vendor_temp_root}",
                        flush=True,
                    )
                    install_native_components(
                        workflow_url,
                        github_repo,
                        set(components),
                        targets,
                        vendor_temp_root,
                        artifacts_temp_root,
                        extracted_cache_dir=extracted_cache_dir,
                        max_download_workers=args.max_download_workers,
                        vendor_copy_mode=args.vendor_copy_mode,
                    )
                    vendor_src_by_native_key[(components, targets)] = (
                        vendor_temp_root / "vendor"
                    )

        staged_output_root = Path(
            tempfile.mkdtemp(prefix="npm-output-", dir=runner_temp)
        )
        staged_results = stage_packages(
            packages,
            args.release_version,
            staged_output_root,
            runner_temp,
            vendor_src_by_native_key,
            args.keep_staging_dirs,
            args.max_stage_workers,
        )

        if resolved_head_sha:
            owned_paths = []
            if artifacts_temp_root is not None:
                owned_paths.append(artifacts_temp_root)
            ensure_source_matches_workflow(
                resolved_head_sha,
                owned_paths=owned_paths,
            )

        for result in commit_staged_packages(staged_results, output_dir):
            final_messages.append(f"Staged {result.package} at {result.pack_output}")
    finally:
        if not args.keep_staging_dirs:
            for vendor_temp_root in vendor_temp_roots:
                shutil.rmtree(vendor_temp_root, ignore_errors=True)
        if cleanup_artifacts_root and artifacts_temp_root is not None:
            shutil.rmtree(artifacts_temp_root, ignore_errors=True)
        if staged_output_root is not None:
            shutil.rmtree(staged_output_root, ignore_errors=True)

    for msg in final_messages:
        print(msg, flush=True)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
