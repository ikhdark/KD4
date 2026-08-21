#!/usr/bin/env python3

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

INSTALL_SCRIPT = Path(__file__).with_name("install.sh")
BUNDLE_SCRIPT = Path(__file__).with_name("build_install_sh.py")
VERSION = "0.142.5"
SH_PATH = shutil.which("sh")
SH_PLATFORM = (
    subprocess.run(
        [SH_PATH, "-c", "uname -s"],
        check=False,
        capture_output=True,
        text=True,
    ).stdout.strip()
    if SH_PATH
    else ""
)
SH_SUPPORTED = SH_PLATFORM in {"Darwin", "Linux"}


@unittest.skipUnless(
    SH_SUPPORTED,
    "install.sh tests require a macOS or Linux POSIX shell",
)
class InstallShTest(unittest.TestCase):
    def test_release_bundle_is_a_standalone_installer(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            bundled_installer = Path(temp_dir) / "install.sh"
            subprocess.run(
                [
                    sys.executable,
                    str(BUNDLE_SCRIPT),
                    "--output",
                    str(bundled_installer),
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            bundled_text = bundled_installer.read_text(encoding="utf-8")
            self.assertNotIn("install_release.sh", bundled_text)
            self.assertIn("release_asset_digest_or_empty()", bundled_text)
            subprocess.run([SH_PATH, "-n", str(bundled_installer)], check=True)

            result, requests = run_installer(
                VERSION,
                install_script=bundled_installer,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(
            requests[0],
            f"https://api.github.com/repos/openai/codex/releases/tags/rust-v{VERSION}",
        )

    def test_metadata_fetch_failure_is_not_reported_as_missing_assets(self) -> None:
        result, requests = run_installer(VERSION, metadata_failure=True)

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(
            requests,
            [
                "https://api.github.com/repos/openai/codex/releases/tags/"
                f"rust-v{VERSION}"
            ],
        )
        self.assertIn(
            f"Could not fetch GitHub release metadata for Codex {VERSION}",
            result.stderr,
        )
        self.assertNotIn("Could not find Codex package", result.stderr)

    def test_exact_release_fetches_metadata_once(self) -> None:
        result, requests = run_installer(VERSION)

        self.assertNotEqual(result.returncode, 0)
        metadata_url = (
            f"https://api.github.com/repos/openai/codex/releases/tags/rust-v{VERSION}"
        )
        checksum_url = (
            "https://github.com/openai/codex/releases/download/"
            f"rust-v{VERSION}/codex-package_SHA256SUMS"
        )
        self.assertEqual(requests.count(metadata_url), 1)
        self.assertEqual(requests.count(checksum_url), 1)
        self.assertIn(f"Resolved version: {VERSION}", result.stdout)

    def test_latest_release_reuses_version_metadata(self) -> None:
        result, requests = run_installer("latest")

        self.assertNotEqual(result.returncode, 0)
        metadata_url = "https://api.github.com/repos/openai/codex/releases/latest"
        checksum_url = (
            "https://github.com/openai/codex/releases/download/"
            f"rust-v{VERSION}/codex-package_SHA256SUMS"
        )
        self.assertEqual(requests.count(metadata_url), 1)
        self.assertEqual(requests.count(checksum_url), 1)
        self.assertIn(f"Resolved version: {VERSION}", result.stdout)

    def test_corrupted_same_version_sidecar_is_reinstalled(self) -> None:
        intact_result, intact_requests = run_installer(
            VERSION,
            seed_existing_release=True,
        )
        result, requests = run_installer(
            VERSION,
            seed_existing_release=True,
            corrupt_sidecar=True,
        )

        self.assertEqual(intact_result.returncode, 0, intact_result.stderr)
        self.assertEqual(intact_requests, [])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "Found incomplete existing release",
            result.stderr,
        )
        self.assertIn(
            "https://github.com/openai/codex/releases/download/"
            f"rust-v{VERSION}/codex-package-x86_64-unknown-linux-musl.tar.gz",
            requests,
        )


def run_installer(
    release: str,
    *,
    metadata_failure: bool = False,
    seed_existing_release: bool = False,
    corrupt_sidecar: bool = False,
    install_script: Path = INSTALL_SCRIPT,
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    with tempfile.TemporaryDirectory() as temp_dir:
        root = Path(temp_dir)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        request_log = root / "requests.log"
        fake_curl = bin_dir / "curl"
        fake_curl.write_text(
            textwrap.dedent(
                """\
                #!/bin/sh
                url=""
                for arg in "$@"; do
                  case "$arg" in
                    https://*) url="$arg" ;;
                  esac
                done
                printf '%s\n' "$url" >>"$CODEX_TEST_REQUEST_LOG"

                case "$url" in
                  https://api.github.com/*)
                    if [ "$CODEX_TEST_METADATA_FAILURE" = "1" ]; then
                      echo "curl: (22) The requested URL returned error: 403" >&2
                      exit 22
                    fi
                    printf '%s\n' "$CODEX_TEST_METADATA_JSON"
                    ;;
                  *)
                    exit 22
                    ;;
                esac
                """
            ),
            encoding="utf-8",
            newline="\n",
        )
        fake_curl.chmod(0o755)

        fake_uname = bin_dir / "uname"
        fake_uname.write_text(
            textwrap.dedent(
                """\
                #!/bin/sh
                case "${1:-}" in
                  -s) printf 'Linux\n' ;;
                  -m) printf 'x86_64\n' ;;
                  *) printf 'Linux\n' ;;
                esac
                """
            ),
            encoding="utf-8",
            newline="\n",
        )
        fake_uname.chmod(0o755)

        shell_root = subprocess.run(
            [SH_PATH, "-c", 'cd "$1" && pwd', "sh", str(root)],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        if seed_existing_release:
            seed_package_release(
                root / "codex-home",
                corrupt_sidecar=corrupt_sidecar,
            )
        env = os.environ.copy()
        env.update(
            {
                "CODEX_HOME": f"{shell_root}/codex-home",
                "CODEX_INSTALL_DIR": f"{shell_root}/install-bin",
                "CODEX_NON_INTERACTIVE": "1",
                "CODEX_RELEASE": release,
                "CODEX_TEST_METADATA_FAILURE": "1" if metadata_failure else "0",
                "CODEX_TEST_METADATA_JSON": release_metadata(),
                "CODEX_TEST_REQUEST_LOG": f"{shell_root}/requests.log",
                "HOME": f"{shell_root}/home",
                "SHELL": SH_PATH,
            }
        )
        result = subprocess.run(
            [
                SH_PATH,
                "-c",
                'PATH="$1:$PATH"; export PATH; exec sh "$2"',
                "sh",
                f"{shell_root}/bin",
                str(install_script),
            ],
            capture_output=True,
            check=False,
            env=env,
            text=True,
        )
        requests = (
            request_log.read_text(encoding="utf-8").splitlines()
            if request_log.exists()
            else []
        )
        return result, requests


def seed_package_release(codex_home: Path, *, corrupt_sidecar: bool) -> None:
    target = "x86_64-unknown-linux-musl"
    release_dir = (
        codex_home / "packages" / "standalone" / "releases" / f"{VERSION}-{target}"
    )
    managed_files = {
        "codex-package.json": json.dumps(
            {
                "layoutVersion": 1,
                "version": VERSION,
                "target": target,
                "variant": "codex",
                "entrypoint": "bin/codex",
                "resourcesDir": "codex-resources",
                "pathDir": "codex-path",
            },
            indent=2,
        ).encode(),
        "bin/codex": f"#!/bin/sh\nprintf 'codex-cli {VERSION}\\n'\n".encode(),
        "bin/codex-code-mode-host": b"#!/bin/sh\nexit 0\n",
        "codex-path/rg": b"#!/bin/sh\nexit 0\n",
        "codex-resources/bwrap": b"#!/bin/sh\nexit 0\n",
        "codex-resources/zsh/bin/zsh": b"#!/bin/sh\nexit 0\n",
    }
    manifest_lines = []
    for relative_path, contents in managed_files.items():
        path = release_dir / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)
        path.chmod(0o755)
        digest = hashlib.sha256(contents).hexdigest()
        manifest_lines.append(f"{digest}  {relative_path}\n")

    tree_sha256 = hashlib.sha256("".join(manifest_lines).encode()).hexdigest()
    (release_dir / "codex-install.env").write_text(
        f"version={VERSION}\n"
        f"target={target}\n"
        "layout=package\n"
        f"tree_sha256={tree_sha256}\n",
        encoding="utf-8",
        newline="\n",
    )
    (release_dir / "codex").symlink_to("bin/codex")

    if corrupt_sidecar:
        (release_dir / "bin" / "codex-code-mode-host").write_text(
            "#!/bin/sh\necho corrupted\n",
            encoding="utf-8",
            newline="\n",
        )


def release_metadata() -> str:
    assets = [
        {
            "name": f"codex-package-{target}.tar.gz",
            "digest": f"sha256:{'a' * 64}",
        }
        for target in (
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
        )
    ]
    assets.append(
        {
            "name": "codex-package_SHA256SUMS",
            "digest": f"sha256:{'b' * 64}",
        }
    )
    return json.dumps(
        {"tag_name": f"rust-v{VERSION}", "assets": assets},
        indent=2,
    )


if __name__ == "__main__":
    unittest.main()
