#!/usr/bin/env python3
"""Structured smoke tests for the Windows sandbox.

``--list-json`` inventories immutable cases without resolving Codex or touching
the filesystem. Repeatable ``--run-case`` arguments run the exact listed cases
beneath a caller-provided attempt root and write one structured report. With
neither option, the script retains its human-oriented full-suite entry point.
"""

import argparse
import contextlib
import hashlib
import http.client
import http.server
import json
import os
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlsplit

TIMEOUT_SEC = 20
BUILD_TIMEOUT_SEC = 3600
CASE_ID_PREFIX = "python-script-case::windows-sandbox-smoke::"
LIST_REPORT_TYPE = "WindowsSandboxSmokeCaseListV1"
CASE_REPORT_TYPE = "WindowsSandboxSmokeCaseReportV1"
SCHEMA_VERSION = 1


class PreResultError(RuntimeError):
    """The selected case could not produce a confirmed validation result."""


@dataclass(frozen=True)
class CaseAssertion:
    ok: bool
    detail: str = ""


@dataclass(frozen=True)
class CaseResult:
    case_id: str
    name: str
    status: str
    detail: str
    sandbox_launches: int

    @property
    def ok(self) -> bool:
        return self.status == "passed"


@dataclass
class SmokeContext:
    codex_cmd: list[str]
    attempt_root: Path
    run_root: Path
    workspace: Path
    outside: Path
    additional_root: Path
    temp_root: Path
    codex_home: Path
    child_runner: Path
    verbose: bool = False
    sandbox_launches: int = 0


@dataclass(frozen=True)
class CaseSpec:
    case_id: str
    name: str
    runner: Callable[[SmokeContext], CaseAssertion]


_CASES: list[CaseSpec] = []


def smoke_case(slug: str, name: str):
    """Register one immutable smoke-case identity."""

    def decorate(runner: Callable[[SmokeContext], CaseAssertion]):
        _CASES.append(CaseSpec(f"{CASE_ID_PREFIX}{slug}", name, runner))
        return runner

    return decorate


def _registry_errors() -> list[str]:
    seen: dict[str, int] = {}
    for spec in _CASES:
        seen[spec.case_id] = seen.get(spec.case_id, 0) + 1
    return [case_id for case_id, count in seen.items() if count != 1]


def _resolve_codex_cmd(explicit_binary: Path | None = None) -> list[str]:
    """Resolve the exact Codex CLI used by a case execution."""
    if explicit_binary is not None:
        candidate = explicit_binary.expanduser().resolve()
        if not candidate.is_file():
            raise PreResultError(f"Codex binary does not exist: {candidate}")
        return [str(candidate)]

    root = Path(__file__).parent
    workspace_root = root.parent
    cargo_target = os.environ.get("CARGO_TARGET_DIR")
    candidates = [
        workspace_root / "target" / "debug" / "codex.exe",
        workspace_root / "target" / "release" / "codex.exe",
    ]
    if cargo_target:
        cargo_base = Path(cargo_target)
        candidates.extend(
            [
                cargo_base / "debug" / "codex.exe",
                cargo_base / "release" / "codex.exe",
            ]
        )
    for candidate in candidates:
        if candidate.is_file():
            return [str(candidate.resolve())]
    codex = shutil.which("codex")
    if codex:
        return [str(Path(codex).resolve())]
    raise PreResultError(
        "Codex CLI not found. Build it with `cargo build -p codex-cli` first."
    )


def _current_fork_codex_binary() -> Path:
    workspace_root = Path(__file__).resolve().parent.parent
    cargo_target = os.environ.get("CARGO_TARGET_DIR")
    target_root = Path(cargo_target) if cargo_target else workspace_root / "target"
    if not target_root.is_absolute():
        target_root = workspace_root / target_root
    executable = "codex.exe" if os.name == "nt" else "codex"
    return (target_root / "debug" / executable).resolve()


def _build_current_fork_codex(explicit_binary: Path | None) -> dict[str, object]:
    if explicit_binary is None:
        raise PreResultError(
            "--build-current-codex requires an explicit --codex-bin path"
        )
    requested = explicit_binary.expanduser().resolve()
    expected = _current_fork_codex_binary()
    if os.path.normcase(str(requested)) != os.path.normcase(str(expected)):
        raise PreResultError(
            "current-fork Codex binary path did not match the active Cargo target: "
            f"requested={requested}, expected={expected}"
        )
    cargo = shutil.which("cargo")
    if not cargo:
        raise PreResultError("required command is unavailable: cargo")
    command = [cargo, "build", "--locked", "-p", "codex-cli", "--bin", "codex"]
    workspace_root = Path(__file__).resolve().parent.parent
    try:
        completed = subprocess.run(
            command,
            cwd=str(workspace_root),
            capture_output=True,
            text=True,
            timeout=BUILD_TIMEOUT_SEC,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise PreResultError(f"current-fork Codex build could not run: {exc}") from exc
    if completed.returncode != 0:
        raise PreResultError(
            "current-fork Codex build failed before validation: "
            f"rc={completed.returncode}, stderr={completed.stderr}"
        )
    if not expected.is_file():
        raise PreResultError(
            f"current-fork Codex build produced no executable at {expected}"
        )
    return {
        "command": command,
        "cwd": str(workspace_root),
        "exit_code": completed.returncode,
    }


def _capture_codex_identity(codex_cmd: Sequence[str]) -> dict[str, str]:
    if len(codex_cmd) != 1:
        raise PreResultError("Codex executable identity was not a single exact path")
    resolved = Path(codex_cmd[0]).expanduser().resolve()
    if not resolved.is_file():
        raise PreResultError(f"Codex binary does not exist: {resolved}")
    digest = hashlib.sha256()
    try:
        with resolved.open("rb") as executable:
            for chunk in iter(lambda: executable.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as exc:
        raise PreResultError(f"could not hash Codex binary {resolved}: {exc}") from exc
    value = digest.hexdigest()
    return {
        "requested": codex_cmd[0],
        "resolved_path": str(resolved),
        "sha256_before": value,
        "sha256_after": value,
    }


def _finish_codex_identity(identity: dict[str, str]) -> str | None:
    try:
        current = _capture_codex_identity([identity["resolved_path"]])
    except PreResultError as exc:
        identity["sha256_after"] = ""
        return str(exc)
    identity["sha256_after"] = current["sha256_after"]
    if identity["sha256_before"] != identity["sha256_after"]:
        return "Codex executable identity changed during Windows sandbox validation"
    return None


def _prepare_context(
    attempt_root: Path, codex_cmd: list[str], *, verbose: bool
) -> SmokeContext:
    attempt_root = attempt_root.expanduser().resolve()
    attempt_root.mkdir(parents=True, exist_ok=True)
    if not attempt_root.is_dir():
        raise PreResultError(f"attempt root is not a directory: {attempt_root}")
    run_root = attempt_root / "windows-sandbox-smoke"
    try:
        run_root.mkdir()
    except FileExistsError as exc:
        raise PreResultError(
            f"attempt root was already used by this runner: {run_root}"
        ) from exc
    workspace = run_root / "workspace"
    outside = run_root / "outside"
    additional_root = run_root / "additional-root"
    temp_root = run_root / "temp"
    codex_home = run_root / "codex-home"
    for path in (workspace, outside, additional_root, temp_root, codex_home):
        path.mkdir()
    child_runner = run_root / "confirmed_child_runner.py"
    child_runner.write_text(
        """\
import subprocess
import sys

marker = sys.argv[1]
command = sys.argv[2:]
if not command:
    raise SystemExit(97)
try:
    child = subprocess.Popen(command)
except OSError as exc:
    print(f"child invocation failed: {exc}", file=sys.stderr, flush=True)
    raise SystemExit(98)
print(marker, flush=True)
raise SystemExit(child.wait())
""",
        encoding="utf-8",
    )
    return SmokeContext(
        codex_cmd=codex_cmd,
        attempt_root=attempt_root,
        run_root=run_root,
        workspace=workspace,
        outside=outside,
        additional_root=additional_root,
        temp_root=temp_root,
        codex_home=codex_home,
        child_runner=child_runner,
        verbose=verbose,
    )


def run_sbx(
    ctx: SmokeContext,
    policy: str,
    cmd_argv: Sequence[str],
    cwd: Path | None = None,
    env_extra: dict[str, str] | None = None,
    additional_root: Path | None = None,
    timeout_sec: int = TIMEOUT_SEC,
) -> tuple[int, str, str]:
    """Launch one real ``codex sandbox windows`` child action."""
    if policy not in ("read-only", "workspace-write"):
        raise ValueError(f"unknown policy: {policy}")
    if not cmd_argv:
        raise PreResultError("sandbox child selection was empty")
    env = os.environ.copy()
    env.update(
        {
            "CODEX_HOME": str(ctx.codex_home),
            "TEMP": str(ctx.temp_root),
            "TMP": str(ctx.temp_root),
        }
    )
    if env_extra:
        env.update(env_extra)
    policy_flags: list[str] = (
        ["-c", 'sandbox_mode="workspace-write"'] if policy == "workspace-write" else []
    )
    overrides: list[str] = []
    if policy == "workspace-write" and additional_root is not None:
        overrides = [
            "-c",
            (
                "sandbox_workspace_write.writable_roots="
                f'["{additional_root.resolve().as_posix()}"]'
            ),
        ]
    execution_marker = f"CODEX_SANDBOX_SMOKE_STARTED_{secrets.token_hex(16)}"
    wrapped_command = [
        sys.executable,
        str(ctx.child_runner),
        execution_marker,
        *cmd_argv,
    ]
    argv = [
        *ctx.codex_cmd,
        "sandbox",
        "windows",
        *policy_flags,
        *overrides,
        "--",
        *wrapped_command,
    ]
    if ctx.verbose:
        print(f"launch: {cmd_argv}")
    try:
        completed = subprocess.run(
            argv,
            cwd=str(cwd or ctx.workspace),
            env=env,
            capture_output=True,
            timeout=timeout_sec,
            text=True,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        stdout = exc.stdout or ""
        if isinstance(stdout, bytes):
            stdout = stdout.decode("utf-8", errors="replace")
        if execution_marker not in stdout:
            raise PreResultError(
                "sandbox runner timed out before the intended child was confirmed"
            ) from exc
        ctx.sandbox_launches += 1
        raise
    if execution_marker not in completed.stdout:
        raise PreResultError(
            "sandbox runner returned before the intended child was confirmed: "
            f"rc={completed.returncode}, stderr={completed.stderr}"
        )
    ctx.sandbox_launches += 1
    stdout = completed.stdout.replace(execution_marker + "\n", "", 1)
    return completed.returncode, stdout, completed.stderr


def _require_command(command: str) -> str:
    resolved = shutil.which(command)
    if not resolved:
        raise PreResultError(f"required command is unavailable: {command}")
    return resolved


def _remove_if_exists(path: Path) -> None:
    try:
        if path.is_symlink():
            path.unlink()
        elif path.is_dir():
            shutil.rmtree(path)
        elif path.exists():
            path.unlink()
    except OSError as exc:
        raise PreResultError(f"could not remove test path {path}: {exc}") from exc


def _write_file(path: Path, content: str = "x") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _make_junction(link: Path, target: Path) -> bool:
    _require_command("cmd")
    _remove_if_exists(link)
    link.parent.mkdir(parents=True, exist_ok=True)
    if not target.is_dir():
        return False
    completed = subprocess.run(
        ["cmd", "/c", f'mklink /J "{link}" "{target}"'],
        capture_output=True,
        text=True,
        check=False,
    )
    return completed.returncode == 0 and link.exists()


def _make_symlink(link: Path, target: Path) -> bool:
    _require_command("cmd")
    _remove_if_exists(link)
    link.parent.mkdir(parents=True, exist_ok=True)
    if not target.exists():
        return False
    completed = subprocess.run(
        ["cmd", "/c", f'mklink /D "{link}" "{target}"'],
        capture_output=True,
        text=True,
        check=False,
    )
    return completed.returncode == 0 and link.exists()


class _QuietHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        del format, args


class _TargetHandler(_QuietHandler):
    def do_GET(self):
        self.server.request_count += 1  # type: ignore[attr-defined]
        body = b"proxy-ok"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class _ProxyHandler(_QuietHandler):
    def do_GET(self):
        parsed = urlsplit(self.path)
        if not parsed.scheme or not parsed.hostname:
            self.send_error(400, "absolute URL required")
            return
        if parsed.hostname not in ("127.0.0.1", "localhost"):
            self.send_error(403, "only loopback hosts are allowed in smoke proxy")
            return
        path = parsed.path or "/"
        if parsed.query:
            path = f"{path}?{parsed.query}"
        connection = None
        try:
            connection = http.client.HTTPConnection(
                parsed.hostname, parsed.port or 80, timeout=2
            )
            connection.request("GET", path)
            upstream = connection.getresponse()
            body = upstream.read()
        except (OSError, http.client.HTTPException) as exc:
            self.send_error(502, f"proxy upstream error: {exc}")
            return
        finally:
            if connection is not None:
                with contextlib.suppress(Exception):
                    connection.close()
        self.send_response(upstream.status, upstream.reason)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@contextlib.contextmanager
def _loopback_target_fixture():
    target = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _TargetHandler)
    target.request_count = 0  # type: ignore[attr-defined]
    target_thread = threading.Thread(target=target.serve_forever, daemon=True)
    target_thread.start()
    try:
        yield target
    finally:
        target.shutdown()
        target.server_close()
        target_thread.join(timeout=2)


@contextlib.contextmanager
def _loopback_proxy_fixture():
    target = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _TargetHandler)
    target.request_count = 0  # type: ignore[attr-defined]
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _ProxyHandler)
    target_thread = threading.Thread(target=target.serve_forever, daemon=True)
    proxy_thread = threading.Thread(target=proxy.serve_forever, daemon=True)
    target_thread.start()
    proxy_thread.start()
    try:
        yield target, proxy
    finally:
        proxy.shutdown()
        target.shutdown()
        proxy.server_close()
        target.server_close()
        proxy_thread.join(timeout=2)
        target_thread.join(timeout=2)


def _result(ok: bool, rc: int, stdout: str = "", stderr: str = "") -> CaseAssertion:
    return CaseAssertion(ok, f"rc={rc}, stdout={stdout}, stderr={stderr}")


def _cmd() -> None:
    _require_command("cmd")


def _powershell() -> None:
    _require_command("powershell")


@smoke_case("read-only-write-cwd-denied", "RO: write in CWD denied")
def _read_only_write_cwd_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "ro_should_fail.txt"
    rc, out, err = run_sbx(
        ctx, "read-only", ["cmd", "/c", "echo nope > ro_should_fail.txt"]
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-cwd-allowed", "WS: write in CWD allowed")
def _workspace_write_cwd_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "ws_ok.txt"
    rc, out, err = run_sbx(ctx, "workspace-write", ["cmd", "/c", "echo ok > ws_ok.txt"])
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case("workspace-write-outside-denied", "WS: write outside workspace denied")
def _workspace_write_outside_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.outside / "blocked.txt"
    rc, out, err = run_sbx(
        ctx, "workspace-write", ["cmd", "/c", f'echo nope > "{target}"']
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-additional-root-allowed", "WS: write in additional root allowed"
)
def _workspace_write_additional_root_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.additional_root / "extra_ok.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", f'echo extra > "{target}"'],
        additional_root=ctx.additional_root,
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case(
    "read-only-write-additional-root-denied", "RO: write in additional root denied"
)
def _read_only_write_additional_root_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.additional_root / "extra_ro.txt"
    rc, out, err = run_sbx(
        ctx,
        "read-only",
        ["cmd", "/c", f'echo nope > "{target}"'],
        additional_root=ctx.additional_root,
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-temp-allowed", "WS: TEMP write allowed")
def _workspace_write_temp_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.temp_root / "ws_temp_ok.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo tempok > %TEMP%\\ws_temp_ok.txt"],
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case("read-only-write-temp-cmd-denied", "RO: TEMP write denied (cmd)")
def _read_only_write_temp_cmd_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.temp_root / "ro_temp_fail.txt"
    rc, out, err = run_sbx(
        ctx,
        "read-only",
        ["cmd", "/c", "echo tempno > %TEMP%\\ro_temp_fail.txt"],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-append-allowed", "WS: append allowed")
def _workspace_write_append_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "append.txt"
    _write_file(target, "line1\n")
    rc, out, err = run_sbx(
        ctx, "workspace-write", ["cmd", "/c", "echo line2 >> append.txt"]
    )
    return _result(
        rc == 0 and target.read_text(encoding="utf-8").strip().endswith("line2"),
        rc,
        out,
        err,
    )


@smoke_case("read-only-append-denied", "RO: append denied")
def _read_only_append_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "ro_append.txt"
    _write_file(target, "line1\n")
    rc, out, err = run_sbx(
        ctx, "read-only", ["cmd", "/c", "echo line2 >> ro_append.txt"]
    )
    return _result(
        rc != 0 and target.read_text(encoding="utf-8") == "line1\n", rc, out, err
    )


@smoke_case(
    "workspace-write-powershell-set-content-allowed",
    "WS: PowerShell Set-Content allowed",
)
def _workspace_write_powershell_set_content_allowed(
    ctx: SmokeContext,
) -> CaseAssertion:
    _powershell()
    target = ctx.workspace / "ps_ok.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        [
            "powershell",
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "Set-Content -LiteralPath ps_ok.txt -Value 'hello' -Encoding ASCII",
        ],
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case(
    "read-only-powershell-set-content-denied", "RO: PowerShell Set-Content denied"
)
def _read_only_powershell_set_content_denied(ctx: SmokeContext) -> CaseAssertion:
    _powershell()
    target = ctx.workspace / "ps_ro_fail.txt"
    rc, out, err = run_sbx(
        ctx,
        "read-only",
        [
            "powershell",
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "Set-Content -LiteralPath ps_ro_fail.txt -Value 'x'",
        ],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-mkdir-write-allowed", "WS: mkdir and write allowed")
def _workspace_write_mkdir_write_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "sub" / "in_sub.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "mkdir sub && echo hi > sub\\in_sub.txt"],
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case("workspace-write-rename-allowed", "WS: rename allowed")
def _workspace_write_rename_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "r2.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo x > r.txt & ren r.txt r2.txt"],
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case("workspace-write-delete-allowed", "WS: delete allowed")
def _workspace_write_delete_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "delme.txt"
    _write_file(target)
    rc, out, err = run_sbx(ctx, "workspace-write", ["cmd", "/c", "del /q delme.txt"])
    return _result(rc == 0 and not target.exists(), rc, out, err)


@smoke_case("read-only-python-write-denied", "RO: Python write denied")
def _read_only_python_write_denied(ctx: SmokeContext) -> CaseAssertion:
    target = ctx.workspace / "py_should_fail.txt"
    rc, out, err = run_sbx(
        ctx,
        "read-only",
        [sys.executable, "-c", "open('py_should_fail.txt','w').write('x')"],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-python-write-allowed", "WS: Python write allowed")
def _workspace_write_python_write_allowed(ctx: SmokeContext) -> CaseAssertion:
    target = ctx.workspace / "py_ok.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        [sys.executable, "-c", "open('py_ok.txt','w').write('x')"],
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case("workspace-write-outbound-curl-denied", "WS: curl network denied")
def _workspace_write_outbound_curl_denied(ctx: SmokeContext) -> CaseAssertion:
    _require_command("curl")
    with _loopback_target_fixture() as target:
        port = target.server_address[1]
        rc, out, err = run_sbx(
            ctx,
            "workspace-write",
            [
                "curl",
                "--noproxy",
                "*",
                "--connect-timeout",
                "1",
                "--max-time",
                "2",
                f"http://127.0.0.1:{port}/blocked-curl",
            ],
            env_extra={"NO_PROXY": "*", "no_proxy": "*"},
        )
        return _result(
            rc != 0 and target.request_count == 0,  # type: ignore[attr-defined]
            rc,
            out,
            err,
        )


@smoke_case("workspace-write-outbound-iwr-denied", "WS: iwr network denied")
def _workspace_write_outbound_iwr_denied(ctx: SmokeContext) -> CaseAssertion:
    _powershell()
    with _loopback_target_fixture() as target:
        port = target.server_address[1]
        command = (
            "try { Invoke-WebRequest -UseBasicParsing "
            f"'http://127.0.0.1:{port}/blocked-iwr' -TimeoutSec 2; exit 0 "
            "} catch { exit 1 }"
        )
        rc, out, err = run_sbx(
            ctx,
            "workspace-write",
            ["powershell", "-NoLogo", "-NoProfile", "-Command", command],
            env_extra={"NO_PROXY": "*", "no_proxy": "*"},
        )
        return _result(
            rc != 0 and target.request_count == 0,  # type: ignore[attr-defined]
            rc,
            out,
            err,
        )


@smoke_case("workspace-write-loopback-proxy-allowed", "WS: loopback proxy allowed")
def _workspace_write_loopback_proxy_allowed(ctx: SmokeContext) -> CaseAssertion:
    _require_command("curl")
    with _loopback_proxy_fixture() as (target, proxy):
        target_port = target.server_address[1]
        proxy_port = proxy.server_address[1]
        proxy_url = f"http://127.0.0.1:{proxy_port}"
        rc, out, err = run_sbx(
            ctx,
            "workspace-write",
            [
                "curl",
                "--noproxy",
                "",
                "--connect-timeout",
                "2",
                "--max-time",
                "4",
                f"http://127.0.0.1:{target_port}/proxied",
            ],
            env_extra={
                "HTTP_PROXY": proxy_url,
                "http_proxy": proxy_url,
                "ALL_PROXY": proxy_url,
                "all_proxy": proxy_url,
                "NO_PROXY": "",
                "no_proxy": "",
            },
        )
        return _result(
            rc == 0 and "proxy-ok" in out and target.request_count > 0,  # type: ignore[attr-defined]
            rc,
            out,
            err,
        )


@smoke_case("workspace-write-direct-loopback-denied", "WS: direct loopback denied")
def _workspace_write_direct_loopback_denied(ctx: SmokeContext) -> CaseAssertion:
    _require_command("curl")
    with _loopback_target_fixture() as target:
        port = target.server_address[1]
        rc, out, err = run_sbx(
            ctx,
            "workspace-write",
            [
                "curl",
                "--noproxy",
                "*",
                "--connect-timeout",
                "1",
                "--max-time",
                "2",
                f"http://127.0.0.1:{port}/direct",
            ],
            env_extra={"NO_PROXY": "*", "no_proxy": "*"},
        )
        return _result(
            rc != 0 and target.request_count == 0,  # type: ignore[attr-defined]
            rc,
            out,
            err,
        )


@smoke_case(
    "read-only-write-temp-powershell-denied",
    "RO: TEMP write denied (PowerShell)",
)
def _read_only_write_temp_powershell_denied(ctx: SmokeContext) -> CaseAssertion:
    _powershell()
    target = ctx.temp_root / "ro_tmpfail.txt"
    rc, out, err = run_sbx(
        ctx,
        "read-only",
        [
            "powershell",
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "Set-Content -LiteralPath $env:TEMP\\ro_tmpfail.txt -Value 'x'",
        ],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-curl-version", "WS: curl --version")
def _workspace_write_curl_version(ctx: SmokeContext) -> CaseAssertion:
    _require_command("curl")
    rc, out, err = run_sbx(ctx, "workspace-write", ["curl", "--version"])
    return _result(rc == 0, rc, out, err)


@smoke_case("workspace-write-ripgrep-version", "WS: rg --version")
def _workspace_write_ripgrep_version(ctx: SmokeContext) -> CaseAssertion:
    _require_command("rg")
    rc, out, err = run_sbx(ctx, "workspace-write", ["rg", "--version"])
    return _result(rc == 0, rc, out, err)


@smoke_case("workspace-write-git-version", "WS: git --version")
def _workspace_write_git_version(ctx: SmokeContext) -> CaseAssertion:
    _require_command("git")
    rc, out, err = run_sbx(ctx, "workspace-write", ["git", "--version"])
    return _result(rc == 0, rc, out, err)


@smoke_case(
    "workspace-write-powershell-bytes-allowed", "WS: PowerShell bytes write allowed"
)
def _workspace_write_powershell_bytes_allowed(ctx: SmokeContext) -> CaseAssertion:
    _powershell()
    target = ctx.workspace / "bytes_ok.bin"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        [
            "powershell",
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "[IO.File]::WriteAllBytes('bytes_ok.bin',[byte[]](0..255))",
        ],
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case("read-only-powershell-bytes-denied", "RO: PowerShell bytes write denied")
def _read_only_powershell_bytes_denied(ctx: SmokeContext) -> CaseAssertion:
    _powershell()
    target = ctx.workspace / "bytes_fail.bin"
    rc, out, err = run_sbx(
        ctx,
        "read-only",
        [
            "powershell",
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "[IO.File]::WriteAllBytes('bytes_fail.bin',[byte[]](0..10))",
        ],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-deep-mkdir-write-allowed", "WS: deep mkdir and write allowed"
)
def _workspace_write_deep_mkdir_write_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "deep" / "nest" / "f.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "mkdir deep\\nest && echo ok > deep\\nest\\f.txt"],
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case("workspace-write-move-allowed", "WS: move allowed")
def _workspace_write_move_allowed(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "m2.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo x > m1.txt & move /y m1.txt m2.txt"],
    )
    return _result(rc == 0 and target.exists(), rc, out, err)


@smoke_case("read-only-cmd-redirection-denied", "RO: cmd redirection denied")
def _read_only_cmd_redirection_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "cmd_ro.txt"
    rc, out, err = run_sbx(ctx, "read-only", ["cmd", "/c", "echo nope > cmd_ro.txt"])
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-junction-cwd-poisoning-denied",
    "WS: junction poisoning through CWD denied",
)
def _workspace_write_junction_cwd_poisoning_denied(
    ctx: SmokeContext,
) -> CaseAssertion:
    poison_cwd = ctx.workspace / "poison_cwd"
    if not _make_junction(poison_cwd, ctx.outside):
        raise PreResultError("junction setup failed")
    target = ctx.outside / "poisoned.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo poison > poisoned.txt"],
        cwd=poison_cwd,
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-junction-windows-denied", "WS: junction into Windows denied"
)
def _workspace_write_junction_windows_denied(ctx: SmokeContext) -> CaseAssertion:
    system_root = Path(os.environ.get("SystemRoot", "C:/Windows"))
    if not system_root.is_dir():
        raise PreResultError(f"Windows system root is unavailable: {system_root}")
    link = ctx.workspace / "sys_link"
    if not _make_junction(link, system_root):
        raise PreResultError("Windows junction setup failed")
    target = system_root / "System32" / "codex_sandbox_smoke_junction.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        [
            "cmd",
            "/c",
            "echo bad > sys_link\\System32\\codex_sandbox_smoke_junction.txt",
        ],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-raw-device-access-denied", "WS: raw device denied")
def _workspace_write_raw_device_access_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    rc, out, err = run_sbx(
        ctx, "workspace-write", ["cmd", "/c", "type \\\\.\\PhysicalDrive0"]
    )
    return _result(rc != 0, rc, out, err)


@smoke_case(
    "workspace-write-named-pipe-creation-denied", "WS: named pipe creation denied"
)
def _workspace_write_named_pipe_creation_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo hi > \\\\.\\pipe\\codex_testpipe"],
    )
    return _result(rc != 0, rc, out, err)


@smoke_case("workspace-write-ads-write-denied", "WS: ADS write denied")
def _workspace_write_ads_write_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / "ads_base.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo secret > ads_base.txt:stream"],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-long-path-escape-denied", "WS: long-path escape denied")
def _workspace_write_long_path_escape_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.outside / "longpath_escape.txt"
    extended = "\\\\?\\" + str(target)
    rc, out, err = run_sbx(
        ctx, "workspace-write", ["cmd", "/c", f'echo long > "{extended}"']
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-protected-path-case-variation-denied",
    "WS: protected path case variation denied",
)
def _workspace_write_protected_path_case_variation_denied(
    ctx: SmokeContext,
) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / ".GiT" / "config"
    target.parent.mkdir()
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo hack > .GiT\\config"],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-codex-cap-sid-tamper-denied", "WS: Codex cap_sid tamper denied"
)
def _workspace_write_codex_cap_sid_tamper_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.codex_home / "cap_sid"
    _write_file(target, "original\n")
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", f'echo tamper > "{target}"'],
    )
    unchanged = target.read_text(encoding="utf-8") == "original\n"
    return _result(rc != 0 and unchanged, rc, out, err)


@smoke_case(
    "workspace-write-codex-policy-tamper-denied", "WS: Codex policy tamper denied"
)
def _workspace_write_codex_policy_tamper_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target = ctx.workspace / ".codex" / "policy.json"
    _write_file(target, "original\n")
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo tamper > .codex\\policy.json"],
    )
    unchanged = target.read_text(encoding="utf-8") == "original\n"
    return _result(rc != 0 and unchanged, rc, out, err)


@smoke_case("workspace-write-path-stub-bypass-denied", "WS: PATH stub bypass denied")
def _workspace_write_path_stub_bypass_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    _require_command("ssh")
    tools_dir = ctx.workspace / "tools"
    tools_dir.mkdir()
    shim = tools_dir / "ssh.bat"
    shim.write_text("@echo off\r\necho stubbed\r\n", encoding="utf-8")
    path = f"{tools_dir}{os.pathsep}{os.environ.get('PATH', '')}"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "ssh"],
        env_extra={"PATH": path},
    )
    return _result("stubbed" not in out, rc, out, err)


@smoke_case("workspace-write-symlink-race-denied", "WS: symlink race denied")
def _workspace_write_symlink_race_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    race_root = ctx.workspace / "race"
    inside = race_root / "inside"
    inside.mkdir(parents=True)
    link = race_root / "flip"
    if not _make_symlink(link, inside):
        raise PreResultError("initial race symlink setup failed")
    target = ctx.outside / "race.txt"
    toggle = [
        "cmd",
        "/c",
        (
            "for /L %i in (1,1,400) do "
            f'(rmdir flip & mklink /D flip "{inside}" >NUL & '
            f'rmdir flip & mklink /D flip "{ctx.outside}" >NUL)'
        ),
    ]
    toggler = subprocess.Popen(
        toggle,
        cwd=str(race_root),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        rc, out, err = run_sbx(
            ctx,
            "workspace-write",
            ["cmd", "/c", "echo race > flip\\race.txt"],
            cwd=race_root,
        )
    finally:
        with contextlib.suppress(Exception):
            toggler.terminate()
        with contextlib.suppress(Exception):
            toggler.wait(timeout=2)
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-deep-junction-world-writable-escape-denied",
    "WS: deep junction/world-writable escape denied",
)
def _workspace_write_deep_junction_world_writable_escape_denied(
    ctx: SmokeContext,
) -> CaseAssertion:
    _cmd()
    _require_command("icacls")
    system_root = Path(os.environ.get("SystemRoot", "C:/Windows"))
    link = ctx.workspace / "deep" / "redir"
    if not _make_junction(link, system_root):
        raise PreResultError("deep junction setup failed")
    unsafe_dir = ctx.workspace / "deep" / "unsafe"
    unsafe_dir.mkdir(parents=True)
    acl = subprocess.run(
        ["icacls", str(unsafe_dir), "/grant", "Everyone:(F)"],
        capture_output=True,
        text=True,
        check=False,
    )
    if acl.returncode != 0:
        raise PreResultError(f"world-writable ACL setup failed: {acl.stderr}")
    target = system_root / "System32" / "codex_sandbox_smoke_audit_gap.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        [
            "cmd",
            "/c",
            "echo probe > deep\\redir\\System32\\codex_sandbox_smoke_audit_gap.txt",
        ],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-root-symlink-poisoning-denied",
    "WS: workspace-root symlink poisoning denied",
)
def _workspace_root_symlink_poisoning_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    drive_root = Path(ctx.workspace.anchor)
    if not drive_root.exists():
        raise PreResultError(f"workspace drive root is unavailable: {drive_root}")
    fake_root = ctx.workspace / "fake_root"
    if not _make_symlink(fake_root, drive_root):
        raise PreResultError("workspace-root symlink setup failed")
    target = drive_root / "codex_sandbox_smoke_escape.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo owned > codex_sandbox_smoke_escape.txt"],
        cwd=fake_root,
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("workspace-write-unc-link-escape-denied", "WS: UNC link escape denied")
def _workspace_write_unc_link_escape_denied(ctx: SmokeContext) -> CaseAssertion:
    _cmd()
    target_root = Path(r"\\localhost\C$")
    if not target_root.exists():
        raise PreResultError("localhost administrative share is unavailable")
    link = ctx.workspace / "unc_link"
    if not _make_symlink(link, target_root):
        raise PreResultError("UNC symlink setup failed")
    target = target_root / "codex_sandbox_smoke_unc.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo unc > unc_link\\codex_sandbox_smoke_unc.txt"],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-other-drive-link-escape-denied",
    "WS: other-drive link escape denied",
)
def _workspace_write_other_drive_link_escape_denied(
    ctx: SmokeContext,
) -> CaseAssertion:
    _cmd()
    current_drive = ctx.workspace.drive.upper()
    target_root = next(
        (
            Path(f"{letter}:/")
            for letter in "DEFGHIJKLMNOPQRSTUVWXYZ"
            if f"{letter}:" != current_drive and Path(f"{letter}:/").is_dir()
        ),
        None,
    )
    if target_root is None:
        raise PreResultError("no second local drive is available")
    link = ctx.workspace / "other_drive"
    if not _make_symlink(link, target_root):
        raise PreResultError("other-drive symlink setup failed")
    target = target_root / "codex_sandbox_smoke_drive.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", "echo drive > other_drive\\codex_sandbox_smoke_drive.txt"],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case(
    "workspace-write-post-timeout-outside-denied",
    "WS: post-timeout outside write denied",
)
def _workspace_write_post_timeout_outside_denied(ctx: SmokeContext) -> CaseAssertion:
    _powershell()
    _cmd()
    script = ctx.workspace / "sleep.ps1"
    script.write_text("Start-Sleep 15", encoding="utf-8")
    try:
        run_sbx(
            ctx,
            "workspace-write",
            ["powershell", "-NoLogo", "-NoProfile", "-File", "sleep.ps1"],
            timeout_sec=1,
        )
    except subprocess.TimeoutExpired:
        pass
    else:
        raise PreResultError("timeout setup did not time out")
    target = ctx.outside / "timeout_leak.txt"
    rc, out, err = run_sbx(
        ctx,
        "workspace-write",
        ["cmd", "/c", f'echo leak > "{target}"'],
    )
    return _result(rc != 0 and not target.exists(), rc, out, err)


@smoke_case("read-only-start-process-uri-denied", "RO: Start-Process URI denied")
def _read_only_start_process_uri_denied(ctx: SmokeContext) -> CaseAssertion:
    _powershell()
    with _loopback_target_fixture() as target:
        port = target.server_address[1]
        rc, out, err = run_sbx(
            ctx,
            "read-only",
            [
                "powershell",
                "-NoLogo",
                "-NoProfile",
                "-Command",
                f"Start-Process 'http://127.0.0.1:{port}/start-process'",
            ],
            env_extra={"NO_PROXY": "*", "no_proxy": "*"},
        )
        return _result(rc != 0, rc, out, err)


def _run_registered_case(ctx: SmokeContext, spec: CaseSpec) -> CaseResult:
    before = ctx.sandbox_launches
    try:
        assertion = spec.runner(ctx)
        launches = ctx.sandbox_launches - before
        if launches == 0:
            return CaseResult(
                spec.case_id,
                spec.name,
                "pre_result_error",
                "selected case launched no sandbox validation action",
                0,
            )
        status = "passed" if assertion.ok else "failed"
        return CaseResult(spec.case_id, spec.name, status, assertion.detail, launches)
    except PreResultError as exc:
        return CaseResult(
            spec.case_id,
            spec.name,
            "pre_result_error",
            str(exc),
            ctx.sandbox_launches - before,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return CaseResult(
            spec.case_id,
            spec.name,
            "pre_result_error",
            f"runner or infrastructure error: {exc}",
            ctx.sandbox_launches - before,
        )
    except Exception as exc:  # noqa: BLE001 - unknown outcomes fail closed
        return CaseResult(
            spec.case_id,
            spec.name,
            "pre_result_error",
            f"unclassified runner error: {type(exc).__name__}: {exc}",
            ctx.sandbox_launches - before,
        )


def _list_payload() -> dict[str, object]:
    return {
        "report_type": LIST_REPORT_TYPE,
        "schema_version": SCHEMA_VERSION,
        "validation_id": "windows-sandbox-smoke",
        "host_platform": "windows",
        "cases": [{"id": spec.case_id, "name": spec.name} for spec in _CASES],
    }


def _case_payload(
    requested_ids: Sequence[str],
    selected_specs: Sequence[CaseSpec],
    results: Sequence[CaseResult],
    attempt_root: Path | None,
    selection_error: str | None = None,
    codex_executable_identity: dict[str, str] | None = None,
    codex_build: dict[str, object] | None = None,
) -> dict[str, object]:
    executed_ids = [result.case_id for result in results if result.sandbox_launches > 0]
    statuses = {status: 0 for status in ("passed", "failed", "pre_result_error")}
    for result in results:
        statuses[result.status] += 1
    return {
        "report_type": CASE_REPORT_TYPE,
        "schema_version": SCHEMA_VERSION,
        "validation_id": "windows-sandbox-smoke",
        "host_platform": "windows",
        "attempt_root": str(attempt_root.resolve()) if attempt_root else None,
        "intended_case_ids": list(requested_ids),
        "selected_case_ids": [spec.case_id for spec in selected_specs],
        "executed_case_ids": executed_ids,
        "selection_error": selection_error,
        "codex_executable_identity": dict(codex_executable_identity or {}),
        "codex_build": dict(codex_build or {}),
        "counts": {
            "intended": len(requested_ids),
            "selected": len(selected_specs),
            "executed": len(executed_ids),
            **statuses,
        },
        "results": [
            {
                "id": result.case_id,
                "name": result.name,
                "status": result.status,
                "sandbox_launches": result.sandbox_launches,
                "detail": result.detail,
                "codex_executable_identity": dict(codex_executable_identity or {}),
                "codex_build": dict(codex_build or {}),
            }
            for result in results
        ],
    }


def _json_bytes(payload: dict[str, object]) -> bytes:
    return (
        json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        + "\n"
    ).encode("utf-8")


def _write_report(path: Path, payload: dict[str, object]) -> None:
    path = path.expanduser().resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    try:
        with temporary.open("xb") as handle:
            handle.write(_json_bytes(payload))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(str(temporary), str(path))
    finally:
        with contextlib.suppress(FileNotFoundError):
            temporary.unlink()


def _summarize(results: Sequence[CaseResult]) -> int:
    passed = sum(1 for result in results if result.ok)
    print("\n" + "=" * 72)
    print(f"Sandbox smoke tests: {passed}/{len(results)} passed")
    for result in results:
        label = "PASS" if result.ok else result.status.upper()
        suffix = (
            f" :: {result.detail.strip()}" if result.detail and not result.ok else ""
        )
        print(f"[{label}] {result.case_id} - {result.name}{suffix}")
    print("=" * 72)
    return 0 if passed == len(results) else 1


def _run_exact_case(args: argparse.Namespace) -> int:
    requested_ids: list[str] = args.run_case
    report_path = args.report_json
    attempt_root = args.attempt_root
    if report_path is None:
        print("--report-json is required with --run-case", file=sys.stderr)
        return 2
    if attempt_root is None:
        payload = _case_payload(
            requested_ids,
            [],
            [],
            None,
            "--attempt-root is required with --run-case",
        )
        _write_report(report_path, payload)
        return 2

    registry_errors = _registry_errors()
    if registry_errors:
        payload = _case_payload(
            requested_ids,
            [],
            [],
            attempt_root,
            f"duplicate registered case IDs: {registry_errors}",
        )
        _write_report(report_path, payload)
        return 2
    if not requested_ids:
        payload = _case_payload(
            requested_ids,
            [],
            [],
            attempt_root,
            "exact mode requires at least one --run-case selection",
        )
        _write_report(report_path, payload)
        return 2
    if len(requested_ids) != len(set(requested_ids)):
        payload = _case_payload(
            requested_ids,
            [],
            [],
            attempt_root,
            "duplicate --run-case selections are not allowed",
        )
        _write_report(report_path, payload)
        return 2

    by_id = {spec.case_id: spec for spec in _CASES}
    unknown = [case_id for case_id in requested_ids if case_id not in by_id]
    if unknown:
        payload = _case_payload(
            requested_ids,
            [],
            [],
            attempt_root,
            f"selected case IDs were missing or ambiguous: {unknown!r}",
        )
        _write_report(report_path, payload)
        return 2
    selected = [by_id[case_id] for case_id in requested_ids]

    codex_build: dict[str, object] = {}
    codex_identity: dict[str, str] = {}
    try:
        if args.build_current_codex:
            codex_build = _build_current_fork_codex(args.codex_bin)
        codex_cmd = _resolve_codex_cmd(args.codex_bin)
        codex_identity = _capture_codex_identity(codex_cmd)
        ctx = _prepare_context(attempt_root, codex_cmd, verbose=False)
    except (OSError, PreResultError) as exc:
        results = [
            CaseResult(
                spec.case_id,
                spec.name,
                "pre_result_error",
                str(exc),
                0,
            )
            for spec in selected
        ]
        _write_report(
            report_path,
            _case_payload(
                requested_ids,
                selected,
                results,
                attempt_root,
                codex_executable_identity=codex_identity,
                codex_build=codex_build,
            ),
        )
        return 2

    results = [_run_registered_case(ctx, spec) for spec in selected]
    identity_error = _finish_codex_identity(codex_identity)
    _write_report(
        report_path,
        _case_payload(
            requested_ids,
            selected,
            results,
            attempt_root,
            selection_error=identity_error,
            codex_executable_identity=codex_identity,
            codex_build=codex_build,
        ),
    )
    if identity_error:
        return 2
    if any(result.status == "failed" for result in results):
        return 1
    if any(result.status == "pre_result_error" for result in results):
        return 2
    return 0


def _run_full(args: argparse.Namespace) -> int:
    errors = _registry_errors()
    if errors:
        raise PreResultError(f"duplicate registered case IDs: {errors}")
    if args.build_current_codex:
        _build_current_fork_codex(args.codex_bin)
    codex_cmd = _resolve_codex_cmd(args.codex_bin)
    if args.attempt_root is not None:
        ctx = _prepare_context(args.attempt_root, codex_cmd, verbose=True)
        return _summarize([_run_registered_case(ctx, spec) for spec in _CASES])
    with tempfile.TemporaryDirectory(prefix="codex-windows-sandbox-smoke-") as root:
        ctx = _prepare_context(Path(root), codex_cmd, verbose=True)
        return _summarize([_run_registered_case(ctx, spec) for spec in _CASES])


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--list-json",
        action="store_true",
        help="write the deterministic case inventory to stdout",
    )
    parser.add_argument(
        "--run-case",
        action="append",
        default=[],
        metavar="STABLE_ID",
        help="run an immutable case ID (repeat for a complete exact selection)",
    )
    parser.add_argument(
        "--report-json", type=Path, help="structured exact-case report path"
    )
    parser.add_argument(
        "--attempt-root",
        type=Path,
        help="fresh caller-owned temporary root for all case files",
    )
    parser.add_argument(
        "--codex-bin",
        type=Path,
        help="exact Codex CLI binary (otherwise resolve the local build)",
    )
    parser.add_argument(
        "--build-current-codex",
        action="store_true",
        help="build and require the exact current-fork debug Codex binary",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if args.list_json:
        if (
            args.run_case
            or args.report_json
            or args.attempt_root
            or args.codex_bin
            or args.build_current_codex
        ):
            print(
                "--list-json cannot be combined with execution options", file=sys.stderr
            )
            return 2
        errors = _registry_errors()
        if errors:
            print(f"duplicate registered case IDs: {errors}", file=sys.stderr)
            return 2
        sys.stdout.buffer.write(_json_bytes(_list_payload()))
        return 0
    if args.run_case or args.report_json is not None:
        return _run_exact_case(args)
    try:
        return _run_full(args)
    except (OSError, PreResultError) as exc:
        print(f"pre-result error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
