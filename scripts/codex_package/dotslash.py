"""Fetch executable artifacts from checked-in DotSlash manifests."""

from __future__ import annotations

import hashlib
import json
import shutil
import stat
import tarfile
import tempfile
import zipfile
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path
from pathlib import PurePosixPath
from urllib.parse import urlparse
from urllib.request import urlopen

from .targets import TargetSpec


DOWNLOAD_TIMEOUT_SECS = 60
HASH_CHUNK_BYTES = 8 * 1024 * 1024


_FETCHED_EXECUTABLES: dict[
    tuple[str, Path, str, str, bool], tuple[Path, DotSlashArtifact] | None
] = {}
_JSON_STAMP_CACHE: dict[Path, tuple[tuple[int, int] | None, dict | None]] = {}


@dataclass(frozen=True)
class DotSlashArtifact:
    size: int
    digest: str
    archive_format: str
    archive_member: str
    url: str


def fetch_dotslash_executable(
    spec: TargetSpec,
    *,
    manifest_path: Path,
    artifact_label: str,
    cache_key: str,
    dest_name: str,
    missing_ok: bool = False,
) -> Path | None:
    cache_key_tuple = (
        spec.target,
        manifest_path,
        cache_key,
        dest_name,
        missing_ok,
    )
    if cache_key_tuple in _FETCHED_EXECUTABLES:
        cached = _FETCHED_EXECUTABLES[cache_key_tuple]
        if cached is None:
            return None
        cached_dest, cached_artifact = cached
        if extracted_member_is_valid(cached_dest, cached_artifact):
            return cached_dest
        _FETCHED_EXECUTABLES.pop(cache_key_tuple, None)

    artifact = artifact_for_target(
        spec,
        manifest_path,
        artifact_label=artifact_label,
        missing_ok=missing_ok,
    )
    if artifact is None:
        _FETCHED_EXECUTABLES[cache_key_tuple] = None
        return None

    cache_dir = default_cache_root() / cache_key
    archive_path = cache_dir / archive_filename(artifact.url)
    dest = cache_dir / dest_name

    if extracted_member_is_valid(dest, artifact):
        _FETCHED_EXECUTABLES[cache_key_tuple] = (dest, artifact)
        return dest

    if not archive_is_valid(archive_path, artifact, artifact_label):
        download_archive(artifact.url, archive_path)
        try:
            verify_archive(archive_path, artifact, artifact_label)
        except RuntimeError:
            archive_path.unlink(missing_ok=True)
            raise

    extract_archive_member(archive_path, artifact, dest, artifact_label)
    if not spec.is_windows:
        mode = dest.stat().st_mode
        dest.chmod(mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    write_extracted_member_stamp(dest, artifact)
    _FETCHED_EXECUTABLES[cache_key_tuple] = (dest, artifact)
    return dest


def clear_runtime_caches() -> None:
    _FETCHED_EXECUTABLES.clear()
    _JSON_STAMP_CACHE.clear()
    load_manifest.cache_clear()


def artifact_for_target(
    spec: TargetSpec,
    manifest_path: Path,
    *,
    artifact_label: str,
    missing_ok: bool = False,
) -> DotSlashArtifact | None:
    manifest = load_manifest(manifest_path)
    if not isinstance(manifest, dict):
        raise RuntimeError(f"Invalid {artifact_label} manifest: {manifest_path}")
    platforms = manifest.get("platforms")
    if not isinstance(platforms, dict):
        raise RuntimeError(
            f"{artifact_label} manifest {manifest_path} has no platform map"
        )
    platform_info = platforms.get(spec.dotslash_platform)
    if platform_info is None:
        if missing_ok:
            return None
        raise RuntimeError(
            f"{artifact_label} manifest {manifest_path} is missing platform "
            f"{spec.dotslash_platform!r}"
        )
    if not isinstance(platform_info, dict):
        raise RuntimeError(
            f"Invalid {artifact_label} platform metadata for "
            f"{spec.dotslash_platform!r} in {manifest_path}"
        )

    providers = platform_info.get("providers")
    if not isinstance(providers, list) or not providers:
        raise RuntimeError(
            f"{artifact_label} manifest {manifest_path} has no providers for "
            f"{spec.dotslash_platform!r}"
        )

    hash_name = platform_info.get("hash")
    if hash_name != "sha256":
        raise RuntimeError(
            f"Unsupported {artifact_label} hash {hash_name!r} for "
            f"{spec.dotslash_platform!r}; expected sha256"
        )

    try:
        size = int(platform_info["size"])
        digest = str(platform_info["digest"]).lower()
        archive_format = str(platform_info["format"])
        archive_member = str(platform_info["path"])
    except (KeyError, TypeError, ValueError) as exc:
        raise RuntimeError(
            f"Invalid {artifact_label} metadata for {spec.dotslash_platform!r} "
            f"in {manifest_path}"
        ) from exc

    if size < 0:
        raise RuntimeError(
            f"Invalid {artifact_label} archive size {size} for "
            f"{spec.dotslash_platform!r}"
        )
    if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
        raise RuntimeError(
            f"Invalid {artifact_label} sha256 digest for "
            f"{spec.dotslash_platform!r}: {digest!r}"
        )
    if archive_format not in {"tar.gz", "zip"}:
        raise RuntimeError(
            f"Unsupported {artifact_label} archive format {archive_format!r}; "
            "expected tar.gz or zip"
        )
    if not is_safe_archive_member(archive_member):
        raise RuntimeError(
            f"Unsafe {artifact_label} archive member path: {archive_member!r}"
        )

    url = next(
        (
            provider.get("url")
            for provider in providers
            if isinstance(provider, dict)
            and isinstance(provider.get("url"), str)
            and provider.get("url")
        ),
        None,
    )
    if url is None:
        raise RuntimeError(
            f"{artifact_label} manifest {manifest_path} has no URL provider for "
            f"{spec.dotslash_platform!r}"
        )

    return DotSlashArtifact(
        size=size,
        digest=digest,
        archive_format=archive_format,
        archive_member=archive_member,
        url=url,
    )


def is_safe_archive_member(member: str) -> bool:
    if not member or "\\" in member:
        return False
    path = PurePosixPath(member)
    return not path.is_absolute() and all(
        part not in {"", ".", ".."} for part in member.split("/")
    )


@lru_cache(maxsize=None)
def load_manifest(manifest_path: Path) -> dict:
    text = manifest_path.read_text(encoding="utf-8")
    if text.startswith("#!"):
        text = "\n".join(text.splitlines()[1:])
    return json.loads(text)


def default_cache_root() -> Path:
    return Path(tempfile.gettempdir()) / "codex-package"


def archive_filename(url: str) -> str:
    filename = Path(urlparse(url).path).name
    if not filename:
        raise RuntimeError(f"Unable to determine archive filename from {url}")
    return filename


def archive_is_valid(
    archive_path: Path,
    artifact: DotSlashArtifact,
    artifact_label: str,
) -> bool:
    if not archive_path.is_file():
        return False
    if verified_archive_stamp_matches(archive_path, artifact):
        return True
    try:
        verify_archive(archive_path, artifact, artifact_label)
    except RuntimeError:
        archive_path.unlink(missing_ok=True)
        return False
    return True


def verify_archive(
    archive_path: Path,
    artifact: DotSlashArtifact,
    artifact_label: str,
) -> None:
    actual_size = archive_path.stat().st_size
    if actual_size != artifact.size:
        raise RuntimeError(
            f"{artifact_label} archive {archive_path} has size {actual_size}, "
            f"expected {artifact.size}"
        )

    digest = hashlib.sha256()
    with open(archive_path, "rb") as fh:
        for chunk in iter(lambda: fh.read(HASH_CHUNK_BYTES), b""):
            digest.update(chunk)

    actual_digest = digest.hexdigest()
    if actual_digest != artifact.digest:
        raise RuntimeError(
            f"{artifact_label} archive {archive_path} has sha256 {actual_digest}, "
            f"expected {artifact.digest}"
        )
    write_verified_archive_stamp(archive_path, artifact)


def download_archive(url: str, archive_path: Path) -> None:
    archive_path.parent.mkdir(parents=True, exist_ok=True)
    temp_file = tempfile.NamedTemporaryFile(
        prefix=f"{archive_path.name}.",
        suffix=".tmp",
        dir=archive_path.parent,
        delete=False,
    )
    temp_path = Path(temp_file.name)
    try:
        with temp_file as out:
            with urlopen(url, timeout=DOWNLOAD_TIMEOUT_SECS) as response:
                shutil.copyfileobj(response, out, length=HASH_CHUNK_BYTES)
        temp_path.replace(archive_path)
    finally:
        temp_path.unlink(missing_ok=True)


def verified_archive_stamp_matches(
    archive_path: Path,
    artifact: DotSlashArtifact,
) -> bool:
    stamp = read_json_stamp(verified_archive_stamp_path(archive_path))
    return stamp == {
        "kind": "dotslash-archive",
        "url": artifact.url,
        "size": artifact.size,
        "digest": artifact.digest,
        "file": file_stamp(archive_path),
    }


def write_verified_archive_stamp(
    archive_path: Path,
    artifact: DotSlashArtifact,
) -> None:
    write_json_stamp(
        verified_archive_stamp_path(archive_path),
        {
            "kind": "dotslash-archive",
            "url": artifact.url,
            "size": artifact.size,
            "digest": artifact.digest,
            "file": file_stamp(archive_path),
        },
    )


def verified_archive_stamp_path(archive_path: Path) -> Path:
    return archive_path.with_name(f"{archive_path.name}.verified.json")


def extracted_member_is_valid(dest: Path, artifact: DotSlashArtifact) -> bool:
    if not dest.is_file():
        return False
    stamp = read_json_stamp(extracted_member_stamp_path(dest))
    return stamp == {
        "kind": "dotslash-extracted-member",
        "archive_digest": artifact.digest,
        "archive_format": artifact.archive_format,
        "archive_member": artifact.archive_member,
        "url": artifact.url,
        "file": file_stamp(dest),
    }


def write_extracted_member_stamp(dest: Path, artifact: DotSlashArtifact) -> None:
    write_json_stamp(
        extracted_member_stamp_path(dest),
        {
            "kind": "dotslash-extracted-member",
            "archive_digest": artifact.digest,
            "archive_format": artifact.archive_format,
            "archive_member": artifact.archive_member,
            "url": artifact.url,
            "file": file_stamp(dest),
        },
    )


def extracted_member_stamp_path(dest: Path) -> Path:
    return dest.with_name(f"{dest.name}.extracted.json")


def file_stamp(path: Path) -> dict[str, int]:
    stat_result = path.stat()
    return {
        "size": stat_result.st_size,
        "mtime_ns": stat_result.st_mtime_ns,
    }


def stamp_cache_key(path: Path) -> tuple[int, int] | None:
    try:
        stat_result = path.stat()
    except OSError:
        return None
    return stat_result.st_size, stat_result.st_mtime_ns


def read_json_stamp(path: Path) -> dict | None:
    cache_key = stamp_cache_key(path)
    cached = _JSON_STAMP_CACHE.get(path)
    if cached is not None and cached[0] == cache_key:
        return cached[1]

    try:
        stamp = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        stamp = None
    _JSON_STAMP_CACHE[path] = (cache_key, stamp)
    return stamp


def write_json_stamp(path: Path, value: dict) -> None:
    path.write_text(
        json.dumps(value, sort_keys=True, indent=2) + "\n", encoding="utf-8"
    )
    _JSON_STAMP_CACHE[path] = (stamp_cache_key(path), value)


def extract_archive_member(
    archive_path: Path,
    artifact: DotSlashArtifact,
    dest: Path,
    artifact_label: str,
) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    temp_file = tempfile.NamedTemporaryFile(
        prefix=f"{dest.name}.",
        suffix=".tmp",
        dir=dest.parent,
        delete=False,
    )
    temp_path = Path(temp_file.name)
    temp_file.close()
    try:
        if artifact.archive_format == "tar.gz":
            with tarfile.open(archive_path, "r:gz") as archive:
                try:
                    member = archive.getmember(artifact.archive_member)
                except KeyError as exc:
                    raise RuntimeError(
                        f"{artifact_label} archive {archive_path} is missing "
                        f"{artifact.archive_member!r}"
                    ) from exc
                if not member.isfile():
                    raise RuntimeError(
                        f"{artifact_label} archive member "
                        f"{artifact.archive_member!r} is not a regular file"
                    )

                extracted = archive.extractfile(member)
                if extracted is None:
                    raise RuntimeError(
                        f"{artifact_label} archive member "
                        f"{artifact.archive_member!r} could not be read"
                    )
                with extracted, temp_path.open("wb") as out:
                    shutil.copyfileobj(extracted, out)
        elif artifact.archive_format == "zip":
            with zipfile.ZipFile(archive_path) as archive:
                try:
                    member = archive.getinfo(artifact.archive_member)
                except KeyError as exc:
                    raise RuntimeError(
                        f"{artifact_label} archive {archive_path} is missing "
                        f"{artifact.archive_member!r}"
                    ) from exc
                member_mode = member.external_attr >> 16
                if member.is_dir() or stat.S_ISLNK(member_mode):
                    raise RuntimeError(
                        f"{artifact_label} archive member "
                        f"{artifact.archive_member!r} is not a regular file"
                    )
                with archive.open(member) as extracted, temp_path.open("wb") as out:
                    shutil.copyfileobj(extracted, out)
        else:
            raise RuntimeError(
                f"Unsupported {artifact_label} archive format "
                f"{artifact.archive_format!r}; expected tar.gz or zip"
            )
        temp_path.replace(dest)
    finally:
        temp_path.unlink(missing_ok=True)
