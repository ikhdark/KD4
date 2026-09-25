#!/usr/bin/env python3
"""Report Codex resolution for this process and optional extension inventory."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class BinaryProbe:
    label: str
    path: str | None
    exists: bool
    version: str | None


def run_version(path: str | None, *, enabled: bool) -> str | None:
    if not enabled or not path:
        return None
    try:
        completed = subprocess.run(
            [path, "--version"],
            cwd=REPO_ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    output = completed.stdout.strip().splitlines()
    return output[0] if completed.returncode == 0 and output else None


def desktop_target() -> str | None:
    # Same precedence as `codex doctor`: Desktop launches CODEX_CLI_PATH, which
    # the publisher sets to the installed target.
    cli_path = os.environ.get("CODEX_CLI_PATH")
    if cli_path:
        return cli_path
    publish_dir = os.environ.get("CODEX_LOCAL_PUBLISH_DIR")
    if publish_dir:
        return str(Path(publish_dir) / "codex.exe")
    return str(Path.home() / "Desktop" / "LOCAL-KD" / "bin" / "codex.exe")


def extension_candidates(limit: int = 8) -> list[str]:
    roots = [
        Path.home() / ".vscode" / "extensions",
        Path.home() / ".vscode-insiders" / "extensions",
        Path.home() / ".vscode-server" / "extensions",
        Path.home() / ".vscode-server-insiders" / "extensions",
    ]
    names = ("codex", "codex.exe")
    matches: list[str] = []
    for root in roots:
        if not root.is_dir():
            continue
        for directory, dirnames, filenames in os.walk(
            root, onerror=lambda _error: None
        ):
            # VS Code leaves `.<uuid>` VSIX staging folders here; they sort first
            # and would fill the limit with binaries no installed extension uses.
            dirnames[:] = sorted(name for name in dirnames if not name.startswith("."))
            for name in sorted(filenames):
                if len(matches) >= limit:
                    return matches
                if name not in names:
                    continue
                matches.append(str(Path(directory) / name))
    return matches


def build_probes(run_codex: bool, *, path_only: bool = False) -> list[BinaryProbe]:
    versions: dict[Path, str | None] = {}

    def version(path: str | None) -> str | None:
        if not run_codex or not path:
            return None
        key = Path(path).resolve()
        if key not in versions:
            versions[key] = run_version(path, enabled=True)
        return versions[key]

    path_codex = shutil.which("codex")
    probes = [
        BinaryProbe(
            "path-codex",
            path_codex,
            bool(path_codex),
            version(path_codex),
        ),
    ]
    if path_only:
        return probes
    target = desktop_target()
    probes.append(
        BinaryProbe(
            "desktop-local-target",
            target,
            bool(target and Path(target).exists()),
            version(target),
        )
    )
    for candidate in extension_candidates():
        probes.append(
            BinaryProbe(
                "vscode-extension-candidate",
                candidate,
                Path(candidate).exists(),
                version(candidate),
            )
        )
    return probes


def print_probes(probes: Sequence[BinaryProbe]) -> None:
    print("Observed Codex resolution for this process")
    for probe in probes:
        status = "exists" if probe.exists else "missing"
        version = f" version={probe.version}" if probe.version else ""
        print(f"- {probe.label}: {status} path={probe.path or '<none>'}{version}")
    print(
        "The Desktop target and extension candidates are inventory, not proof of "
        "the runtime Desktop or the extension launches."
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--json", action="store_true", help="Emit machine-readable output."
    )
    parser.add_argument(
        "--no-run-codex",
        action="store_true",
        help="Do not execute discovered Codex binaries; only report paths.",
    )
    parser.add_argument(
        "--expected-binary",
        help="Fail if `codex` on PATH does not resolve to this binary.",
    )
    args = parser.parse_args(argv)

    probes = build_probes(
        run_codex=not args.no_run_codex, path_only=bool(args.expected_binary)
    )
    if args.json:
        print(json.dumps({"probes": [asdict(probe) for probe in probes]}, indent=2))
    else:
        print_probes(probes)
    if args.expected_binary:
        path_probe = next(
            (probe for probe in probes if probe.label == "path-codex"), None
        )
        expected = Path(args.expected_binary).resolve()
        actual = (
            Path(path_probe.path).resolve() if path_probe and path_probe.path else None
        )
        return 0 if actual == expected else 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
