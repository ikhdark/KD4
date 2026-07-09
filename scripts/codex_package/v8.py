"""Codex-built V8 artifact overrides for package Cargo builds."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import tempfile
from collections.abc import Mapping
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from urllib.request import urlopen

from .targets import REPO_ROOT
from .targets import TargetSpec


DOWNLOAD_TIMEOUT_SECS = 120


@dataclass(frozen=True)
class RustyV8ArtifactPair:
    archive: Path
    binding: Path


def resolve_codex_v8_cargo_env(
    spec: TargetSpec,
    *,
    environ: Mapping[str, str] | None = None,
    cache_root: Path | None = None,
) -> dict[str, str]:
    if spec.is_windows:
        return {}

    environ = os.environ if environ is None else environ
    if is_truthy_env(environ.get("V8_FROM_SOURCE")):
        return {}

    archive_override = environ.get("RUSTY_V8_ARCHIVE")
    binding_override = environ.get("RUSTY_V8_SRC_BINDING_PATH")
    if archive_override and binding_override:
        return {}
    if archive_override or binding_override:
        raise RuntimeError(
            "Cargo package builds need RUSTY_V8_ARCHIVE and RUSTY_V8_SRC_BINDING_PATH set together."
        )

    artifacts = fetch_codex_v8_artifacts(spec, cache_root=cache_root)
    return {
        "RUSTY_V8_ARCHIVE": str(artifacts.archive),
        "RUSTY_V8_SRC_BINDING_PATH": str(artifacts.binding),
    }


def fetch_codex_v8_artifacts(
    spec: TargetSpec,
    *,
    version: str | None = None,
    cache_root: Path | None = None,
) -> RustyV8ArtifactPair:
    if spec.is_windows:
        raise RuntimeError(
            f"No Codex-built V8 release artifacts for target: {spec.target}"
        )

    version = version or resolved_v8_crate_version()
    release_url = (
        f"https://github.com/openai/codex/releases/download/rusty-v8-v{version}"
    )
    target = spec.target
    cache_dir = (cache_root or default_cache_root()) / f"rusty-v8-{version}-{target}"
    archive = cache_dir / f"librusty_v8_release_{target}.a.gz"
    binding = cache_dir / f"src_binding_release_{target}.rs"
    checksums = cache_dir / f"rusty_v8_release_{target}.sha256"
    artifacts = [archive, binding]
    artifact_names = {artifact.name for artifact in artifacts}

    expected_checksums = try_load_checksums(checksums, artifact_names)
    if expected_checksums is not None and all(
        has_checksum(artifact, expected_checksums[artifact.name])
        for artifact in artifacts
    ):
        return RustyV8ArtifactPair(archive=archive, binding=binding)

    download_file(f"{release_url}/{checksums.name}", checksums)
    expected_checksums = load_checksums(checksums, artifact_names)
    with ThreadPoolExecutor(max_workers=len(artifacts)) as executor:
        futures = [
            executor.submit(
                ensure_valid_artifact,
                artifact,
                expected_checksums[artifact.name],
                f"{release_url}/{artifact.name}",
            )
            for artifact in artifacts
        ]
        for future in futures:
            future.result()

    return RustyV8ArtifactPair(archive=archive, binding=binding)


def is_truthy_env(value: str | None) -> bool:
    return value in {"1", "true", "TRUE", "yes", "YES", "on", "ON"}


def resolved_v8_crate_version() -> str:
    import tomllib

    cargo_lock = tomllib.loads(
        (REPO_ROOT / "codex-rs" / "Cargo.lock").read_text(encoding="utf-8")
    )
    versions = sorted(
        {
            package["version"]
            for package in cargo_lock["package"]
            if package["name"] == "v8"
        }
    )
    if len(versions) != 1:
        raise RuntimeError(
            f"Expected exactly one resolved v8 version, found: {versions}"
        )
    return versions[0]


def default_cache_root() -> Path:
    return Path(tempfile.gettempdir()) / "codex-package"


def load_checksums(checksums_path: Path, artifact_names: set[str]) -> dict[str, str]:
    checksums: dict[str, str] = {}
    lines = checksums_path.read_text(encoding="utf-8").splitlines()
    if len(lines) != len(artifact_names):
        raise RuntimeError(
            f"Expected {len(artifact_names)} V8 checksums in {checksums_path}, found {len(lines)}."
        )

    for line in lines:
        parts = line.split(maxsplit=1)
        if len(parts) != 2:
            raise RuntimeError(
                f"Invalid V8 checksum line in {checksums_path}: {line!r}"
            )

        digest, artifact_name = parts[0], parts[1].strip()
        if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
            raise RuntimeError(
                f"Invalid V8 checksum digest in {checksums_path}: {digest}"
            )
        if artifact_name not in artifact_names:
            raise RuntimeError(
                f"Unexpected V8 checksum artifact in {checksums_path}: {artifact_name}"
            )
        checksums[artifact_name] = digest

    if checksums.keys() != artifact_names:
        raise RuntimeError(
            f"V8 checksum manifest {checksums_path} does not cover {artifact_names}."
        )
    return checksums


def try_load_checksums(
    checksums_path: Path, artifact_names: set[str]
) -> dict[str, str] | None:
    if not checksums_path.is_file():
        return None
    try:
        return load_checksums(checksums_path, artifact_names)
    except (OSError, RuntimeError):
        return None


def ensure_valid_artifact(artifact: Path, checksum: str, url: str) -> None:
    if has_checksum(artifact, checksum):
        return

    artifact.unlink(missing_ok=True)
    download_file(url, artifact)
    if has_checksum(artifact, checksum):
        return

    artifact.unlink(missing_ok=True)
    raise RuntimeError(
        f"Codex-built V8 artifact {artifact} failed checksum validation."
    )


def has_checksum(path: Path, expected: str) -> bool:
    if not path.is_file():
        return False
    if verified_checksum_stamp_matches(path, expected):
        return True

    digest = hashlib.sha256()
    with path.open("rb") as artifact:
        for chunk in iter(lambda: artifact.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected:
        return False
    write_verified_checksum_stamp(path, expected)
    return True


def download_file(url: str, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    temp_file = tempfile.NamedTemporaryFile(
        prefix=f"{dest.name}.",
        suffix=".tmp",
        dir=dest.parent,
        delete=False,
    )
    temp_path = Path(temp_file.name)
    try:
        with temp_file as output:
            with urlopen(url, timeout=DOWNLOAD_TIMEOUT_SECS) as response:
                shutil.copyfileobj(response, output)
        temp_path.replace(dest)
    finally:
        temp_path.unlink(missing_ok=True)


def verified_checksum_stamp_matches(path: Path, expected: str) -> bool:
    stamp = read_json_stamp(verified_checksum_stamp_path(path))
    return stamp == {
        "kind": "v8-artifact-checksum",
        "digest": expected,
        "file": file_stamp(path),
    }


def write_verified_checksum_stamp(path: Path, expected: str) -> None:
    write_json_stamp(
        verified_checksum_stamp_path(path),
        {
            "kind": "v8-artifact-checksum",
            "digest": expected,
            "file": file_stamp(path),
        },
    )


def verified_checksum_stamp_path(path: Path) -> Path:
    return path.with_name(f"{path.name}.verified.json")


def file_stamp(path: Path) -> dict[str, int]:
    stat_result = path.stat()
    return {
        "size": stat_result.st_size,
        "mtime_ns": stat_result.st_mtime_ns,
    }


def read_json_stamp(path: Path) -> dict | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def write_json_stamp(path: Path, value: dict) -> None:
    path.write_text(
        json.dumps(value, sort_keys=True, indent=2) + "\n", encoding="utf-8"
    )
