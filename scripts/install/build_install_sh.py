#!/usr/bin/env python3
"""Build the standalone macOS/Linux installer used by GitHub releases."""

from __future__ import annotations

import argparse
from pathlib import Path


INSTALL_DIR = Path(__file__).resolve().parent
SOURCE_PATH = INSTALL_DIR / "install.sh"
HELPER_PATH = INSTALL_DIR / "install_release.sh"
BEGIN_MARKER = "# BEGIN INSTALL RELEASE HELPERS"
END_MARKER = "# END INSTALL RELEASE HELPERS"


def standalone_installer_text(
    source_path: Path = SOURCE_PATH,
    helper_path: Path = HELPER_PATH,
) -> str:
    source_lines = source_path.read_text(encoding="utf-8").splitlines()
    helper_lines = helper_path.read_text(encoding="utf-8").splitlines()
    if not helper_lines or helper_lines[0] != "#!/bin/sh":
        raise ValueError(f"{helper_path} must start with #!/bin/sh")
    if source_lines.count(BEGIN_MARKER) != 1 or source_lines.count(END_MARKER) != 1:
        raise ValueError("install.sh must contain one release-helper marker block")

    begin = source_lines.index(BEGIN_MARKER)
    end = source_lines.index(END_MARKER)
    if end <= begin:
        raise ValueError("install.sh release-helper markers are out of order")

    bundled_lines = [
        *source_lines[:begin],
        "# Bundled release metadata and checksum helpers.",
        *helper_lines[1:],
        *source_lines[end + 1 :],
    ]
    return "\n".join(bundled_lines) + "\n"


def build_standalone_installer(output_path: Path) -> None:
    output_path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = output_path.with_name(f".{output_path.name}.tmp")
    temp_path.write_text(standalone_installer_text(), encoding="utf-8", newline="\n")
    temp_path.chmod(SOURCE_PATH.stat().st_mode)
    temp_path.replace(output_path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    build_standalone_installer(args.output)
    print(args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
