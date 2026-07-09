"""Archive writers for canonical Codex package directories."""

import shutil
import subprocess
import tarfile
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import BinaryIO

from .targets import REPO_ROOT


ZSTD_DOTSLASH = REPO_ROOT / ".github" / "workflows" / "zstd"


def write_archive(
    package_dir: Path,
    archive_path: Path,
    *,
    force: bool,
    entries: list[Path] | None = None,
    compression: str = "default",
) -> None:
    package_dir = package_dir.resolve()
    archive_path = archive_path.resolve()
    if is_relative_to(archive_path, package_dir):
        raise RuntimeError(
            f"Archive output must be outside the package directory: {archive_path}"
        )

    archive_path.parent.mkdir(parents=True, exist_ok=True)
    if archive_path.exists():
        if not force:
            raise RuntimeError(f"Archive output already exists: {archive_path}")
        archive_path.unlink()

    archive_format = archive_format_for_path(archive_path)
    if archive_format == "tar.gz":
        write_tar_archive(
            package_dir,
            archive_path,
            mode="w:gz",
            entries=entries,
            compression=compression,
        )
    elif archive_format == "tar.zst":
        write_tar_zst_archive(
            package_dir,
            archive_path,
            entries=entries,
            compression=compression,
        )
    elif archive_format == "zip":
        write_zip_archive(
            package_dir,
            archive_path,
            entries=entries,
            compression=compression,
        )
    else:
        raise AssertionError(f"unexpected archive format: {archive_format}")


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
    kwargs = {}
    if mode.endswith(":gz"):
        if compression == "fast":
            kwargs["compresslevel"] = 1
        elif compression == "none":
            # Silently dropping ":gz" would produce an uncompressed tar under
            # a gzip filename that gzip-expecting consumers reject.
            raise RuntimeError(
                "compression 'none' conflicts with a .tar.gz/.tgz output; "
                "use a .tar.zst or .zip output, or a gzip compression level."
            )

    with tarfile.open(archive_path, mode, **kwargs) as archive:
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
    dotslash_manifest: Path = ZSTD_DOTSLASH,
    which: Callable[[str], str | None] = shutil.which,
) -> list[str]:
    zstd = which("zstd")
    if zstd is not None:
        return [zstd]

    dotslash = which("dotslash")
    if dotslash is not None and dotslash_manifest.is_file():
        return [dotslash, str(dotslash_manifest)]

    # The DotSlash wrapper referenced by ZSTD_DOTSLASH does not exist in this
    # fork, so only suggest it when the manifest is actually present.
    hint = (
        f", or install DotSlash so the repository wrapper can run: {dotslash_manifest}"
        if dotslash_manifest.is_file()
        else " (e.g. `scoop install zstd` or `winget install Facebook.Zstandard`)"
    )
    raise RuntimeError(
        f"zstd is required to write .tar.zst archives. Install zstd{hint}"
    )


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
            if path.is_dir():
                archive.write(path, f"{relative_path}/")
            else:
                archive.write(path, relative_path)


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
        archive.add(
            path,
            arcname=archive_member_name(path, package_dir),
            recursive=False,
        )


def archive_member_name(path: Path, package_dir: Path) -> str:
    return path.relative_to(package_dir).as_posix()


def package_entries(package_dir: Path) -> list[Path]:
    return sorted(
        package_dir.rglob("*"),
        key=lambda path: path.relative_to(package_dir).as_posix(),
    )
