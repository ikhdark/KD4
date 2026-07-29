#!/usr/bin/env python3

import base64
import hashlib
import os
import shutil
import socket
import stat
import subprocess
import tempfile
import textwrap
import threading
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "run_tui_with_exec_server.sh"
BASH = (
    shutil.which("sh") or shutil.which("bash")
    if os.name == "nt"
    else shutil.which("bash") or shutil.which("sh")
)


def write_executable(path: Path, body: str) -> None:
    path.write_text(body, encoding="utf-8", newline="\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def bash_path(path: Path) -> str:
    return str(path).replace("\\", "/")


def bash_available() -> bool:
    if BASH is None:
        return False
    return (
        subprocess.run(
            [
                BASH,
                "-c",
                'test -n "$BASH_VERSION" && test -f "$1"',
                "bash",
                bash_path(SCRIPT),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )


def serve_websocket_handshake(listener: socket.socket) -> threading.Thread:
    def serve() -> None:
        connection, _ = listener.accept()
        with connection:
            request = bytearray()
            while b"\r\n\r\n" not in request:
                chunk = connection.recv(4096)
                if not chunk:
                    return
                request.extend(chunk)
            headers = {}
            for line in bytes(request).decode("iso-8859-1").split("\r\n")[1:]:
                name, delimiter, value = line.partition(":")
                if delimiter:
                    headers[name.strip().lower()] = value.strip()
            key = headers["sec-websocket-key"]
            accept = base64.b64encode(
                hashlib.sha1(
                    (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
                ).digest()
            ).decode("ascii")
            connection.sendall(
                (
                    "HTTP/1.1 101 Switching Protocols\r\n"
                    "Upgrade: websocket\r\n"
                    "Connection: Upgrade\r\n"
                    f"Sec-WebSocket-Accept: {accept}\r\n"
                    "\r\n"
                ).encode("ascii")
            )

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    return thread


def serve_plain_http(listener: socket.socket) -> threading.Thread:
    def serve() -> None:
        connection, _ = listener.accept()
        with connection:
            connection.recv(4096)
            connection.sendall(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    return thread


@unittest.skipUnless(bash_available(), "bash is required for shell launcher tests")
class RunTuiWithExecServerTest(unittest.TestCase):
    def run_script(
        self,
        env: dict[str, str],
        *,
        cwd: Path = REPO_ROOT,
        script: Path = SCRIPT,
    ) -> subprocess.CompletedProcess[str]:
        merged_env = os.environ.copy()
        merged_env.update(env)
        return subprocess.run(
            [BASH, bash_path(script), "--probe"],
            cwd=cwd,
            env=merged_env,
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
        )

    def test_uses_binary_overrides_without_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            log = root / "calls.log"
            bin_dir = root / "bin"
            bin_dir.mkdir()
            cli = bin_dir / "codex"
            tui = bin_dir / "codex-tui"

            write_executable(
                cli,
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "codex $*" >> {bash_path(log)}
                    printf 'ws://127.0.0.1:4567\\n'
                    sleep 2
                    """
                ),
            )
            write_executable(
                tui,
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "codex-tui CODEX_EXEC_SERVER_URL=$CODEX_EXEC_SERVER_URL $*" >> {bash_path(log)}
                    """
                ),
            )
            write_executable(
                bin_dir / "cargo",
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "cargo $*" >> {bash_path(log)}
                    exit 99
                    """
                ),
            )

            result = self.run_script(
                {
                    "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                    "CODEX_CLI_BIN": bash_path(cli),
                    "CODEX_TUI_BIN": bash_path(tui),
                    "CODEX_EXEC_SERVER_START_TIMEOUT_SECONDS": "2",
                }
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text(encoding="utf-8")
            self.assertIn("codex exec-server --listen", calls)
            self.assertIn("codex-tui CODEX_EXEC_SERVER_URL=ws://127.0.0.1:4567", calls)
            self.assertNotIn("cargo", calls)

    def test_reuses_existing_exec_server_url(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            log = root / "calls.log"
            tui = root / "codex-tui"
            bin_dir = root / "bin"
            bin_dir.mkdir()

            write_executable(
                tui,
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "tui $CODEX_EXEC_SERVER_URL $*" >> {bash_path(log)}
                    """
                ),
            )
            for name in ("cargo", "codex"):
                write_executable(
                    bin_dir / name,
                    textwrap.dedent(
                        f"""\
                        #!/usr/bin/env bash
                        echo "{name} $*" >> {bash_path(log)}
                        exit 97
                        """
                    ),
                )

            result = self.run_script(
                {
                    "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                    "CODEX_TUI_BIN": bash_path(tui),
                    "CODEX_EXEC_SERVER_URL": "ws://127.0.0.1:9999",
                }
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text(encoding="utf-8")
            self.assertEqual(calls.count("tui ws://127.0.0.1:9999"), 1)
            self.assertNotIn("cargo", calls)
            self.assertNotIn("codex ", calls)

    def test_reuses_ready_file_url_without_starting_server(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            log = root / "calls.log"
            ready = root / "ready.url"
            listener = socket.socket()
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            self.addCleanup(listener.close)
            _, port = listener.getsockname()
            handshake_thread = serve_websocket_handshake(listener)
            self.addCleanup(handshake_thread.join, 2)
            ready.write_text(f"ws://127.0.0.1:{port}", encoding="utf-8")
            tui = root / "codex-tui"
            bin_dir = root / "bin"
            bin_dir.mkdir()

            write_executable(
                tui,
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "tui $CODEX_EXEC_SERVER_URL $*" >> {bash_path(log)}
                    """
                ),
            )
            for name in ("cargo", "codex"):
                write_executable(
                    bin_dir / name,
                    textwrap.dedent(
                        f"""\
                        #!/usr/bin/env bash
                        echo "{name} $*" >> {bash_path(log)}
                        exit 96
                        """
                    ),
                )

            result = self.run_script(
                {
                    "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                    "CODEX_TUI_BIN": bash_path(tui),
                    "CODEX_EXEC_SERVER_READY_FILE": bash_path(ready),
                }
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text(encoding="utf-8")
            self.assertNotIn("cargo", calls)

            self.assertNotIn("codex ", calls)
            self.assertEqual(calls.count(f"tui ws://127.0.0.1:{port}"), 1)

    def test_stale_ready_file_starts_new_exec_server(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            log = root / "calls.log"
            ready = root / "ready.url"
            listener = socket.socket()
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            self.addCleanup(listener.close)
            _, port = listener.getsockname()
            http_thread = serve_plain_http(listener)
            self.addCleanup(http_thread.join, 2)
            ready.write_text(f"ws://127.0.0.1:{port}\n", encoding="utf-8")
            bin_dir = root / "bin"
            bin_dir.mkdir()
            cli = bin_dir / "codex"
            tui = bin_dir / "codex-tui"

            write_executable(
                cli,
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "codex $*" >> {bash_path(log)}
                    printf 'ws://127.0.0.1:4567\\n'
                    sleep 5
                    """
                ),
            )
            write_executable(
                tui,
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "tui $CODEX_EXEC_SERVER_URL $*" >> {bash_path(log)}
                    """
                ),
            )
            write_executable(
                bin_dir / "cargo",
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "cargo $*" >> {bash_path(log)}
                    exit 95
                    """
                ),
            )

            result = self.run_script(
                {
                    "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                    "CODEX_CLI_BIN": bash_path(cli),
                    "CODEX_TUI_BIN": bash_path(tui),
                    "CODEX_EXEC_SERVER_READY_FILE": bash_path(ready),
                    "CODEX_EXEC_SERVER_START_TIMEOUT_SECONDS": "2",
                }
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text(encoding="utf-8")
            self.assertIn("codex exec-server --listen", calls)
            self.assertIn("tui ws://127.0.0.1:4567", calls)
            self.assertEqual(
                ready.read_text(encoding="utf-8").strip(), "ws://127.0.0.1:4567"
            )

    def test_disabled_build_does_not_invoke_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            log = root / "calls.log"
            bin_dir = root / "bin"
            bin_dir.mkdir()
            write_executable(
                bin_dir / "cargo",
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "cargo $*" >> {bash_path(log)}
                    exit 95
                    """
                ),
            )

            result = self.run_script(
                {
                    "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                    "CODEX_CLI_BIN": bash_path(root / "missing-codex"),
                    "CODEX_TUI_BIN": bash_path(root / "missing-codex-tui"),
                    "CODEX_BUILD_MISSING_BINARIES": "0",
                    "CODEX_EXEC_SERVER_START_TIMEOUT_SECONDS": "2",
                }
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("CODEX_BUILD_MISSING_BINARIES=0", result.stderr)
            self.assertFalse(
                log.exists(), "cargo must not run when builds are disabled"
            )

    def test_cargo_run_fallback_uses_manifest_and_preserves_launch_cwd(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            launch_dir = root / "launch"
            launch_dir.mkdir()
            log = root / "calls.log"
            bin_dir = root / "bin"
            bin_dir.mkdir()
            write_executable(
                bin_dir / "cargo",
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env bash
                    echo "$PWD|cargo $*" >> {bash_path(log)}
                    if [[ "$*" == *"codex-cli"* ]]; then
                      printf 'ws://127.0.0.1:7654\\n'
                      sleep 2
                    fi
                    """
                ),
            )

            result = self.run_script(
                {
                    "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                    "CODEX_CLI_BIN": bash_path(root / "missing-codex"),
                    "CODEX_TUI_BIN": bash_path(root / "missing-codex-tui"),
                    "CODEX_EXEC_SERVER_START_TIMEOUT_SECONDS": "2",
                },
                cwd=launch_dir,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(calls), 2)
            working_directories = [line.split("|", 1)[0] for line in calls]
            self.assertEqual(working_directories[0], working_directories[1])
            self.assertTrue(
                working_directories[0].replace("\\", "/").endswith("/launch")
            )
            self.assertFalse(
                working_directories[0].replace("\\", "/").endswith("/codex-rs")
            )
            self.assertTrue(
                all(
                    "--manifest-path" in line
                    and "/codex-rs/Cargo.toml" in line.replace("\\", "/")
                    for line in calls
                )
            )

    def test_binary_discovery_respects_requested_profile(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_repo = root / "repo"
            scripts_dir = fake_repo / "scripts"
            scripts_dir.mkdir(parents=True)
            copied_script = scripts_dir / SCRIPT.name
            shutil.copyfile(SCRIPT, copied_script)
            copied_script.chmod(copied_script.stat().st_mode | stat.S_IXUSR)
            log = root / "calls.log"
            for profile in ("debug", "release"):
                target = fake_repo / "codex-rs" / "target" / profile
                target.mkdir(parents=True)
                write_executable(
                    target / "codex",
                    textwrap.dedent(
                        f"""\
                        #!/usr/bin/env bash
                        echo "{profile} cli" >> {bash_path(log)}
                        printf 'ws://127.0.0.1:4567\\n'
                        sleep 2
                        """
                    ),
                )
                write_executable(
                    target / "codex-tui",
                    textwrap.dedent(
                        f"""\
                        #!/usr/bin/env bash
                        echo "{profile} tui" >> {bash_path(log)}
                        """
                    ),
                )

            result = self.run_script(
                {
                    "CODEX_BUILD_PROFILE": "debug",
                    "CODEX_EXEC_SERVER_START_TIMEOUT_SECONDS": "2",
                },
                script=copied_script,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text(encoding="utf-8")
            self.assertIn("debug cli", calls)
            self.assertIn("debug tui", calls)
            self.assertNotIn("release", calls)

    def test_failure_logs_are_capped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cli = root / "codex"
            tui = root / "codex-tui"

            write_executable(
                cli,
                textwrap.dedent(
                    """\
                    #!/usr/bin/env bash
                    for i in $(seq 1 300); do echo "server stderr line $i" >&2; done
                    exit 42
                    """
                ),
            )
            write_executable(tui, "#!/usr/bin/env bash\nexit 0\n")

            result = self.run_script(
                {
                    "CODEX_CLI_BIN": bash_path(cli),
                    "CODEX_TUI_BIN": bash_path(tui),
                    "CODEX_EXEC_SERVER_START_TIMEOUT_SECONDS": "2",
                    "CODEX_EXEC_SERVER_LOG_MAX_LINES": "20",
                }
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("output truncated", result.stderr)
            self.assertLess(result.stderr.count("server stderr line"), 40)

    def test_script_uses_process_group_cleanup_and_no_polling_files(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")

        self.assertIn("setsid", text)
        self.assertIn('kill -- "-$server_pid"', text)
        self.assertIn("read -r -t", text)
        self.assertNotIn("seq 1", text)
        self.assertNotIn("head -n 1", text)


if __name__ == "__main__":
    unittest.main()
