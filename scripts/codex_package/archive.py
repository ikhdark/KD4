"""Archive writers for canonical Codex package directories."""

import os
import gzip
import hashlib
import shutil
import subprocess
import tarfile
import tempfile
import zipfile
from pathlib import Path
from typing import BinaryIO

from .layout import MANAGED_PACKAGE_PATHS

ZSTD_PATH_ENV = "CODEX_ZSTD"
ZSTD_SHA256_ENV = "CODEX_ZSTD_SHA256"


def write_archive(
    package_dir: Path,
    archive_path: Path,
    *,
    force: bool,
    entries: list[Path] | None = None,
    compression: str = "default",
) -> None:
    package_dir, archive_path, archive_format = validate_archive_output(
        package_dir,
        archive_path,
        force=force,
        compression=compression,
    )
    archive_path.parent.mkdir(parents=True, exist_ok=True)
    temp_file = tempfile.NamedTemporaryFile(
        prefix=f"{archive_path.name}.",
        suffix=".tmp",
        dir=archive_path.parent,
        delete=False,
    )
    temp_path = Path(temp_file.name)
    temp_file.close()
    try:
        if archive_format == "tar.gz":
            write_tar_archive(
                package_dir,
                temp_path,
                mode="w:gz",
                entries=entries,
                compression=compression,
            )
        elif archive_format == "tar.zst":
            write_tar_zst_archive(
                package_dir,
                temp_path,
                entries=entries,
                compression=compression,
            )
        elif archive_format == "zip":
            write_zip_archive(
                package_dir,
                temp_path,
                entries=entries,
                compression=compression,
            )
        else:
            raise AssertionError(f"unexpected archive format: {archive_format}")
        activate_archive(temp_path, archive_path, force=force)
    finally:
        temp_path.unlink(missing_ok=True)


def activate_archive(staged_path: Path, archive_path: Path, *, force: bool) -> None:
    """Publish a staged archive without overwriting unless explicitly allowed."""
    if force:
        staged_path.replace(archive_path)
        return

    try:
        # Staging beside the destination keeps this on one filesystem. Creating a
        # hard link is atomic and, unlike Path.replace(), fails if another writer
        # published the destination after our initial validation.
        os.link(staged_path, archive_path)
    except FileExistsError as exc:
        raise RuntimeError(f"Archive output already exists: {archive_path}") from exc
    staged_path.unlink()


def validate_archive_output(
    package_dir: Path,
    archive_path: Path,
    *,
    force: bool,
    compression: str = "default",
) -> tuple[Path, Path, str]:
    package_dir = package_dir.resolve()
    archive_path = archive_path.resolve()
    if is_relative_to(archive_path, package_dir):
        raise RuntimeError(
            f"Archive output must be outside the package directory: {archive_path}"
        )
    if archive_path.exists() and not force:
        raise RuntimeError(f"Archive output already exists: {archive_path}")

    archive_format = archive_format_for_path(archive_path)
    if compression not in {"default", "fast", "none"}:
        raise RuntimeError(f"Unsupported archive compression mode: {compression}")
    if archive_format == "tar.gz" and compression == "none":
        raise RuntimeError(
            "compression 'none' conflicts with a .tar.gz/.tgz output; "
            "use a .tar.zst or .zip output, or a gzip compression level."
        )
    return package_dir, archive_path, archive_format


def is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def archive_format_for_path(path: Path) -> str:
    suffixes = path.suffixes
    if suffixes[-2:] == [".tar", ".gz"] or path.suffix == ".tgz":
        return "tar.gz"
    if suffixes[-2:] == [".tar", ".zst"]:
        return "tar.zst"
    if path.suffix == ".zip":
        return "zip"
    raise RuntimeError(
        f"Unsupported archive suffix for {path}. Use .tar.gz, .tgz, .tar.zst, or .zip."
    )


def write_tar_archive(
    package_dir: Path,
    archive_path: Path,
    *,
    mode: str,
    entries: list[Path] | None = None,
    compression: str = "default",
) -> None:
    if mode.endswith(":gz"):
        compresslevel = 9
        if compression == "fast":
            compresslevel = 1
        elif compression == "none":
            # Silently dropping ":gz" would produce an uncompressed tar under
            # a gzip filename that gzip-expecting consumers reject.
            raise RuntimeError(
                "compression 'none' conflicts with a .tar.gz/.tgz output; "
                "use a .tar.zst or .zip output, or a gzip compression level."
            )

        with archive_path.open("wb") as raw:
            with gzip.GzipFile(
                filename="",
                fileobj=raw,
                mode="wb",
                compresslevel=compresslevel,
                mtime=0,
            ) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as archive:
                    write_tar_entries(archive, package_dir, entries=entries)
        return

    with tarfile.open(archive_path, mode) as archive:
        write_tar_entries(archive, package_dir, entries=entries)


def write_tar_zst_archive(
    package_dir: Path,
    archive_path: Path,
    *,
    entries: list[Path] | None = None,
    compression: str = "default",
) -> None:
    zstd_command = resolve_zstd_command()
    if compression == "none":
        zstd_level = "-0"
    elif compression == "fast":
        zstd_level = "-1"
    else:
        zstd_level = "-19"
    cmd = [*zstd_command, "-T0", zstd_level, "-f", "-", "-o", str(archive_path)]
    process = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    try:
        if process.stdin is None:
            raise RuntimeError("zstd stdin pipe was not created")
        with process.stdin:
            write_tar_stream(package_dir, process.stdin, entries=entries)
        return_code = process.wait()
    except BaseException:
        process.kill()
        process.wait()
        raise
    if return_code != 0:
        raise subprocess.CalledProcessError(return_code, cmd)


def resolve_zstd_command(
    *,
    environ: dict[str, str] | None = None,
) -> list[str]:
    values = os.environ if environ is None else environ
    path_value = values.get(ZSTD_PATH_ENV)
    expected_digest = values.get(ZSTD_SHA256_ENV, "").lower()
    if not path_value or not expected_digest:
        raise RuntimeError(
            ".tar.zst requires a pinned compressor: set CODEX_ZSTD to the "
            "executable and CODEX_ZSTD_SHA256 to its SHA-256 digest"
        )
    path = Path(path_value).resolve()
    if not path.is_file():
        raise RuntimeError(f"Pinned zstd executable does not exist: {path}")
    actual_digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if actual_digest != expected_digest:
        raise RuntimeError(
            f"Pinned zstd digest mismatch: expected {expected_digest}, got {actual_digest}"
        )
    return [str(path)]


def write_zip_archive(
    package_dir: Path,
    archive_path: Path,
    *,
    entries: list[Path] | None = None,
    compression: str = "default",
) -> None:
    zip_compression = (
        zipfile.ZIP_STORED if compression == "none" else zipfile.ZIP_DEFLATED
    )
    kwargs = {"compression": zip_compression}
    if compression == "fast":
        kwargs["compresslevel"] = 1

    with zipfile.ZipFile(archive_path, "w", **kwargs) as archive:
        for path in entries if entries is not None else package_entries(package_dir):
            relative_path = archive_member_name(path, package_dir)
            member_name = f"{relative_path}/" if path.is_dir() else relative_path
            info = zipfile.ZipInfo(member_name, date_time=(1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (0o755 if path.is_dir() else 0o644) << 16
            archive.writestr(
                info,
                b"" if path.is_dir() else path.read_bytes(),
                compress_type=zip_compression,
            )


def write_tar_stream(
    package_dir: Path,
    output: BinaryIO,
    *,
    entries: list[Path] | None = None,
) -> None:
    with tarfile.open(fileobj=output, mode="w|") as archive:
        write_tar_entries(archive, package_dir, entries=entries)


def write_tar_entries(
    archive: tarfile.TarFile,
    package_dir: Path,
    *,
    entries: list[Path] | None = None,
) -> None:
    for path in entries if entries is not None else package_entries(package_dir):
        info = archive.gettarinfo(path, arcname=archive_member_name(path, package_dir))
        info.mtime = 0
        info.uid = 0
        info.gid = 0
        info.uname = ""
        info.gname = ""
        info.mode = 0o755 if path.is_dir() else 0o644
        if path.is_dir():
            archive.addfile(info)
        else:
            with path.open("rb") as file:
                archive.addfile(info, file)


def archive_member_name(path: Path, package_dir: Path) -> str:
    return path.relative_to(package_dir).as_posix()


def package_entries(package_dir: Path) -> list[Path]:
    entries: list[Path] = []
    for relative_path in MANAGED_PACKAGE_PATHS:
        root = package_dir / relative_path
        if not root.exists():
            continue
        entries.append(root)
        if root.is_dir():
            entries.extend(root.rglob("*"))
    return sorted(
        entries,
        key=lambda path: path.relative_to(package_dir).as_posix(),
    )
