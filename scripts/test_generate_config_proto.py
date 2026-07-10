#!/usr/bin/env python3

from dataclasses import dataclass
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "codex-rs" / "config" / "scripts" / "generate-proto.ps1"
EXPECTED_GENERATED = """// @generated
#![allow(clippy::trivially_copy_pass_by_ref)]

pub struct Generated;
"""


def powershell() -> str | None:
    return shutil.which("powershell") or shutil.which("pwsh")


def write_lf(path: Path, contents: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents, encoding="utf-8", newline="\n")
    return path


@dataclass
class ProtoFixture:
    script: Path
    checked_generated: Path
    cargo_lock: Path
    cargo_lane_log: Path
    default_protoc: Path
    env: dict[str, str]


def create_fixture(
    temp_path: Path,
    *,
    generated_contents: str = EXPECTED_GENERATED,
) -> ProtoFixture:
    repo_root = temp_path / "repo"
    script = repo_root / "codex-rs" / "config" / "scripts" / SCRIPT.name
    script.parent.mkdir(parents=True)
    shutil.copy2(SCRIPT, script)

    proto_dir = repo_root / "codex-rs" / "config" / "src" / "thread_config" / "proto"
    write_lf(proto_dir / "codex.thread_config.v1.proto", 'syntax = "proto3";\n')
    checked_generated = write_lf(
        proto_dir / "codex.thread_config.v1.rs",
        EXPECTED_GENERATED,
    )
    generated_fixture = write_lf(
        temp_path / "generated-fixture.rs",
        generated_contents,
    )
    cargo_lock = write_lf(repo_root / "codex-rs" / "Cargo.lock", "fixture lock\n")

    cargo_lane_log = temp_path / "cargo-lane.log"
    write_lf(
        repo_root / "scripts" / "cargo-lane.ps1",
        """$ProgramArgs = @($args | Select-Object -Skip 2)
$protoDir = $ProgramArgs[-1]
[System.IO.File]::Copy(
    $env:GENERATED_FIXTURE,
    (Join-Path $protoDir 'codex.thread_config.v1.rs'),
    $true
)
[System.IO.File]::WriteAllLines($env:CARGO_LANE_LOG, $ProgramArgs)
exit 0
""",
    )

    user_profile = temp_path / "user"
    default_protoc = (
        user_profile
        / ".cargo"
        / "registry"
        / "src"
        / "index.test"
        / "protoc-bin-vendored-win32-3.2.0"
        / "bin"
        / "protoc.exe"
    )
    default_protoc.parent.mkdir(parents=True)
    default_protoc.write_bytes(b"fake protoc")

    fake_bin = temp_path / "fake-bin"
    fake_bin.mkdir()
    (fake_bin / "rustfmt.cmd").write_text(
        "@echo off\r\nexit /b 0\r\n",
        encoding="utf-8",
    )

    env = os.environ.copy()
    env.pop("CARGO_HOME", None)
    env.pop("PROTOC", None)
    env["USERPROFILE"] = str(user_profile)
    env["GENERATED_FIXTURE"] = str(generated_fixture)
    env["CARGO_LANE_LOG"] = str(cargo_lane_log)
    env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "")
    return ProtoFixture(
        script=script,
        checked_generated=checked_generated,
        cargo_lock=cargo_lock,
        cargo_lane_log=cargo_lane_log,
        default_protoc=default_protoc,
        env=env,
    )


class GenerateConfigProtoTest(unittest.TestCase):
    def setUp(self) -> None:
        self.shell = powershell()
        if self.shell is None:
            self.skipTest("PowerShell is not available")

    def run_fixture(
        self,
        fixture: ProtoFixture,
        *args: str,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(fixture.script),
                *args,
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
            env=fixture.env,
        )

    def assert_locked_lane_args(self, fixture: ProtoFixture) -> None:
        lane_args = fixture.cargo_lane_log.read_text(encoding="utf-8").splitlines()
        self.assertEqual(
            lane_args[:-1],
            [
                "cargo",
                "run",
                "--locked",
                "-p",
                "codex-config",
                "--example",
                "generate-proto",
            ],
        )
        self.assertTrue(lane_args[-1].endswith("proto"), lane_args[-1])

    def test_check_uses_default_cargo_home_and_locked_generation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = create_fixture(Path(temp_dir))
            generated_before = fixture.checked_generated.read_bytes()
            lock_before = fixture.cargo_lock.read_bytes()

            result = self.run_fixture(fixture, "-Check")

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"Using protoc: {fixture.default_protoc}", result.stdout)
            self.assertIn("Config proto is up to date:", result.stdout)
            self.assertEqual(fixture.checked_generated.read_bytes(), generated_before)
            self.assertEqual(fixture.cargo_lock.read_bytes(), lock_before)
            self.assertNotIn(b"\r", generated_before)
            self.assertFalse(generated_before.startswith(b"\xef\xbb\xbf"))
            self.assert_locked_lane_args(fixture)

    def test_explicit_protoc_precedes_environment_and_default(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            fixture = create_fixture(temp_path)
            env_protoc = temp_path / "env-protoc.exe"
            env_protoc.write_bytes(b"env protoc")
            explicit_protoc = temp_path / "explicit-protoc.exe"
            explicit_protoc.write_bytes(b"explicit protoc")
            fixture.env["PROTOC"] = str(env_protoc)

            result = self.run_fixture(
                fixture,
                "-Check",
                "-ProtocPath",
                str(explicit_protoc),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"Using protoc: {explicit_protoc}", result.stdout)
            self.assertNotIn(f"Using protoc: {env_protoc}", result.stdout)
            self.assert_locked_lane_args(fixture)

    def test_stale_check_fails_without_replacing_binding_or_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = create_fixture(
                Path(temp_dir),
                generated_contents=EXPECTED_GENERATED.replace(
                    "pub struct Generated;",
                    "pub struct Changed;",
                ),
            )
            generated_before = fixture.checked_generated.read_bytes()
            lock_before = fixture.cargo_lock.read_bytes()

            result = self.run_fixture(fixture, "-Check")

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Generated config proto is stale", result.stderr)
            self.assertEqual(fixture.checked_generated.read_bytes(), generated_before)
            self.assertEqual(fixture.cargo_lock.read_bytes(), lock_before)
            self.assert_locked_lane_args(fixture)

    def test_write_replaces_stale_binding_atomically_without_touching_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            changed = EXPECTED_GENERATED.replace(
                "pub struct Generated;",
                "pub struct Changed;",
            )
            fixture = create_fixture(Path(temp_dir), generated_contents=changed)
            lock_before = fixture.cargo_lock.read_bytes()

            result = self.run_fixture(fixture)

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("Updated config proto:", result.stdout)
            self.assertEqual(
                fixture.checked_generated.read_text(encoding="utf-8"),
                changed,
            )
            self.assertEqual(fixture.cargo_lock.read_bytes(), lock_before)
            self.assertEqual(
                list(fixture.checked_generated.parent.glob(".*.tmp*")),
                [],
            )
            self.assert_locked_lane_args(fixture)

    def test_just_recipes_expose_write_arguments_and_named_check(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        windows_recipe = justfile.split("[windows]\ngenerate-config-proto *args:", 1)[1]
        windows_recipe = windows_recipe.split("\n\n", 1)[0]

        self.assertIn("Select-Object -Skip 1", windows_recipe)
        self.assertIn('generate-proto.ps1" @forwarded_args', windows_recipe)
        self.assertIn("generate-config-proto-check:", justfile)
        self.assertIn('generate-proto.ps1" -Check', justfile)
        self.assertIn("generate-proto.sh --check", justfile)


if __name__ == "__main__":
    unittest.main()
