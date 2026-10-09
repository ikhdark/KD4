# sandbox_smoketests.py
# Run a suite of smoke tests against the Windows sandbox via the Codex CLI
# Requires: Python 3.8+ on Windows. No pip requirements.

import os
import sys
import shutil
import subprocess
import contextlib
import http.client
import http.server
import threading
import tempfile
from pathlib import Path
from typing import List, Optional, Tuple
from urllib.parse import urlsplit

def _resolve_codex_cmd() -> List[str]:
    """Resolve the Codex CLI to invoke `codex sandbox windows`.

    Prefer local builds (debug first), then fall back to PATH.
    Returns the argv prefix to run Codex.
    """
    root = Path(__file__).parent
    ws_root = root.parent
    cargo_target = os.environ.get("CARGO_TARGET_DIR")

    candidates = [
        ws_root / "target" / "debug" / "codex.exe",
        ws_root / "target" / "release" / "codex.exe",
    ]
    if cargo_target:
        cargo_base = Path(cargo_target)
        candidates.extend([
            cargo_base / "debug" / "codex.exe",
            cargo_base / "release" / "codex.exe",
        ])

    for candidate in candidates:
        if candidate.exists():
            return [str(candidate)]

    if shutil.which("codex"):
        return ["codex"]

    raise FileNotFoundError(
        "Codex CLI not found. Build it first, e.g.\n"
        "  cargo build -p codex-cli --release\n"
        "or for debug:\n"
        "  cargo build -p codex-cli\n"
    )

CODEX_CMD = None
TIMEOUT_SEC = 20

ENV_BASE = {}  # extend if needed

class CaseResult:
    def __init__(self, name: str, ok: Optional[bool], detail: str = ""):
        self.name, self.ok, self.detail = name, ok, detail

def run_sbx(
    policy: str,
    cmd_argv: List[str],
    cwd: Path,
    env_extra: Optional[dict] = None,
    additional_root: Optional[Path] = None,
) -> Tuple[int, str, str]:
    env = os.environ.copy()
    env.update(ENV_BASE)
    if env_extra:
        env.update(env_extra)
    # Map policy to codex CLI overrides.
    # Explicit modes must not inherit a user's ambient permission profile.
    if policy not in ("read-only", "workspace-write"):
        raise ValueError(f"unknown policy: {policy}")
    policy_flags: List[str] = ["-c", f'sandbox_mode="{policy}"']

    overrides: List[str] = []
    if additional_root is not None:
        # Use config override to inject an additional writable root.
        overrides = [
            "-c",
            f'sandbox_workspace_write.writable_roots=["{additional_root.as_posix()}"]',
        ]

    argv = [*(CODEX_CMD or _resolve_codex_cmd()), "sandbox", "windows", *policy_flags, *overrides, "--", *cmd_argv]
    print(cmd_argv)
    cp = subprocess.run(argv, cwd=str(cwd), env=env,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                        timeout=TIMEOUT_SEC, text=True)
    return cp.returncode, cp.stdout, cp.stderr

def have(cmd: str) -> bool:
    return shutil.which(cmd) is not None

def make_dir_clean(p: Path) -> None:
    if p.exists():
        shutil.rmtree(p, ignore_errors=True)
    p.mkdir(parents=True, exist_ok=True)

def write_file(p: Path, content: str = "x") -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content, encoding="utf-8")

def remove_if_exists(p: Path) -> None:
    try:
        if p.is_dir(): shutil.rmtree(p, ignore_errors=True)
        elif p.exists(): p.unlink(missing_ok=True)
    except Exception:
        pass

def assert_exists(p: Path) -> bool:
    return p.exists()

def assert_not_exists(p: Path) -> bool:
    return not p.exists()

def make_junction(link: Path, target: Path) -> bool:
    """Create a directory junction; return True if it exists afterward."""
    remove_if_exists(link)
    target.mkdir(parents=True, exist_ok=True)
    cmd = ["cmd", "/c", f'mklink /J "{link}" "{target}"']
    cp = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    return cp.returncode == 0 and link.exists()

def make_symlink(link: Path, target: Path) -> bool:
    """Create a directory symlink; return True if it exists afterward."""
    remove_if_exists(link)
    if not target.exists():
        try:
            target.mkdir(parents=True, exist_ok=True)
        except OSError:
            pass
    cmd = ["cmd", "/c", f'mklink /D "{link}" "{target}"']
    cp = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    return cp.returncode == 0 and link.exists()

class _QuietHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

class _TargetHandler(_QuietHandler):
    def do_GET(self):
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
        conn = None
        try:
            conn = http.client.HTTPConnection(parsed.hostname, parsed.port or 80, timeout=2)
            conn.request("GET", path)
            upstream = conn.getresponse()
            body = upstream.read()
        except Exception as err:
            self.send_error(502, f"proxy upstream error: {err}")
            return
        finally:
            if conn is not None:
                with contextlib.suppress(Exception):
                    conn.close()
        self.send_response(upstream.status, upstream.reason)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

@contextlib.contextmanager
def start_loopback_proxy_fixture():
    target = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _TargetHandler)
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _ProxyHandler)
    target_port = target.server_address[1]
    proxy_port = proxy.server_address[1]
    target_thread = threading.Thread(target=target.serve_forever, daemon=True)
    proxy_thread = threading.Thread(target=proxy.serve_forever, daemon=True)
    target_thread.start()
    proxy_thread.start()
    try:
        yield target_port, proxy_port
    finally:
        proxy.shutdown()
        target.shutdown()
        proxy.server_close()
        target.server_close()

def summarize(results: List[CaseResult]) -> int:
    ok = sum(1 for r in results if r.ok is True)
    skipped = sum(1 for r in results if r.ok is None)
    total = len(results)
    print("\n" + "=" * 72)
    print(f"Sandbox smoke tests: {ok}/{total} passed, {skipped} skipped")
    for r in results:
        status = "SKIP" if r.ok is None else "PASS" if r.ok else "FAIL"
        print(f"[{status}] {r.name}" + (f" :: {r.detail.strip()}" if r.detail and r.ok is not True else ""))
    print("=" * 72)
    return 0 if ok and ok + skipped == total else 1

def main() -> int:
    global CODEX_CMD, WS_ROOT, OUTSIDE, EXTRA_ROOT
    if os.name != "nt":
        raise RuntimeError("Windows sandbox smoke tests require Windows")
    CODEX_CMD = _resolve_codex_cmd()
    # Keep deny targets outside TEMP and the workspace, without deleting any
    # fixed user-owned directory or using the user's real CODEX_HOME.
    with tempfile.TemporaryDirectory(prefix="codex-sandbox-smoke-", dir=Path.home()) as directory:
        root = Path(directory)
        WS_ROOT, OUTSIDE, EXTRA_ROOT = (root / name for name in ("workspace", "outside", "extra"))
        home, temp = root / "home", root / "temp"
        home.mkdir()
        temp.mkdir()
        ENV_BASE.update(CODEX_HOME=str(home), TEMP=str(temp), TMP=str(temp), TMPDIR=str(temp))
        try:
            return run_cases()
        finally:
            ENV_BASE.clear()

def run_cases() -> int:
    results: List[CaseResult] = []
    make_dir_clean(WS_ROOT)
    OUTSIDE.mkdir(exist_ok=True)
    EXTRA_ROOT.mkdir(exist_ok=True)
    def add(name: str, ok: Optional[bool], detail: str = ""):
        print('running', name)
        results.append(CaseResult(name, ok, detail))

    # 1. RO: deny write in CWD
    target = WS_ROOT / "ro_should_fail.txt"
    remove_if_exists(target)
    rc, out, err = run_sbx("read-only", ["cmd", "/c", "echo nope > ro_should_fail.txt"], WS_ROOT)
    add("RO: write in CWD denied", rc != 0 and assert_not_exists(target), f"rc={rc}, err={err}")

    # 2. WS: allow write in CWD
    target = WS_ROOT / "ws_ok.txt"
    remove_if_exists(target)
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo ok > ws_ok.txt"], WS_ROOT)
    add("WS: write in CWD allowed", rc == 0 and target.read_text().strip() == "ok", f"rc={rc}, err={err}")

    # 3. WS: deny write outside workspace
    outside_file = OUTSIDE / "blocked.txt"
    remove_if_exists(outside_file)
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", f'echo nope > "{outside_file}"'], WS_ROOT)
    add("WS: write outside workspace denied", rc != 0 and assert_not_exists(outside_file), f"rc={rc}")

    # 3b. WS: allow write in additional workspace root
    extra_target = EXTRA_ROOT / "extra_ok.txt"
    remove_if_exists(extra_target)
    rc, out, err = run_sbx(
        "workspace-write",
        ["cmd", "/c", f'echo extra > "{extra_target}"'],
        WS_ROOT,
        additional_root=EXTRA_ROOT,
    )
    add("WS: write in additional root allowed", rc == 0 and extra_target.read_text().strip() == "extra", f"rc={rc}, err={err}")

    # 3c. RO: deny write in additional workspace root
    ro_extra_target = EXTRA_ROOT / "extra_ro.txt"
    remove_if_exists(ro_extra_target)
    rc, out, err = run_sbx(
        "read-only",
        ["cmd", "/c", f'echo nope > "{ro_extra_target}"'],
        WS_ROOT,
        additional_root=EXTRA_ROOT,
    )
    add(
        "RO: write in additional root denied",
        rc != 0 and assert_not_exists(ro_extra_target),
        f"rc={rc}",
    )

    # 4. WS: allow TEMP write
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo tempok > %TEMP%\\ws_temp_ok.txt"], WS_ROOT)
    add("WS: TEMP write allowed", rc == 0 and (Path(ENV_BASE["TEMP"]) / "ws_temp_ok.txt").read_text().strip() == "tempok", f"rc={rc}")

    # 5. RO: deny TEMP write
    rc, out, err = run_sbx("read-only", ["cmd", "/c", "echo tempno > %TEMP%\\ro_temp_fail.txt"], WS_ROOT)
    add("RO: TEMP write denied", rc != 0 and not (Path(ENV_BASE["TEMP"]) / "ro_temp_fail.txt").exists(), f"rc={rc}")

    # 6. WS: append OK in CWD
    target = WS_ROOT / "append.txt"
    remove_if_exists(target); write_file(target, "line1\n")
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo line2 >> append.txt"], WS_ROOT)
    add("WS: append allowed", rc == 0 and [line.rstrip() for line in target.read_text().splitlines()] == ["line1", "line2"], f"rc={rc}")

    # 7. RO: append denied
    target = WS_ROOT / "ro_append.txt"
    write_file(target, "line1\n")
    rc, out, err = run_sbx("read-only", ["cmd", "/c", "echo line2 >> ro_append.txt"], WS_ROOT)
    add("RO: append denied", rc != 0 and target.read_text() == "line1\n", f"rc={rc}")

    # 8. WS: PowerShell Set-Content in CWD (OK)
    target = WS_ROOT / "ps_ok.txt"
    remove_if_exists(target)
    rc, out, err = run_sbx("workspace-write",
                           ["powershell", "-NoLogo", "-NoProfile", "-Command",
                            "Set-Content -LiteralPath ps_ok.txt -Value 'hello' -Encoding ASCII"], WS_ROOT)
    add("WS: PowerShell Set-Content allowed", rc == 0 and target.read_bytes() == b"hello\r\n", f"rc={rc}, err={err}")

    # 9. RO: PowerShell Set-Content denied
    target = WS_ROOT / "ps_ro_fail.txt"
    remove_if_exists(target)
    rc, out, err = run_sbx("read-only",
                           ["powershell", "-NoLogo", "-NoProfile", "-Command",
                            "$ErrorActionPreference='Stop'; Set-Content -LiteralPath ps_ro_fail.txt -Value 'x'"], WS_ROOT)
    add("RO: PowerShell Set-Content denied", rc != 0 and assert_not_exists(target), f"rc={rc}")

    # 10. WS: mkdir and write (OK)
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "mkdir sub && echo hi > sub\\in_sub.txt"], WS_ROOT)
    add("WS: mkdir+write allowed", rc == 0 and (WS_ROOT / "sub/in_sub.txt").read_text().strip() == "hi", f"rc={rc}")

    # 11. WS: rename (EXPECTED SUCCESS on this host)
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo x > r.txt & ren r.txt r2.txt"], WS_ROOT)
    add("WS: rename succeeds", rc == 0 and not (WS_ROOT / "r.txt").exists() and (WS_ROOT / "r2.txt").read_text().strip() == "x", f"rc={rc}, err={err}")

    # 12. WS: delete (EXPECTED SUCCESS on this host)
    target = WS_ROOT / "delme.txt"; write_file(target, "x")
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "del /q delme.txt"], WS_ROOT)
    add("WS: delete succeeds (expected on this host)", rc == 0 and not target.exists(), f"rc={rc}, err={err}")

    # 13. RO: python tries to write (denied)
    pyfile = WS_ROOT / "py_should_fail.txt"; remove_if_exists(pyfile)
    rc, out, err = run_sbx("read-only", [sys.executable, "-c", "open('py_should_fail.txt','w').write('x')"], WS_ROOT)
    add("RO: python file write denied", rc != 0 and assert_not_exists(pyfile), f"rc={rc}")

    # 14. WS: python writes file (OK)
    pyfile = WS_ROOT / "py_ok.txt"; remove_if_exists(pyfile)
    rc, out, err = run_sbx("workspace-write", [sys.executable, "-c", "open('py_ok.txt','w').write('x')"], WS_ROOT)
    add("WS: python file write allowed", rc == 0 and pyfile.read_bytes() == b"x", f"rc={rc}, err={err}")

    # Network denial uses a known-live fixture, not public DNS/site availability.
    if have("curl"):
        with start_loopback_proxy_fixture() as (target_port, proxy_port):
            proxy_home = Path(ENV_BASE["CODEX_HOME"]).with_name("proxy-home")
            remove_if_exists(proxy_home)
            proxy_home.mkdir(parents=True, exist_ok=True)
            proxy_url = f"http://127.0.0.1:{proxy_port}"
            proxy_env = {
                "CODEX_HOME": str(proxy_home),
                "HTTP_PROXY": proxy_url,
                "http_proxy": proxy_url,
                "ALL_PROXY": proxy_url,
                "all_proxy": proxy_url,
                "NO_PROXY": "",
                "no_proxy": "",
            }
            proxied_cmd = [
                "curl",
                "--noproxy",
                "",
                "--connect-timeout",
                "2",
                "--max-time",
                "4",
                f"http://127.0.0.1:{target_port}/proxied",
            ]
            rc_proxy, out_proxy, err_proxy = run_sbx(
                "workspace-write",
                proxied_cmd,
                WS_ROOT,
                env_extra=proxy_env,
            )
            add(
                "WS: loopback proxy allowed",
                rc_proxy == 0 and out_proxy == "proxy-ok",
                f"rc={rc_proxy}, out={out_proxy}, err={err_proxy}",
            )

            direct_cmd = [
                "curl",
                "--noproxy",
                "*",
                "--connect-timeout",
                "1",
                "--max-time",
                "2",
                f"http://127.0.0.1:{target_port}/direct",
            ]
            rc_direct, _out_direct, err_direct = run_sbx(
                "workspace-write",
                direct_cmd,
                WS_ROOT,
                env_extra={"CODEX_HOME": str(proxy_home)},
            )
            add("WS: direct loopback blocked", rc_direct != 0, f"rc={rc_direct}, err={err_direct}")
            rc, out, err = run_sbx(
                "workspace-write",
                ["powershell", "-NoLogo", "-NoProfile", "-Command",
                 f"$ErrorActionPreference='Stop'; iwr http://127.0.0.1:{target_port}/direct -UseBasicParsing -TimeoutSec 2"],
                WS_ROOT,
                env_extra={"CODEX_HOME": str(proxy_home)},
            )
            add("WS: iwr direct loopback blocked", rc != 0, f"rc={rc}, err={err}")
    else:
        add("WS: direct/proxy loopback tests (curl missing)", None, "curl not installed")

    # 18. RO: deny TEMP writes via PowerShell
    rc, out, err = run_sbx("read-only",
                           ["powershell", "-NoLogo", "-NoProfile", "-Command",
                            "$ErrorActionPreference='Stop'; Set-Content -LiteralPath $env:TEMP\\ro_tmpfail.txt -Value 'x'"], WS_ROOT)
    add("RO: TEMP write denied (PS)", rc != 0 and not (Path(ENV_BASE["TEMP"]) / "ro_tmpfail.txt").exists(), f"rc={rc}")

    # 19. WS: curl version check — don't rely on stub, just succeed
    if have("curl"):
        rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "curl --version"], WS_ROOT)
        add("WS: curl present (version prints)", rc == 0 and out.startswith("curl "), f"rc={rc}, err={err}")
    else:
        add("WS: curl present (optional, skipped)", None)

    # 20. Optional: ripgrep version
    if have("rg"):
        rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "rg --version"], WS_ROOT)
        add("WS: rg --version (optional)", rc == 0 and out.startswith("ripgrep "), f"rc={rc}, err={err}")
    else:
        add("WS: rg --version (optional, skipped)", None)

    # 21. Optional: git --version
    if have("git"):
        rc, out, err = run_sbx("workspace-write", ["git", "--version"], WS_ROOT)
        add("WS: git --version (optional)", rc == 0 and out.startswith("git version "), f"rc={rc}, err={err}")
    else:
        add("WS: git --version (optional, skipped)", None)

    # 24. WS: PS bytes write (OK)
    rc, out, err = run_sbx("workspace-write",
                           ["powershell", "-NoLogo", "-NoProfile", "-Command",
                            "[IO.File]::WriteAllBytes('bytes_ok.bin',[byte[]](0..255))"], WS_ROOT)
    add("WS: PS bytes write allowed", rc == 0 and (WS_ROOT / "bytes_ok.bin").read_bytes() == bytes(range(256)), f"rc={rc}")

    # 25. RO: PS bytes write denied
    rc, out, err = run_sbx("read-only",
                           ["powershell", "-NoLogo", "-NoProfile", "-Command",
                            "[IO.File]::WriteAllBytes('bytes_fail.bin',[byte[]](0..10))"], WS_ROOT)
    add("RO: PS bytes write denied", rc != 0 and not (WS_ROOT / "bytes_fail.bin").exists(), f"rc={rc}")

    # 26. WS: deep mkdir and write (OK)
    rc, out, err = run_sbx("workspace-write",
                           ["cmd", "/c", "mkdir deep\\nest && echo ok > deep\\nest\\f.txt"], WS_ROOT)
    add("WS: deep mkdir+write allowed", rc == 0 and (WS_ROOT / "deep/nest/f.txt").read_text().strip() == "ok", f"rc={rc}")

    # 27. WS: move (EXPECTED SUCCESS on this host)
    rc, out, err = run_sbx("workspace-write",
                           ["cmd", "/c", "echo x > m1.txt & move /y m1.txt m2.txt"], WS_ROOT)
    add("WS: move succeeds", rc == 0 and not (WS_ROOT / "m1.txt").exists() and (WS_ROOT / "m2.txt").read_text().strip() == "x", f"rc={rc}, err={err}")
    # Reparse-point containment: the denied target is writable by the host,
    # unlike C:\Windows, where ordinary host ACLs could cause a false pass.
    deep = WS_ROOT / "deep" / "redir"
    junction_target = OUTSIDE / "junction.txt"
    if make_junction(deep, OUTSIDE):
        rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo probe > deep\\redir\\junction.txt"], WS_ROOT)
        add("WS: deep junction escape denied", rc != 0 and not junction_target.exists(), f"rc={rc}, err={err}")
    else:
        add("WS: deep junction escape denied", None, "junction creation failed")

    # ADS are not categorically forbidden: they inherit their base file's ACL.
    # Exercise a real outside-workspace stream rather than claiming workspace
    # ADS writes violate a policy which only restricts filesystem roots.
    ads_base = OUTSIDE / "ads_base.txt"
    write_file(ads_base, "unchanged")
    ads_stream = Path(f"{ads_base}:stream")
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", f'echo secret > "{ads_stream}"'], WS_ROOT)
    add("WS: outside ADS write denied", rc != 0 and ads_base.read_bytes() == b"unchanged" and not ads_stream.exists(), f"rc={rc}")

    lp_target = OUTSIDE / "longpath.txt"
    extended_path = "\\\\?\\" + str(lp_target)
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", f'echo long > "{extended_path}"'], WS_ROOT)
    add("WS: long-path escape denied", rc != 0 and not lp_target.exists(), f"rc={rc}")

    # Case-insensitive protected path bypass denied (.GiT).
    git_variation = WS_ROOT / ".GiT" / "config"
    git_variation.parent.mkdir(exist_ok=True)
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo hack > .GiT\\config"], WS_ROOT)
    add("WS: protected path case-variation denied", rc != 0 and not git_variation.exists(), f"rc={rc}")

    # Never target the user's real .codex. Verify existing bytes are preserved,
    # not just that cmd happens to return an error (e.g. from a missing parent).
    cap_sid_target = Path(ENV_BASE["CODEX_HOME"]) / "cap_sid"
    cap_before = cap_sid_target.read_bytes() if cap_sid_target.exists() else None
    rc, out, err = run_sbx(
        "workspace-write", ["cmd", "/c", f'echo tamper > "{cap_sid_target}"'], WS_ROOT,
    )
    cap_after = cap_sid_target.read_bytes() if cap_sid_target.exists() else None
    add("WS: .codex cap_sid tamper denied", rc != 0 and cap_after == cap_before, f"rc={rc}, err={err}")
    policy = WS_ROOT / ".codex" / "policy.json"
    write_file(policy, "unchanged")
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo tamper > .codex\\policy.json"], WS_ROOT)
    add("WS: .codex policy tamper denied", rc != 0 and policy.read_bytes() == b"unchanged", f"rc={rc}, err={err}")

    # This checks PATH propagation, not whether a network tool can bypass stubs.
    tools_dir = WS_ROOT / "tools"
    tools_dir.mkdir(exist_ok=True)
    shim = tools_dir / "smoke-command.bat"
    shim.write_bytes(b"@echo off\r\necho smoke-shim\r\n")
    env = {"PATH": f"{tools_dir};{os.environ.get('PATH', '')}"}
    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "smoke-command"], WS_ROOT, env_extra=env)
    add("WS: workspace PATH shim resolves", rc == 0 and out.strip() == "smoke-shim", f"rc={rc}, out={out}")

    # The old unsynchronized toggle could never prove a race was exercised and
    # checked the wrong outside directory. Keep a deterministic symlink escape
    # check; no leaked toggler process or race-resistance claim.
    link = WS_ROOT / "outside_link"
    symlink_target = OUTSIDE / "symlink.txt"
    if make_symlink(link, OUTSIDE):
        rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo escape > outside_link\\symlink.txt"], WS_ROOT)
        add("WS: symlink escape denied", rc != 0 and not symlink_target.exists(), f"rc={rc}, err={err}")
    else:
        add("WS: symlink escape denied", None, "symlink creation failed")

    # UNC canonicalization uses this run's disposable target, not C:\.
    unc_link = WS_ROOT / "unc_link"
    relative = OUTSIDE.relative_to(OUTSIDE.anchor)
    unc_target = Path(f"\\\\localhost\\{OUTSIDE.drive[0]}$\\{relative}")
    if unc_target.exists() and make_symlink(unc_link, unc_target):
        rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo unc > unc_link\\unc_test.txt"], WS_ROOT)
        add("WS: UNC link escape denied", rc != 0 and not (OUTSIDE / "unc_test.txt").exists(), f"rc={rc}")
    else:
        add("WS: UNC link escape denied", None, "local administrative share or symlink privilege unavailable")

    other_drive = WS_ROOT / "other_drive"
    if Path("D:/").is_dir() and OUTSIDE.drive.upper() != "D:":
        with tempfile.TemporaryDirectory(prefix="codex-sandbox-smoke-", dir="D:/") as directory:
            other_target = Path(directory)
            if make_symlink(other_drive, other_target):
                try:
                    rc, out, err = run_sbx("workspace-write", ["cmd", "/c", "echo drive > other_drive\\drive.txt"], WS_ROOT)
                    add("WS: other-drive link escape denied", rc != 0 and not (other_target / "drive.txt").exists(), f"rc={rc}")
                finally:
                    other_drive.unlink()
            else:
                add("WS: other-drive link escape denied", None, "symlink creation failed")
    else:
        add("WS: other-drive link escape denied", None, "second drive unavailable")

    # A regression can launch a real browser: require an explicitly opted-in
    # disposable desktop rather than doing so in the default smoke run.
    if os.environ.get("CODEX_SMOKE_GUI_TESTS") == "1":
        rc, out, err = run_sbx(
            "read-only",
            ["powershell", "-NoLogo", "-NoProfile", "-Command",
             "$ErrorActionPreference='Stop'; Start-Process 'https://codex-invalid.local/smoke'"],
            WS_ROOT,
        )
        add("RO: Start-Process https denied", rc != 0, f"rc={rc}, stdout={out}, stderr={err}")
    else:
        add("RO: Start-Process https denied", None, "set CODEX_SMOKE_GUI_TESTS=1 on a disposable desktop")

    return summarize(results)

if __name__ == "__main__":
    sys.exit(main())
