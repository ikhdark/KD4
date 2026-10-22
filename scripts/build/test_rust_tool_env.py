from __future__ import annotations

import contextlib
import tempfile
import os
import socket
import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock

from scripts.build import rust_tool_env


@contextlib.contextmanager
def loopback_listener():
    """A local listener standing in for a shared server, without accepting."""
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        server.listen()
        yield server.getsockname()[1]


def unused_loopback_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class SharedSccacheStartupTest(unittest.TestCase):
    def test_reachable_server_is_reused_without_a_start_server_spawn(self) -> None:
        # Validation f6ed4cdc: the lane had already started the shared server,
        # yet the runner's below-normal `--start-server` spawn timed out after
        # 10 s and aborted the run before Cargo started.
        with (
            loopback_listener() as port,
            mock.patch.object(rust_tool_env.subprocess, "run") as start,
        ):
            rust_tool_env.prepare_shared_sccache(
                env={
                    "RUSTC_WRAPPER": "C:/tools/sccache.exe",
                    "SCCACHE_SERVER_PORT": str(port),
                }
            )
        start.assert_not_called()

    def test_unreachable_server_starts_once_and_accepts_a_concurrent_winner(
        self,
    ) -> None:
        port = unused_loopback_port()
        env = {"RUSTC_WRAPPER": "sccache", "SCCACHE_SERVER_PORT": str(port)}
        timeout = subprocess.TimeoutExpired(["sccache", "--start-server"], 10)
        with socket.socket() as concurrent:

            def start(*_args, **_kwargs):
                # Another lane binds the address while this client times out.
                concurrent.bind(("127.0.0.1", port))
                concurrent.listen()
                raise timeout

            with mock.patch.object(
                rust_tool_env.subprocess, "run", side_effect=start
            ) as run:
                rust_tool_env.prepare_shared_sccache(env=env)
        self.assertEqual(run.call_count, 1)
        self.assertEqual(run.call_args.args[0][1:], ["--start-server"])
        # Without a server, the failure still reaches the caller's fallback;
        # nothing is retried or restarted here.
        with (
            mock.patch.object(
                rust_tool_env.subprocess, "run", side_effect=timeout
            ) as run,
            self.assertRaises(subprocess.TimeoutExpired),
        ):
            rust_tool_env.prepare_shared_sccache(env=env)
        self.assertEqual(run.call_count, 1)

    def test_probe_targets_only_the_unambiguous_loopback_port(self) -> None:
        self.assertEqual(
            rust_tool_env.sccache_server_address({}), ("127.0.0.1", 4226)
        )
        self.assertEqual(
            rust_tool_env.sccache_server_address({"SCCACHE_SERVER_PORT": "4300"}),
            ("127.0.0.1", 4300),
        )
        for env in (
            {"SCCACHE_SERVER_PORT": ""},
            {"SCCACHE_SERVER_PORT": "0"},
            {"SCCACHE_SERVER_PORT": "70000"},
            {"SCCACHE_SERVER_PORT": "+4226"},
            {"SCCACHE_SERVER_UDS": "/tmp/sccache.sock"},
        ):
            with self.subTest(env=env):
                self.assertIsNone(rust_tool_env.sccache_server_address(env))
                # An address sccache could resolve differently keeps the
                # existing `--start-server` path instead of guessing.
                with (
                    mock.patch.object(
                        rust_tool_env.socket, "create_connection"
                    ) as connect,
                    mock.patch.object(
                        rust_tool_env.subprocess,
                        "run",
                        return_value=subprocess.CompletedProcess(
                            [], 2, stderr="Address in use"
                        ),
                    ) as start,
                ):
                    rust_tool_env.prepare_shared_sccache(
                        env={"RUSTC_WRAPPER": "sccache", **env}
                    )
                connect.assert_not_called()
                start.assert_called_once()


class RustToolEnvTest(unittest.TestCase):
    def test_local_defaults_are_idempotent_and_preserve_explicit_overrides(
        self,
    ) -> None:
        env = {"CODEX_SCCACHE_CACHE_SIZE": "100G"}

        def which(name):
            return f"/tools/{name}"

        updates = rust_tool_env.local_rust_env(env, repo_root=Path.cwd(), which=which)
        self.assertEqual(updates["RUSTC_WRAPPER"], "/tools/sccache")
        self.assertEqual(updates["SCCACHE_CACHE_SIZE"], "100G")
        self.assertEqual(
            updates["CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"], "/tools/lld-link"
        )
        self.assertEqual(
            updates["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"], "/tools/lld-link"
        )
        self.assertEqual(
            rust_tool_env.local_rust_env(
                env | updates, repo_root=Path.cwd(), which=which
            ),
            {},
        )
        explicit = {
            "RUSTC_WRAPPER": "",
            "CARGO_INCREMENTAL": "1",
            "RUSTFLAGS": "custom",
            "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER": "custom-link",
            "SCCACHE_CACHE_SIZE": "4G",
        }
        self.assertFalse(
            set(rust_tool_env.local_rust_env(explicit, which=which)) & set(explicit)
        )
        self.assertEqual(rust_tool_env.local_rust_env({"CI": "1"}, which=which), {})

    def test_just_shell_loads_shared_policy_outside_the_repository(self) -> None:
        just_shell = Path(__file__).with_name("just-shell.py").resolve()
        with tempfile.TemporaryDirectory() as temp:
            completed = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    "-c",
                    "import runpy, sys; runpy.run_path(sys.argv[1], run_name='loaded')",
                    str(just_shell),
                ],
                cwd=temp,
                check=False,
                capture_output=True,
                text=True,
                env={
                    key: value
                    for key, value in os.environ.items()
                    if key != "PYTHONPATH"
                },
                timeout=30,
            )

        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_shared_sccache_policy_handles_wrappers_and_cache_override(self) -> None:
        self.assertTrue(rust_tool_env.is_sccache_wrapper("C:/tools/sccache.exe"))
        self.assertFalse(rust_tool_env.is_sccache_wrapper("cachepot"))
        self.assertEqual(rust_tool_env.sccache_cache_size({}), "80G")
        self.assertEqual(
            rust_tool_env.sccache_cache_size({"CODEX_SCCACHE_CACHE_SIZE": "100G"}),
            "100G",
        )

    def test_windows_linker_fallback_order_is_shared(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scoop = root / "custom-scoop"
            user = root / "user"
            default = root / "program-files" / "lld-link.exe"
            expected = scoop / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
            expected.parent.mkdir(parents=True)
            expected.write_text("", encoding="utf-8")
            user_linker = (
                user / "scoop" / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
            )
            for candidate in (user_linker, default):
                candidate.parent.mkdir(parents=True)
                candidate.write_text("", encoding="utf-8")

            self.assertEqual(
                rust_tool_env.find_windows_lld_link(
                    {"SCOOP": str(scoop), "USERPROFILE": str(user)},
                    which=lambda _program: "path-linker",
                    default_path=default,
                ),
                "path-linker",
            )
            result = rust_tool_env.find_windows_lld_link(
                {"SCOOP": str(scoop), "USERPROFILE": str(user)},
                which=lambda _program: None,
                default_path=default,
            )
            self.assertEqual(result, str(expected))
            expected.unlink()
            self.assertEqual(
                rust_tool_env.find_windows_lld_link(
                    {"SCOOP": str(scoop), "USERPROFILE": str(user)},
                    which=lambda _program: None,
                    default_path=default,
                ),
                str(user_linker),
            )
            # A directory is not an executable. Skip it rather than masking
            # the next usable fallback (or advertising a broken linker).
            user_linker.unlink()
            user_linker.mkdir()
            self.assertEqual(
                rust_tool_env.find_windows_lld_link(
                    {"SCOOP": str(scoop), "USERPROFILE": str(user)},
                    which=lambda _program: None,
                    default_path=default,
                ),
                str(default),
            )

            default.unlink()
            self.assertIsNone(
                rust_tool_env.find_windows_lld_link(
                    {"SCOOP": str(scoop), "USERPROFILE": str(user)},
                    which=lambda _program: None,
                    default_path=default,
                )
            )


if __name__ == "__main__":
    unittest.main()
