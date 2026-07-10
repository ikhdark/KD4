#!/usr/bin/env python3

import os
import shutil
import socket
import stat
import subprocess
import tempfile
import textwrap
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


@unittest.skipUnless(bash_available(), "bash is required for shell launcher tests")
class RunTuiWithExecServerTest(unittest.TestCase):
    def run_script(self, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
        merged_env = os.environ.copy()
        merged_env.update(env)
        return subprocess.run(
            [BASH, bash_path(SCRIPT), "--probe"],
            cwd=REPO_ROOT,
            env=merged_env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
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
            ready.write_text(f"ws://127.0.0.1:{port}\n", encoding="utf-8")
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
            ready.write_text("ws://127.0.0.1:9\n", encoding="utf-8")
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

    def test_disabled_build_uses_quiet_cargo_run_fallback(self) -> None:
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
                    if [[ "$*" == build* ]]; then
                      exit 95
                    fi
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
                    "CODEX_BUILD_MISSING_BINARIES": "0",
                    "CODEX_EXEC_SERVER_START_TIMEOUT_SECONDS": "2",
                }
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text(encoding="utf-8")
            self.assertIn("cargo run --quiet -p codex-cli --bin codex", calls)
            self.assertIn("cargo run --quiet -p codex-tui --bin codex-tui", calls)
            self.assertNotIn("cargo build", calls)

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
