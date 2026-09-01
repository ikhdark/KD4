#!/usr/bin/env python3
"""Run a minimal paired live-agent benchmark for KD4 versus official upstream C."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import queue
import re
import shutil
import signal
import statistics
import subprocess
import tempfile
import threading
import time
from collections import Counter, deque
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

TASK_PROMPT = """Fix `parse_duration` in `duration.py` so it satisfies the contract in
`README.md`. Work until the implementation is correct and the tests pass. Modify only
`duration.py`, and run `python -m unittest -q` before finishing.
"""

FIXTURE_FILES = {
    ".gitignore": "__pycache__/\n*.pyc\n",
    "AGENTS.md": """# Task instructions

- Modify only `duration.py`.
- Use only the Python standard library.
- Run `python -m unittest -q` before finishing.
""",
    "README.md": """# Duration parser

Implement `parse_duration(text)` in `duration.py`.

- Return the duration as an integer number of milliseconds.
- Accepted units are `h`, `m`, `s`, and `ms`.
- A duration contains one or more non-negative ASCII decimal integer components.
- Components may touch or be separated by whitespace.
- Components must be ordered from largest to smallest unit and each unit may appear once.
- Leading and trailing whitespace is allowed.
- Empty strings, decimals, signs, unsupported units, repeated units, out-of-order units,
  and any unmatched text must raise `ValueError`.

Examples: `500ms`, `2m15s`, and `1h 30m 4ms`.
""",
    "duration.py": """import re


_MULTIPLIERS = {"ms": 1, "s": 1_000, "m": 60_000, "h": 3_600_000}


def parse_duration(text: str) -> int:
    match = re.fullmatch(r"(\\d+)(ms|s|m|h)", text.strip())
    if match is None:
        raise ValueError("invalid duration")
    value, unit = match.groups()
    return int(value) * _MULTIPLIERS[unit]
""",
    "test_duration.py": """import unittest

from duration import parse_duration


class ParseDurationTests(unittest.TestCase):
    def test_single_component(self):
        self.assertEqual(parse_duration("12s"), 12_000)
        self.assertEqual(parse_duration("500ms"), 500)

    def test_compound_components(self):
        self.assertEqual(parse_duration("2m15s"), 135_000)
        self.assertEqual(parse_duration("1h 30m 4ms"), 5_400_004)

    def test_whitespace(self):
        self.assertEqual(parse_duration("  1h   2m 3s 4ms  "), 3_723_004)

    def test_invalid_input(self):
        for text in ("", "1.5s", "-1s", "1x", "1s junk", "1s2m", "1m1m"):
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    parse_duration(text)


if __name__ == "__main__":
    unittest.main()
""",
}

CORRECT_IMPLEMENTATION = """import re


_MULTIPLIERS = {"ms": 1, "s": 1_000, "m": 60_000, "h": 3_600_000}
_RANKS = {"h": 3, "m": 2, "s": 1, "ms": 0}
_COMPONENT = re.compile(r"(?a)(\\d+)(ms|[hms])")


def parse_duration(text: str) -> int:
    if not isinstance(text, str):
        raise ValueError("invalid duration")
    position = 0
    total = 0
    previous_rank = 4
    matched = False
    while position < len(text):
        while position < len(text) and text[position].isspace():
            position += 1
        if position == len(text):
            break
        component = _COMPONENT.match(text, position)
        if component is None:
            raise ValueError("invalid duration")
        value, unit = component.groups()
        rank = _RANKS[unit]
        if rank >= previous_rank:
            raise ValueError("invalid duration")
        total += int(value) * _MULTIPLIERS[unit]
        previous_rank = rank
        matched = True
        position = component.end()
    if not matched:
        raise ValueError("invalid duration")
    return total
"""

VALID_CASES = {
    "0ms": 0,
    "1ms": 1,
    "500ms": 500,
    "1s": 1_000,
    "2m15s": 135_000,
    "1h30m": 5_400_000,
    "1h 30m 4ms": 5_400_004,
    " 1h   2m 3s 4ms ": 3_723_004,
}

INVALID_CASES = (
    "",
    "   ",
    "-1s",
    "+1s",
    "1.5s",
    "1x",
    "1s junk",
    "junk 1s",
    "1s2m",
    "1m1m",
    "1ms1s",
    # A component may not be split between its value and its unit.
    "1 s",
    "1h 30 m",
    "1h30 m",
    # Repeated units stay invalid when separated by whitespace.
    "1s 1s",
    "1ms 1s",
    # Out-of-order units stay invalid when separated by whitespace.
    "30ms1m",
    "30ms 1m",
    "1m 1h",
    # A unit with no value, and a value with no unit.
    "s",
    "ms",
    "1",
    "1h 30",
    # Non-ASCII digits are not integers for this contract.
    "١s",
)

REPORT_SCHEMA_VERSION = 6
TURN_TRACE_SCHEMA_VERSION = 2
REQUIRED_TEST_COMMAND = "python -m unittest -q"
# Hard ceiling for each post-turn check. Both run agent-authored code, so an
# unbounded wait lets one bad run stall the entire benchmark.
VERIFIER_TIMEOUT_SECONDS = 120
# Ceiling for joining a reader thread once the agent has been reaped. A leaked
# grandchild can hold the inherited pipe open, so this join must be bounded and
# its outcome reported rather than assumed.
READER_JOIN_TIMEOUT_SECONDS = 5
# Trailing shell redirections such as `> out.txt`, `2>&1`, `>> log`, or `< in`.
# A redirection changes where the suite's output goes, never which tests run, so
# it must not disqualify an otherwise-exact required-test invocation. Leaving it
# out made `python -m unittest -q 2>&1 | tail -5` — an ordinary agent idiom —
# read as non-compliant, and the choice of idiom is itself model-dependent, so
# the omission fed a build-correlated bias straight into the headline rate.
_REDIRECTION_SUFFIX = r"(?:\s*\d*(?:>>?|<)\s*(?:&\d+|[^\s>|;&]+))*"
# Matches one complete shell command segment that runs the required suite. The
# interpreter may be an absolute or relative path (quoted when it contains
# spaces), and output may be redirected, but no unittest selector or other
# trailing argument is accepted.
_REQUIRED_TEST_PATTERN = re.compile(
    r"(?ix)"
    r"(?:[A-Z_][A-Z0-9_]*=\S+\s+)*"
    r"(?:"
    r"\"[^\"]*(?:python(?:\d+(?:\.\d+)*)?|py)(?:\.exe)?\""
    r"|'[^']*(?:python(?:\d+(?:\.\d+)*)?|py)(?:\.exe)?'"
    r"|(?:[^\s\"';&|]*[\\/])?(?:python(?:\d+(?:\.\d+)*)?|py)(?:\.exe)?"
    r")"
    r"\s+-m\s+unittest\s+-q"
    + _REDIRECTION_SUFFIX
    + r"\s*"
)
# Separators between shell command segments. The `&` of a descriptor-duplicating
# redirection (`2>&1`) is part of that redirection, not a separator: splitting
# there left a dangling `2>` that read as a file-opening write and truncated the
# required-test match.
_SHELL_SEGMENT = re.compile(r"(?:&&|\|\||[;|]|(?<!>)&)")
# The same separators with the operator captured, so a matched required-test
# segment can be checked against what runs after it.
_SHELL_SEGMENT_CAPTURE = re.compile(r"(&&|\|\||[;|]|(?<!>)&)")
# Separators that unconditionally replace the exit code of the command before
# them: a `||` fallback, a `;` sequel, and a backgrounding `&`. `&&` preserves
# a failure, and `|` is the accepted output idiom above whose propagation
# depends on shell configuration the harness cannot see.
_EXIT_MASKING_SEPARATORS = frozenset({"||", ";", "&"})

# `command_display_string` joins the executed argv, so a command the agent ran
# through a shell arrives as `bash -lc <script>` rather than as the script. Every
# command predicate below is applied to the raw text *and* to the unwrapped
# script so wrapper choice never changes the classification.
_TOKEN = re.compile(r"\"[^\"]*\"|'[^']*'|\S+")
_SHELL_WRAPPERS: tuple[tuple[re.Pattern[str], frozenset[str]], ...] = (
    (
        re.compile(r"(?i)^(?:.*[\\/])?(?:ba|z|k|da)?sh(?:\.exe)?$"),
        frozenset({"-c", "-lc", "-ic", "-lic", "-cl"}),
    ),
    (
        re.compile(r"(?i)^(?:.*[\\/])?(?:powershell|pwsh)(?:\.exe)?$"),
        frozenset({"-command", "-c", "-encodedcommand"}),
    ),
    (
        re.compile(r"(?i)^(?:.*[\\/])?cmd(?:\.exe)?$"),
        frozenset({"/c", "/k"}),
    ),
)

# Tool names that can create a JSONL `command_execution` item. `write_stdin`
# deliberately is not included: it can advance an existing unified-exec
# process, but it does not create a new command item to pair by ordinal.
_EXEC_TOOL_NAMES = frozenset(
    {
        "container.exec",
        "exec",
        "exec_command",
        "local_shell",
        "shell",
        "unified_exec",
    }
)

# Tools that write to the workspace without running a shell command. An edit
# normally arrives through one of these rather than through `command_execution`,
# so a mutation check that only reads command text would miss every patch.
_MUTATING_TOOL_NAMES = frozenset(
    {"apply_patch", "applypatch", "edit_file", "patch", "write_file"}
)

# Item types whose completion hands a result back to the model and therefore
# closes a harness-observable round. Used for the stream-derived reconstruction
# that stays available when the build emits no timing block and when a run is
# killed before any terminal event.
_TOOL_ITEM_TYPES = frozenset(
    {
        "command_execution",
        "file_change",
        "mcp_tool_call",
        "collab_tool_call",
        "web_search",
    }
)

# Every retained collection has a hard ceiling. A live agent can emit arbitrary
# output and command text, so report construction must not turn one pathological
# run into unbounded harness memory or an enormous JSON artifact. Counts used for
# scoring continue to observe the full stream; overflow fields state what the
# reviewable trace omitted.
MAX_RETAINED_STREAM_EVENTS = 4_096
MAX_RETAINED_MODEL_REQUESTS = 512
MAX_RETAINED_TOOL_CALLS = 512
MAX_RETAINED_COMMANDS = 512
MAX_COMMAND_TEXT_CHARS = 1_000
MAX_RETAINED_REQUIRED_TEST_ATTEMPTS = 256
MAX_QUEUED_STDOUT_LINES = 64
MAX_STREAM_LINE_CHARS = 4_000_000
# Diagnostic text and stderr keep the *newest* lines on overflow: failure
# signals cluster at the end of a transcript, so the oldest line is the one
# to drop.
MAX_DIAGNOSTIC_TEXT_LINES = 2_048
MAX_STDERR_LINES = 512

# Generation purposes emitted by the build's timing instrumentation.
_RECOVERY_PURPOSES = frozenset({"failure_diagnosis", "repair", "compaction_recovery"})
_VERIFICATION_PURPOSES = frozenset({"validation_interpretation"})
_RETRY_ATTEMPT_KINDS = frozenset({"retry", "fallback"})

# Single-label taxonomy for a model round, in the precedence
# `classify_model_request` applies. `initial` is the turn's first generation and
# `necessary` is the residual left when no other predicate matches; the
# independent `tags` list keeps every observation the single label cannot carry.
CONTINUATION_CLASS_PRECEDENCE = ("retry", "recovery", "verification", "non_progress")
REQUEST_CLASSES = ("initial", *CONTINUATION_CLASS_PRECEDENCE, "necessary")

# Leading program names that only read state. Anything not listed here and not
# listed as mutating is reported as `other` rather than guessed at.
_READ_ONLY_LEADERS = frozenset(
    {
        "cat",
        "date",
        "dir",
        "du",
        "echo",
        "env",
        "file",
        "find",
        "findstr",
        "grep",
        "head",
        "hostname",
        "ls",
        "nl",
        "od",
        "printenv",
        "pwd",
        "rg",
        "stat",
        "tail",
        "tree",
        "type",
        "wc",
        "where",
        "which",
        "whoami",
    }
)
_MUTATING_LEADERS = frozenset(
    {
        "apply_patch",
        "applypatch",
        "chmod",
        "chown",
        "copy",
        "cp",
        "del",
        "erase",
        "install",
        "ln",
        "md",
        "mkdir",
        "move",
        "mv",
        "patch",
        "rm",
        "rmdir",
        "tee",
        "touch",
        "truncate",
    }
)
_READ_ONLY_GIT_SUBCOMMANDS = frozenset(
    {
        "blame",
        "cat-file",
        "describe",
        "diff",
        "grep",
        "log",
        "ls-files",
        "rev-parse",
        "shortlog",
        "show",
        "status",
        "whatchanged",
    }
)
_MUTATING_GIT_SUBCOMMANDS = frozenset(
    {
        "add",
        "am",
        "apply",
        "checkout",
        "cherry-pick",
        "clean",
        "commit",
        "init",
        "merge",
        "mv",
        "pull",
        "push",
        "rebase",
        "reset",
        "restore",
        "revert",
        "rm",
        "stash",
        "switch",
    }
)
_TEST_RUNNER_PATTERN = re.compile(
    r"(?i)(?:^|\s)-m\s+(?:unittest|pytest|nose2)\b|(?:^|[\\/\s])pytest(?:\.exe)?\b"
)
# An unquoted redirection or an in-place `sed`/`perl` rewrite makes an otherwise
# read-only command a mutation. A file-descriptor prefix is part of the
# redirection, not a reason to ignore it: `cat a 1> b` and `cmd 2> err.txt` both
# write the workspace. Only `>&`, which duplicates a descriptor rather than
# opening a file, is excluded, and `(?!&)` alone is enough to do that.
_REDIRECTION_PATTERN = re.compile(r"(?<![<>])\d*>>?(?!&)")
_IN_PLACE_EDIT_PATTERN = re.compile(r"(?i)(?:^|\s)(?:sed|perl)\s+[^|;&]*-i\b")


_DIAGNOSTIC_PATTERNS: dict[str, tuple[tuple[str, str], ...]] = {
    "schema_rejection": (
        ("argument preflight failed", "argument preflight rejected the call"),
        ("not valid under any of the given schemas", "JSON Schema rejected the call"),
        ("schema rejection", "schema rejection was reported"),
    ),
    "freshness_invalidation": (
        ("stale_workspace_evidence", "workspace evidence was marked stale"),
        ("stale workspace evidence", "workspace evidence was marked stale"),
        ('"force_fresh":true', "the call requested a fresh rerun"),
        ('"force_fresh": true', "the call requested a fresh rerun"),
    ),
    "reuse_suppression": (
        ("unchanged failure", "an unchanged failure was suppressed"),
        ("unchanged-failure", "an unchanged failure was suppressed"),
        ("reuse suppression", "reuse suppression was reported"),
        ("negative cache", "negative-cache reuse was reported"),
    ),
    "unsupported_interactive_tool": (
        (
            "request_user_input is not supported in exec mode",
            "request_user_input was unsupported in exec mode",
        ),
        (
            "request_user_input is unavailable when approval policy is",
            "request_user_input was unavailable under the approval policy",
        ),
        ("unsupported interactive tool", "an interactive tool was unsupported"),
    ),
    "patch_mismatch": (
        ("apply_patch verification failed", "apply_patch verification failed"),
        (
            "failed to find expected lines",
            "a patch could not find its expected context",
        ),
        ("patch mismatch", "a patch mismatch was reported"),
    ),
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def text_sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def fixture_manifest() -> dict[str, str]:
    return {
        name: text_sha256(content) for name, content in sorted(FIXTURE_FILES.items())
    }


# Operator gitconfig must not reach the fixture repository. `core.autocrlf=true`
# in particular rewrites the worktree to CRLF whenever the agent runs an ordinary
# `git checkout`/`git stash`, which would fail the protected-file check on a run
# that never edited those files.
_FIXTURE_LOCAL_CONFIG = (
    ("core.autocrlf", "false"),
    ("core.eol", "lf"),
    ("core.safecrlf", "false"),
    ("core.hooksPath", "hooks-disabled"),
    ("core.fsmonitor", "false"),
    ("commit.gpgsign", "false"),
    ("gc.auto", "0"),
    ("user.name", "KD4 Benchmark"),
    ("user.email", "benchmark.invalid"),
)
_FIXTURE_GIT_CONFIG = tuple(f"{key}={value}" for key, value in _FIXTURE_LOCAL_CONFIG)

def without_git_environment() -> dict[str, str]:
    """Copy the process environment without Git's ambient control surface."""
    env = os.environ.copy()
    for name in tuple(env):
        # Windows environment keys are case-insensitive. Apply the same rule on
        # every platform so a mixed-case pointer cannot evade sanitization.
        if name.upper().startswith("GIT_"):
            env.pop(name, None)
    return env


def fixture_git_command(*args: str) -> list[str]:
    command = ["git"]
    for setting in _FIXTURE_GIT_CONFIG:
        command += ["-c", setting]
    command += args
    return command


def fixture_git_env() -> dict[str, str]:
    env = without_git_environment()
    env.update(
        {
            "GIT_AUTHOR_DATE": "2000-01-01T00:00:00Z",
            "GIT_COMMITTER_DATE": "2000-01-01T00:00:00Z",
            "GIT_AUTHOR_NAME": "KD4 Benchmark",
            "GIT_AUTHOR_EMAIL": "benchmark.invalid",
            "GIT_COMMITTER_NAME": "KD4 Benchmark",
            "GIT_COMMITTER_EMAIL": "benchmark.invalid",
            # Ignore global/system gitconfig so the fixture commit is a property
            # of the fixture alone rather than of the workstation.
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_SYSTEM": os.devnull,
        }
    )
    return env


def create_fixture(root: Path) -> str:
    for relative, content in FIXTURE_FILES.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="\n")
    env = fixture_git_env()
    with tempfile.TemporaryDirectory(
        prefix="kd4-empty-git-template-", dir=root.parent
    ) as empty_template:
        subprocess.run(
            fixture_git_command(
                "init", "-q", "-b", "main", f"--template={empty_template}"
            ),
            cwd=root,
            check=True,
            env=env,
        )
    # Persist the isolation settings because the agent invokes ordinary Git
    # commands without the harness's `-c` arguments or sanitized environment.
    for key, value in _FIXTURE_LOCAL_CONFIG:
        subprocess.run(
            fixture_git_command("config", "--local", key, value),
            cwd=root,
            check=True,
            env=env,
        )
    subprocess.run(fixture_git_command("add", "."), cwd=root, check=True, env=env)
    subprocess.run(
        fixture_git_command("commit", "-q", "-m", "benchmark fixture"),
        cwd=root,
        check=True,
        env=env,
    )
    return subprocess.check_output(
        fixture_git_command("rev-parse", "HEAD"),
        cwd=root,
        text=True,
        encoding="utf-8",
        env=env,
    ).strip()


def protected_fixture_failures(root: Path) -> list[str]:
    failures: list[str] = []
    for protected in (".gitignore", "README.md", "test_duration.py", "AGENTS.md"):
        try:
            # Universal-newline reads make CRLF/LF differences irrelevant while
            # preserving every substantive character in the protected file.
            actual = (root / protected).read_text(encoding="utf-8")
        except (OSError, UnicodeError):
            failures.append(f"{protected} was modified")
            continue
        if actual != FIXTURE_FILES[protected]:
            failures.append(f"{protected} was modified")
    return failures


# Byproducts of running the suite, which `.gitignore` already excludes and which
# the contract does not treat as authored files.
_IGNORED_WORKSPACE_NAMES = frozenset({".git", "__pycache__"})
_IGNORED_WORKSPACE_SUFFIXES = (".pyc", ".pyo")


def added_workspace_files(root: Path) -> list[str]:
    """Files present in the workspace that the fixture never created.

    The contract is `Modify only duration.py`, so a new module is a violation
    even though every protected file still matches. It also explains an
    otherwise cryptic verifier failure: the hidden verifier runs under `-I`, so
    the workspace is off `sys.path` and a `duration.py` importing a sibling the
    agent added fails there while the visible suite passes.
    """
    added: list[str] = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if any(part in _IGNORED_WORKSPACE_NAMES for part in relative.parts):
            continue
        if path.is_dir() or path.suffix in _IGNORED_WORKSPACE_SUFFIXES:
            continue
        if relative.as_posix() not in FIXTURE_FILES:
            added.append(relative.as_posix())
    return added


def verify_fixture(root: Path) -> tuple[bool, list[str]]:
    failures = protected_fixture_failures(root)
    failures.extend(
        f"{path} was added; only duration.py may be modified"
        for path in added_workspace_files(root)
    )

    verifier = f"""
import importlib.util
from pathlib import Path

path = Path({str(root / "duration.py")!r})
spec = importlib.util.spec_from_file_location('bench_duration', path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
parse_duration = module.parse_duration
valid = {VALID_CASES!r}
invalid = {INVALID_CASES!r}
for text, expected in valid.items():
    actual = parse_duration(text)
    assert type(actual) is int and actual == expected, (text, expected, actual)
for text in invalid:
    try:
        parse_duration(text)
    except ValueError:
        pass
    else:
        raise AssertionError(('expected ValueError', text))
"""
    env = os.environ.copy()
    env["PYTHONDONTWRITEBYTECODE"] = "1"

    def _verifier_run(label: str, command: list[str]) -> subprocess.CompletedProcess | None:
        """Run one post-turn check under a hard timeout.

        Both checks execute agent-authored code: the hidden verifier calls
        `parse_duration` directly and the visible suite imports it. A
        non-terminating parser is an ordinary failure mode for this task, and
        without a timeout it hangs the whole benchmark indefinitely — after the
        agent's own `--timeout-seconds` has already been enforced, so nothing
        else would ever recover it.
        """
        process = spawn_owned_process(
            command,
            cwd=root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
        )
        try:
            stdout, stderr = process.communicate(timeout=VERIFIER_TIMEOUT_SECONDS)
            return subprocess.CompletedProcess(
                command,
                process.returncode,
                stdout,
                stderr,
            )
        except subprocess.TimeoutExpired:
            failures.append(
                f"{label} did not finish within {VERIFIER_TIMEOUT_SECONDS}s"
            )
            return None
        finally:
            # `communicate()` kills only its direct child on timeout. A verifier
            # can import agent-authored code that spawns a descendant holding the
            # captured pipes, so sweep the entire isolated process tree on every
            # exit path.
            terminate_process(process)
            try:
                process.communicate(timeout=READER_JOIN_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                pass

    hidden = _verifier_run("external verifier", ["python", "-I", "-B", "-c", verifier])
    if hidden is not None and hidden.returncode != 0:
        tail = (hidden.stderr or hidden.stdout).strip().splitlines()[-1:]
        failures.append("external verifier failed" + (f": {tail[0]}" if tail else ""))

    visible = _verifier_run("visible tests", ["python", "-m", "unittest", "-q"])
    if visible is not None and visible.returncode != 0:
        failures.append("visible tests failed")
    return not failures, failures


def git_env() -> dict[str, str]:
    """Environment for provenance-sensitive Git commands.

    Git repository pointer variables override `-C` and can redirect a query to
    an unrelated checkout. Every benchmark Git invocation uses this sanitized
    environment rather than trusting ambient process state.
    """
    return without_git_environment()


def git_output(root: Path, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", str(root), *args],
        text=True,
        encoding="utf-8",
        env=git_env(),
    ).strip()


def git_bytes(root: Path, *args: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(root), *args], env=git_env())


def exact_source_state(
    root: Path, expected_revision: str, label: str
) -> dict[str, Any]:
    requested_root = root.resolve(strict=True)
    resolved_root = Path(
        git_output(requested_root, "rev-parse", "--show-toplevel")
    ).resolve(strict=True)
    if resolved_root != requested_root:
        raise RuntimeError(
            f"{label} Git top-level is {resolved_root}, expected requested repository "
            f"{requested_root}"
        )
    common_dir_text = git_output(
        requested_root, "rev-parse", "--path-format=absolute", "--git-common-dir"
    )
    common_dir = Path(common_dir_text).resolve(strict=True)
    if not common_dir.is_dir():
        raise RuntimeError(
            f"{label} Git common directory is not a directory: {common_dir}"
        )
    expected_commit = git_output(
        requested_root, "rev-parse", f"{expected_revision}^{{commit}}"
    )
    head_commit = git_output(requested_root, "rev-parse", "HEAD^{commit}")
    if head_commit != expected_commit:
        raise RuntimeError(
            f"{label} HEAD is {head_commit}, expected {expected_commit}; "
            "refusing to benchmark a different source revision"
        )
    status = git_bytes(
        requested_root, "status", "--porcelain=v1", "-z", "--untracked-files=all"
    )
    if status:
        dirty_paths = status.count(b"\0")
        raise RuntimeError(
            f"{label} source tree has {dirty_paths} dirty paths; commit or remove "
            "every change before building or benchmarking"
        )
    return {
        "commit": head_commit,
        "tree": git_output(requested_root, "rev-parse", "HEAD^{tree}"),
        "topLevel": str(resolved_root),
        "gitCommonDirectory": str(common_dir),
        "clean": True,
        "statusPorcelainV1zSha256": hashlib.sha256(status).hexdigest(),
        "exactContentsReconstructable": True,
    }


def binary_identity(
    path: Path,
    source_root: Path,
    source_state: dict[str, Any],
    label: str,
    *,
    build_command: str | None = None,
    sha256_before_runs: str | None = None,
) -> dict[str, Any]:
    stat = path.stat()
    # Recorded from the hash taken before the first run when the caller supplies
    # it, so the identity names the binary that actually produced the results.
    digest = sha256(path) if sha256_before_runs is None else sha256_before_runs
    identity = {
        "label": label,
        "sourceRoot": str(source_root),
        "revision": source_state["commit"],
        "sourceState": source_state,
        "binary": {
            "path": str(path),
            "sha256": digest,
            "sha256VerifiedBeforeAndAfterRuns": sha256_before_runs is not None,
            "sizeBytes": stat.st_size,
            "mtimeUtc": datetime.fromtimestamp(stat.st_mtime, timezone.utc).isoformat(),
        },
        "buildProfile": "release",
    }
    if build_command is not None:
        identity["recordedBuildCommand"] = build_command
    return identity


def require_binary_sha256(path: Path, expected: str, label: str) -> None:
    """Abort as soon as a benchmark binary differs from its initial bytes."""
    actual = sha256(path)
    if actual != expected:
        raise RuntimeError(
            f"{label} binary at {path} changed during the benchmark; the "
            "recorded results were not all produced by one binary"
        )


def prepare_home(root: Path, auth_source: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    shutil.copy2(auth_source, root / "auth.json")


def bounded_text_lines(stream: Any, max_chars: int):
    """Yield logical text lines without ever buffering more than one bounded chunk."""
    while True:
        fragment = stream.readline(max_chars + 1)
        if fragment == "":
            return
        terminated = fragment.endswith("\n")
        truncated = not terminated and len(fragment) > max_chars
        if truncated:
            # Discard the rest of this one logical line in bounded chunks. If we
            # yielded each chunk separately, one huge line could also defeat the
            # queue row limit and manufacture thousands of invalid JSONL events.
            while True:
                remainder = stream.readline(max_chars + 1)
                if remainder == "" or remainder.endswith("\n"):
                    break
        yield fragment[:max_chars].rstrip(), truncated


def process_group_options() -> dict[str, Any]:
    """Platform-specific Popen options that isolate one benchmark run."""
    if os.name == "nt":
        return {
            "creationflags": getattr(subprocess, "CREATE_NO_WINDOW", 0)
            | getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
            | getattr(subprocess, "CREATE_SUSPENDED", 0x00000004)
        }
    return {"start_new_session": True}


def _new_windows_job() -> Any:
    """Create a kill-on-close Job Object for one benchmark process tree."""
    import ctypes
    from ctypes import wintypes

    class BasicLimitInformation(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class IoCounters(ctypes.Structure):
        _fields_ = [
            ("ReadOperationCount", ctypes.c_ulonglong),
            ("WriteOperationCount", ctypes.c_ulonglong),
            ("OtherOperationCount", ctypes.c_ulonglong),
            ("ReadTransferCount", ctypes.c_ulonglong),
            ("WriteTransferCount", ctypes.c_ulonglong),
            ("OtherTransferCount", ctypes.c_ulonglong),
        ]

    class ExtendedLimitInformation(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", BasicLimitInformation),
            ("IoInfo", IoCounters),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    kernel32.SetInformationJobObject.argtypes = [
        wintypes.HANDLE,
        ctypes.c_int,
        ctypes.c_void_p,
        wintypes.DWORD,
    ]
    kernel32.SetInformationJobObject.restype = wintypes.BOOL
    kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
    kernel32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
    kernel32.TerminateJobObject.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL

    handle = kernel32.CreateJobObjectW(None, None)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    information = ExtendedLimitInformation()
    information.BasicLimitInformation.LimitFlags = 0x00002000
    if not kernel32.SetInformationJobObject(
        handle, 9, ctypes.byref(information), ctypes.sizeof(information)
    ):
        error = ctypes.WinError(ctypes.get_last_error())
        kernel32.CloseHandle(handle)
        raise error

    class WindowsJob:
        def __init__(self) -> None:
            self.handle = handle

        def attach_and_resume(self, process: subprocess.Popen[str]) -> None:
            if not kernel32.AssignProcessToJobObject(
                self.handle, wintypes.HANDLE(process._handle)  # type: ignore[attr-defined]
            ):
                raise ctypes.WinError(ctypes.get_last_error())
            _resume_windows_process(process.pid)

        def terminate(self) -> None:
            if self.handle and not kernel32.TerminateJobObject(self.handle, 1):
                raise ctypes.WinError(ctypes.get_last_error())

        def close(self) -> None:
            if self.handle:
                kernel32.CloseHandle(self.handle)
                self.handle = None

    return WindowsJob()


def _resume_windows_process(pid: int) -> None:
    """Resume every thread in a just-created suspended process."""
    import ctypes
    from ctypes import wintypes

    class ThreadEntry32(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD),
            ("cntUsage", wintypes.DWORD),
            ("th32ThreadID", wintypes.DWORD),
            ("th32OwnerProcessID", wintypes.DWORD),
            ("tpBasePri", wintypes.LONG),
            ("tpDeltaPri", wintypes.LONG),
            ("dwFlags", wintypes.DWORD),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    kernel32.Thread32First.argtypes = [wintypes.HANDLE, ctypes.POINTER(ThreadEntry32)]
    kernel32.Thread32First.restype = wintypes.BOOL
    kernel32.Thread32Next.argtypes = [wintypes.HANDLE, ctypes.POINTER(ThreadEntry32)]
    kernel32.Thread32Next.restype = wintypes.BOOL
    kernel32.OpenThread.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel32.OpenThread.restype = wintypes.HANDLE
    kernel32.ResumeThread.argtypes = [wintypes.HANDLE]
    kernel32.ResumeThread.restype = wintypes.DWORD
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL

    invalid_handle = ctypes.c_void_p(-1).value
    snapshot = kernel32.CreateToolhelp32Snapshot(0x00000004, 0)
    if not snapshot or snapshot == invalid_handle:
        raise ctypes.WinError(ctypes.get_last_error())
    resumed = 0
    try:
        entry = ThreadEntry32()
        entry.dwSize = ctypes.sizeof(entry)
        present = bool(kernel32.Thread32First(snapshot, ctypes.byref(entry)))
        while present:
            if entry.th32OwnerProcessID == pid:
                thread = kernel32.OpenThread(0x0002, False, entry.th32ThreadID)
                if not thread:
                    raise ctypes.WinError(ctypes.get_last_error())
                try:
                    if kernel32.ResumeThread(thread) == 0xFFFFFFFF:
                        raise ctypes.WinError(ctypes.get_last_error())
                    resumed += 1
                finally:
                    kernel32.CloseHandle(thread)
            present = bool(kernel32.Thread32Next(snapshot, ctypes.byref(entry)))
    finally:
        kernel32.CloseHandle(snapshot)
    if resumed == 0:
        raise RuntimeError(f"no thread found for suspended process {pid}")


def spawn_owned_process(command: list[str], **kwargs: Any) -> subprocess.Popen[str]:
    """Spawn a process whose complete descendant tree has a native owner."""
    options = process_group_options()
    if os.name != "nt":
        return subprocess.Popen(command, **kwargs, **options)

    owner = _new_windows_job()
    try:
        process = subprocess.Popen(command, **kwargs, **options)
    except BaseException:
        owner.close()
        raise
    setattr(process, "_kd4_owned_process", True)
    setattr(process, "_kd4_windows_job", owner)
    try:
        owner.attach_and_resume(process)
    except BaseException:
        try:
            process.kill()
            process.wait(timeout=5)
        finally:
            owner.close()
        raise
    return process


def _kill_process_tree(process: subprocess.Popen[str]) -> None:
    """Kill the agent and every process it spawned.

    Benchmark-owned processes keep a native process-group or Job handle, so the
    descendants remain addressable even after the root exits.  The Windows
    taskkill path is only a compatibility fallback for externally-created
    processes that do not carry that ownership metadata.
    """
    if os.name == "nt":
        owner = getattr(process, "_kd4_windows_job", None)
        if owner is not None:
            try:
                owner.terminate()
            except OSError:
                pass
            return
        if getattr(process, "_kd4_owned_process", False):
            return
        try:
            subprocess.run(
                ["taskkill", "/T", "/F", "/PID", str(process.pid)],
                capture_output=True,
                check=False,
                timeout=5,
            )
        except (OSError, subprocess.TimeoutExpired):
            pass
        return
    try:
        # `start_new_session=True` makes the root pid the process-group id. Use
        # that stable id directly: getpgid(root) stops working once the root has
        # exited even though descendants can still hold the group and its pipes.
        os.killpg(process.pid, signal.SIGKILL)
    except (OSError, ProcessLookupError):
        pass


def terminate_process(process: subprocess.Popen[str]) -> None:
    """Stop the agent and everything it started.

    `terminate()` alone reaches only the direct child. Under `--sandbox
    danger-full-access` the agent's own subprocesses would survive a timeout,
    leak into later repetitions, and keep the inherited stderr pipe open so the
    reader thread never observes EOF.
    """
    # Always sweep the group. On POSIX the root may have exited while a
    # descendant still owns an inherited pipe; poll() therefore cannot prove
    # that the tree is gone.
    try:
        _kill_process_tree(process)
        if process.poll() is not None:
            return
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
    finally:
        owner = getattr(process, "_kd4_windows_job", None)
        if owner is not None:
            owner.close()
            setattr(process, "_kd4_windows_job", None)


def build_agent_command(
    *,
    binary: Path,
    workspace: Path,
    model: str,
    reasoning_effort: str,
    personality: str,
    code_mode: str,
) -> list[str]:
    code_mode_enabled = "true" if code_mode == "enabled" else "false"
    return [
        str(binary),
        "exec",
        "--json",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        "--model",
        model,
        "-c",
        f'model_reasoning_effort="{reasoning_effort}"',
        "-c",
        f'personality="{personality}"',
        "-c",
        'approval_policy="never"',
        "-c",
        f"features.code_mode={code_mode_enabled}",
        "--sandbox",
        "danger-full-access",
        "-C",
        str(workspace),
        TASK_PROMPT,
    ]


def _tokenize(text: str) -> list[str]:
    return _TOKEN.findall(text)


def _unquote(token: str) -> str:
    if len(token) >= 2 and token[0] == token[-1] and token[0] in "\"'":
        return token[1:-1]
    return token


def shell_script_payloads(command: str) -> list[str]:
    """Return the command text plus the script it runs through a shell wrapper.

    `bash -lc "python -m unittest -q"` and `python -m unittest -q` describe the
    same execution, so every command predicate is evaluated against both forms.
    Nested wrappers are unwrapped until a non-wrapper leader is reached.
    """
    payloads = [command]
    current = command
    for _ in range(4):
        tokens = _tokenize(current)
        if len(tokens) < 3:
            break
        leader = _unquote(tokens[0])
        matched = next(
            (flags for pattern, flags in _SHELL_WRAPPERS if pattern.fullmatch(leader)),
            None,
        )
        if matched is None:
            break
        script_index = next(
            (
                index
                for index in range(1, len(tokens) - 1)
                if _unquote(tokens[index]).lower() in matched
            ),
            None,
        )
        if script_index is None:
            break
        script = " ".join(_unquote(token) for token in tokens[script_index + 1 :])
        if not script or script == current:
            break
        payloads.append(script)
        current = script
    return payloads


def _split_segments(payload: str) -> list[str]:
    return [
        stripped
        for stripped in (segment.strip() for segment in _SHELL_SEGMENT.split(payload))
        if stripped
    ]


def command_segments(command: str) -> list[str]:
    """Every shell segment of the command and of any wrapped script it runs."""
    segments: list[str] = []
    for payload in shell_script_payloads(command):
        segments.extend(_split_segments(payload))
    return segments


def innermost_command_segments(command: str) -> list[str]:
    """Shell segments of the innermost payload only.

    `command_is_mutating` and `is_required_test_command` ask whether *any*
    segment matches, so offering them the wrapper text as well as the script it
    runs is safe. Deciding `inspection` asks whether *every* segment is
    read-only, and a wrapper leader (`bash`, `pwsh`, `cmd`) appears in neither
    leader table, so feeding it both forms made every wrapped command `other`
    and let the wrapper choice change the classification. `codex exec` surfaces
    shell commands as `bash -lc <script>`, so that collapsed `inspection` to
    nearly zero in practice.
    """
    return _split_segments(shell_script_payloads(command)[-1])


def is_required_test_command(command: str) -> bool:
    return any(
        _REQUIRED_TEST_PATTERN.fullmatch(segment) is not None
        for segment in command_segments(command)
    )


def required_test_exit_code_reflects_suite(command: str) -> bool:
    """Whether exit code 0 can be read as the required suite passing.

    `python -m unittest -q || true` exits 0 however the suite behaved, so a
    matching segment proves a pass only when no exit-masking separator stands
    between it and the end of its payload. Detection stays separate: a masked
    invocation still counts as an observed attempt, it just cannot count as a
    passing one.
    """
    # Parsing general shell control flow is not a sound way to recover the exit
    # status of one nested process. Trust only an exact innermost invocation;
    # pipelines and commands before or after it are attempts, never proof of a
    # passing suite.
    payload = shell_script_payloads(command)[-1]
    return _REQUIRED_TEST_PATTERN.fullmatch(payload.strip()) is not None


def _segment_leader(segment: str) -> tuple[str, str | None]:
    """The invoked program and, for `git`, its subcommand, both lowercased."""
    tokens = [_unquote(token) for token in _tokenize(segment)]
    # Skip leading `NAME=value` environment assignments.
    index = 0
    while index < len(tokens) and re.fullmatch(
        r"[A-Za-z_][A-Za-z0-9_]*=\S*", tokens[index]
    ):
        index += 1
    if index >= len(tokens):
        return "", None
    leader = re.split(r"[\\/]", tokens[index])[-1].lower()
    leader = re.sub(r"\.(?:exe|cmd|bat|ps1)$", "", leader)
    subcommand = None
    if leader == "git":
        subcommand = next(
            (
                token.lower()
                for token in tokens[index + 1 :]
                if not token.startswith("-")
            ),
            None,
        )
    return leader, subcommand


def command_is_mutating(command: str) -> bool:
    """Whether any segment can write to the workspace.

    Conservative: an unrecognized program is not assumed to be read-only, but
    only the listed writers, redirections, and in-place edits assert a mutation.
    """
    for segment in command_segments(command):
        leader, subcommand = _segment_leader(segment)
        if leader in _MUTATING_LEADERS:
            return True
        if leader == "git" and subcommand in _MUTATING_GIT_SUBCOMMANDS:
            return True
        if _IN_PLACE_EDIT_PATTERN.search(segment):
            return True
        # Strip quoted spans before looking for a redirection so that `echo ">"`
        # is not read as one.
        unquoted = _TOKEN.sub(
            lambda match: "" if match.group(0)[:1] in "\"'" else match.group(0),
            segment,
        )
        if _REDIRECTION_PATTERN.search(unquoted):
            return True
    return False


def classify_command(command: str) -> str:
    """Single label for one observed command execution.

    Precedence is fixed so the label is reproducible: running the contract's
    required suite outranks running any other suite, which outranks writing,
    which outranks reading. An unrecognized program is reported as `other`
    rather than assigned to a category the text does not establish.
    """
    if is_required_test_command(command):
        return "required_test"
    segments = command_segments(command)
    if any(_TEST_RUNNER_PATTERN.search(segment) for segment in segments):
        return "test"
    if command_is_mutating(command):
        return "mutation"
    # `all()` over every payload form would count the shell wrapper itself as an
    # unrecognized program, so this one predicate reads the innermost script.
    leaders = [
        _segment_leader(segment) for segment in innermost_command_segments(command)
    ]
    if leaders and all(
        leader in _READ_ONLY_LEADERS
        or (leader == "git" and subcommand in _READ_ONLY_GIT_SUBCOMMANDS)
        for leader, subcommand in leaders
    ):
        return "inspection"
    return "other"


def turn_measurements(event: dict[str, Any]) -> tuple[float | None, int | None]:
    timing = event.get("timing")
    if not isinstance(timing, dict):
        return None, None
    unions = timing.get("unions")
    requests = timing.get("modelRequests")
    if not isinstance(unions, dict) or not isinstance(requests, list):
        return None, None
    model_wait_ns = unions.get("modelStreamWaitUnionNs")
    if not isinstance(model_wait_ns, int) or isinstance(model_wait_ns, bool):
        return None, None
    continuation_count = sum(
        request.get("isContinuation") is True
        for request in requests
        if isinstance(request, dict)
    )
    return round(model_wait_ns / 1_000_000, 3), continuation_count


def _is_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _rounded_delta_ms(later: Any, earlier: Any) -> float | None:
    if not _is_number(later) or not _is_number(earlier) or later < earlier:
        return None
    return round(float(later) - float(earlier), 3)


def _unix_ms_from_offset(started_at_unix_ms: Any, offset_ms: Any) -> float | None:
    if not _is_number(started_at_unix_ms) or not _is_number(offset_ms):
        return None
    return round(float(started_at_unix_ms) + float(offset_ms), 3)


def _absolute_timestamps(
    record: dict[str, Any],
    *,
    started_at_unix_ms: Any,
    offset_fields: tuple[tuple[str, str], ...],
) -> dict[str, float | None]:
    """Translate runtime offsets without replacing their lossless raw values."""
    return {
        absolute_name: _unix_ms_from_offset(started_at_unix_ms, record.get(offset_name))
        for offset_name, absolute_name in offset_fields
    }


def _command_text(value: Any) -> str:
    if isinstance(value, list):
        return " ".join(str(part) for part in value)
    return str(value)


def required_test_covers_final_workspace_state(
    last_successful_test_sequence: int | None,
    last_workspace_mutation_sequence: int | None,
) -> bool:
    """Whether an exact passing suite observed the final workspace state."""

    return last_successful_test_sequence is not None and (
        last_workspace_mutation_sequence is None
        or last_successful_test_sequence >= last_workspace_mutation_sequence
    )


def record_command_event(
    *,
    records: dict[str, dict[str, Any]],
    order: list[str],
    event_type: str,
    item: dict[str, Any],
    sequence: int,
    observed_ms: float,
    observed_at_unix_ms: float,
) -> dict[str, Any]:
    """Merge one JSONL command lifecycle event into a bounded command record."""

    item_id_value = item.get("id")
    item_id = str(item_id_value) if item_id_value is not None else None
    record_key = item_id or f"missing-item-id:{sequence}"
    command_present = "command" in item
    full_command = _command_text(item.get("command", ""))
    if record_key not in records:
        records[record_key] = {
            "itemId": item_id,
            "command": full_command[:MAX_COMMAND_TEXT_CHARS],
            "commandChars": len(full_command),
            "commandTruncated": len(full_command) > MAX_COMMAND_TEXT_CHARS,
            "commandSha256": text_sha256(full_command),
            "status": item.get("status"),
            "exitCode": item.get("exit_code"),
            "startedSequence": None,
            "startedObservedMs": None,
            "startedObservedAtUnixMs": None,
            "completedSequence": None,
            "completedObservedMs": None,
            "completedObservedAtUnixMs": None,
            "observedDurationMs": None,
            "requiredTest": False,
            "exitCodeReflectsSuite": None,
            "kind": "other",
            "mutating": False,
            "passed": False,
        }
        order.append(record_key)
    record = records[record_key]
    if command_present:
        record["command"] = full_command[:MAX_COMMAND_TEXT_CHARS]
        record["commandChars"] = len(full_command)
        record["commandTruncated"] = len(full_command) > MAX_COMMAND_TEXT_CHARS
        record["commandSha256"] = text_sha256(full_command)
        # Classification sees the complete transient value even though the
        # serialized record keeps only a bounded review prefix.
        record["requiredTest"] = is_required_test_command(full_command)
        record["exitCodeReflectsSuite"] = (
            required_test_exit_code_reflects_suite(full_command)
            if record["requiredTest"]
            else None
        )
        record["kind"] = classify_command(full_command)
        record["mutating"] = command_is_mutating(full_command)
    record["status"] = item.get("status", record["status"])
    record["exitCode"] = item.get("exit_code", record["exitCode"])
    if event_type == "item.started":
        record["startedSequence"] = sequence
        record["startedObservedMs"] = observed_ms
        record["startedObservedAtUnixMs"] = observed_at_unix_ms
    elif event_type == "item.completed":
        record["completedSequence"] = sequence
        record["completedObservedMs"] = observed_ms
        record["completedObservedAtUnixMs"] = observed_at_unix_ms
    record["observedDurationMs"] = _rounded_delta_ms(
        record["completedObservedMs"], record["startedObservedMs"]
    )
    record["passed"] = (
        record["completedSequence"] is not None
        and record["status"] in {None, "completed"}
        and record["exitCode"] == 0
        # A required-test pass additionally needs the suite's failure to be
        # able to reach this exit code; `... || true` exits 0 either way.
        and record["exitCodeReflectsSuite"] is not False
    )
    return record


def _tool_generation_index(tool_call: dict[str, Any]) -> int | None:
    generation_index = tool_call.get("generationIndex")
    if isinstance(generation_index, int) and not isinstance(generation_index, bool):
        return generation_index
    generation_id = tool_call.get("samplingGenerationId")
    if isinstance(generation_id, str):
        match = re.fullmatch(r"generation-(\d+)", generation_id)
        if match is not None:
            return int(match.group(1))
    return None


def _is_command_tool_call(tool_call: dict[str, Any]) -> bool:
    if (
        any(
            _is_number(tool_call.get(field))
            for field in (
                "processSpawnedAtMs",
                "processExitedAtMs",
            )
        )
        or tool_call.get("execCleanupStateObserved") is True
    ):
        return True
    tool_name = str(tool_call.get("toolName", "")).lower().replace("-", "_")
    return tool_name in _EXEC_TOOL_NAMES or tool_name.endswith("exec_command")


def _tool_completion_boundary(tool_call: dict[str, Any]) -> tuple[str | None, Any]:
    for field in (
        "outputModelVisibleAtMs",
        "deliveredAtMs",
        "outputCollectedAtMs",
        "processExitedAtMs",
        "handlerExitAtMs",
    ):
        value = tool_call.get(field)
        if _is_number(value):
            return field, value
    return None, None


def _tool_latency(
    tool_call: dict[str, Any], model_requests: list[dict[str, Any]]
) -> dict[str, Any]:
    boundary_name, completion_ms = _tool_completion_boundary(tool_call)
    resumed_ms = tool_call.get("modelResumedAtMs")
    next_request: dict[str, Any] | None = None
    if _is_number(completion_ms):
        dispatched_after_completion = [
            request
            for request in model_requests
            if isinstance(request, dict)
            and _is_number(request.get("dispatchMs"))
            and request["dispatchMs"] >= completion_ms
        ]
        if dispatched_after_completion:
            next_request = min(
                dispatched_after_completion,
                key=lambda request: request["dispatchMs"],
            )
    ready_ns = tool_call.get("readyToSampleToDispatchNs")
    return {
        "completionBoundary": boundary_name,
        "completionAtMs": completion_ms,
        "modelResumedAtMs": resumed_ms if _is_number(resumed_ms) else None,
        "completionToModelResumeMs": _rounded_delta_ms(resumed_ms, completion_ms),
        "nextRequestGenerationIndex": (
            next_request.get("generationIndex") if next_request is not None else None
        ),
        "nextRequestDispatchAtMs": (
            next_request.get("dispatchMs") if next_request is not None else None
        ),
        "completionToNextRequestDispatchMs": (
            _rounded_delta_ms(next_request.get("dispatchMs"), completion_ms)
            if next_request is not None
            else None
        ),
        "readyToSampleToDispatchMs": (
            round(ready_ns / 1_000_000, 3)
            if isinstance(ready_ns, int) and not isinstance(ready_ns, bool)
            else None
        ),
    }


def classify_model_request(
    request: dict[str, Any],
    *,
    classification_complete: bool,
    prior_successful_test: dict[str, Any] | None,
    linked_commands: list[dict[str, Any]],
    intervening_mutation: bool = False,
) -> dict[str, Any]:
    """Classify a request from observed runtime facts without claiming causality."""

    if request.get("isContinuation") is not True:
        return {
            "primary": "initial",
            "tags": [],
            "confidence": "observed",
            "basis": ["isContinuation=false"],
            "interpretation": None,
            "necessityCausallyEstablished": False,
        }

    purpose = request.get("generationPurpose")
    reason = request.get("generationReason")
    attempt_kind = request.get("attemptKind")
    progress_values = request.get("progressKinds")
    progress = (
        {value for value in progress_values if isinstance(value, str)}
        if isinstance(progress_values, list)
        else set()
    )
    tags: list[str] = []
    basis: list[str] = []
    if attempt_kind in _RETRY_ATTEMPT_KINDS:
        tags.append("retry")
        basis.append(f"attemptKind={attempt_kind}")
    if (
        purpose in _RECOVERY_PURPOSES
        or reason == "compaction"
        or "failure_observation" in progress
    ):
        tags.append("recovery")
        basis.append("recovery purpose, reason, or failure observation")
    if purpose in _VERIFICATION_PURPOSES or "validation_result" in progress:
        tags.append("verification")
        basis.append("validation purpose or result")
    non_progress = (
        request.get("unchangedRelevantState") is True
        and request.get("nextStructuredActionChanged") is False
    )
    if non_progress:
        tags.append("non_progress")
        basis.append(
            "unchangedRelevantState=true and nextStructuredActionChanged=false"
        )
    repeated_test = prior_successful_test is not None and any(
        command.get("requiredTest") is True for command in linked_commands
    )
    if repeated_test:
        tags.append("post_success_verification")
        basis.append(
            "a linked required-test command followed an earlier passing required test"
        )
        # Rerunning a suite after an edit is ordinary work; rerunning it when
        # nothing was written since it last passed is what makes it redundant.
        if not intervening_mutation:
            tags.append("no_intervening_mutation")
            basis.append(
                "no command that can write to the workspace ran between that pass "
                "and this request"
            )

    primary = next(
        (category for category in CONTINUATION_CLASS_PRECEDENCE if category in tags),
        "necessary",
    )
    if primary == "necessary":
        tags.append("necessary")
        if progress:
            basis.append("runtime recorded progress")
        elif request.get("nextStructuredActionChanged") is True:
            basis.append("runtime recorded a changed next structured action")
        else:
            basis.append(
                "no retry, recovery, verification, or non-progress predicate matched"
            )
    interpretation = (
        "redundant_verification"
        if repeated_test and not intervening_mutation and non_progress
        else "post_success_verification_after_mutation"
        if repeated_test and intervening_mutation
        else None
    )
    return {
        "primary": primary,
        "tags": tags,
        "confidence": (
            "observed"
            if primary != "necessary" or progress or classification_complete
            else "heuristic"
        ),
        "basis": basis,
        "interpretation": interpretation,
        # Even the `necessary` bucket is an observational residual; this field
        # prevents downstream consumers from turning it into a causal claim.
        "necessityCausallyEstablished": False,
    }


def _inline_command(text: Any, limit: int = 120) -> str:
    """Collapse a command to one readable line for a narrative sentence."""
    collapsed = " ".join(str(text).split())
    return collapsed if len(collapsed) <= limit else collapsed[: limit - 1] + "…"


def describe_model_request(
    request: dict[str, Any], *, linked_commands: list[dict[str, Any]]
) -> str:
    """One reviewable sentence of evidence for a single model round.

    Every clause restates a field already present on the record, so the sentence
    can be checked against the row it came from. It reports what was observed and
    when; it never asserts why the model chose the round.
    """
    classification = request.get("continuationClassification", {})
    head = f"Round {request.get('generationIndex')}"
    attributes = [f"class={classification.get('primary')}"]
    if request.get("generationPurpose"):
        attributes.append(f"purpose={request['generationPurpose']}")
    if request.get("generationReason"):
        attributes.append(f"reason={request['generationReason']}")
    if request.get("attemptKind") not in (None, "primary"):
        attributes.append(f"attempt={request['attemptKind']}")
    sentence = f"{head} ({', '.join(attributes)})"

    clauses: list[str] = []
    prior_test = request.get("priorSuccessfulRequiredTest")
    if prior_test:
        clauses.append(
            f"ran after `{_inline_command(prior_test.get('command'))}` had already "
            f"passed at {prior_test.get('completedAtMs')} ms"
        )
        clauses.append(
            "with an intervening workspace mutation"
            if request.get("interveningWorkspaceMutation")
            else "with no intervening workspace mutation"
        )
    if request.get("unchangedRelevantState") is True:
        clauses.append("saw unchanged relevant state")
    if request.get("nextStructuredActionChanged") is False:
        clauses.append("did not change the next structured action")
    progress_values = request.get("progressKinds")
    progress = (
        [value for value in progress_values if isinstance(value, str)]
        if isinstance(progress_values, list)
        else []
    )
    if progress:
        clauses.append("observed " + ", ".join(progress))
    wait_ns = request.get("modelStreamWaitNs")
    if isinstance(wait_ns, int) and not isinstance(wait_ns, bool):
        clauses.append(f"waited {round(wait_ns / 1_000_000, 3)} ms on the model")
    if linked_commands:
        rendered = ", ".join(
            f"`{_inline_command(command.get('command'))}` ({command.get('kind')})"
            for command in linked_commands[:4]
        )
        extra = (
            "" if len(linked_commands) <= 4 else f" and {len(linked_commands) - 4} more"
        )
        clauses.append(f"issued {len(linked_commands)} command(s): {rendered}{extra}")
    elif request.get("commandItemIds") == []:
        clauses.append("issued no linked command")
    interpretation = classification.get("interpretation")
    if interpretation == "redundant_verification":
        clauses.append(
            "which the trace marks as a redundant verification: an already-passing "
            "suite rerun over unchanged state"
        )
    if not clauses:
        clauses.append("recorded no further distinguishing observation")
    return f"{sentence} {'; '.join(clauses)}."


def summarize_turn_trace(trace: dict[str, Any]) -> dict[str, Any]:
    """Counts over the trace rows, kept next to the rows they came from."""
    requests = trace.get("modelRequests", [])
    classifications = [
        request.get("continuationClassification", {}) for request in requests
    ]
    wait_by_class: dict[str, float] = {}
    for request, classification in zip(requests, classifications):
        wait_ns = request.get("modelStreamWaitNs")
        if isinstance(wait_ns, int) and not isinstance(wait_ns, bool):
            primary = str(classification.get("primary"))
            wait_by_class[primary] = round(
                wait_by_class.get(primary, 0.0) + wait_ns / 1_000_000, 3
            )
    commands = trace.get("commands", [])
    latencies = [
        command["nextObservedAction"]["latencyMs"]
        for command in commands
        if isinstance(command.get("nextObservedAction"), dict)
        and command["nextObservedAction"].get("latencyMs") is not None
    ]
    runtime_latencies = [
        command["runtimeLatencyToNextAction"]["completionToNextRequestDispatchMs"]
        for command in commands
        if isinstance(command.get("runtimeLatencyToNextAction"), dict)
        and command["runtimeLatencyToNextAction"].get(
            "completionToNextRequestDispatchMs"
        )
        is not None
    ]
    return {
        "recordedRequests": len(requests),
        "continuationRequests": sum(
            request.get("isContinuation") is True for request in requests
        ),
        "byPrimaryClass": dict(
            sorted(
                Counter(
                    str(classification.get("primary"))
                    for classification in classifications
                ).items()
            )
        ),
        "byTag": dict(
            sorted(
                Counter(
                    tag
                    for classification in classifications
                    for tag in classification.get("tags", [])
                ).items()
            )
        ),
        "byPurpose": dict(
            sorted(
                Counter(
                    str(request.get("generationPurpose") or "unreported")
                    for request in requests
                ).items()
            )
        ),
        "byReason": dict(
            sorted(
                Counter(
                    str(request.get("generationReason") or "unreported")
                    for request in requests
                ).items()
            )
        ),
        "byDisposition": dict(
            sorted(
                Counter(
                    str(request.get("disposition") or "unreported")
                    for request in requests
                ).items()
            )
        ),
        "byInterpretation": dict(
            sorted(
                Counter(
                    str(classification.get("interpretation"))
                    for classification in classifications
                    if classification.get("interpretation") is not None
                ).items()
            )
        ),
        "modelWaitMsByPrimaryClass": dict(sorted(wait_by_class.items())),
        "commandKinds": dict(
            sorted(Counter(str(command.get("kind")) for command in commands).items())
        ),
        "mutatingCommands": sum(
            command.get("mutating") is True for command in commands
        ),
        "observedToolToNextActionMs": distribution(latencies),
        "runtimeToolToNextRequestDispatchMs": distribution(runtime_latencies),
    }


def truncate_turn_trace(trace: dict[str, Any]) -> dict[str, int]:
    """Cap the per-row lists a trace serializes, reporting every drop.

    `summarize_turn_trace` must run first: the counts it publishes are over the
    complete rows, so capping afterwards bounds the report without changing any
    reported aggregate. Command text is shortened in place because a single
    heredoc can carry an entire file.
    """
    overflow = {
        key: int(value)
        for key, value in trace.get("retentionOverflow", {}).items()
        if isinstance(value, int) and not isinstance(value, bool)
    }
    for key, cap in (
        ("modelRequests", MAX_RETAINED_MODEL_REQUESTS),
        ("toolCalls", MAX_RETAINED_TOOL_CALLS),
        ("commands", MAX_RETAINED_COMMANDS),
    ):
        rows = trace.get(key)
        if isinstance(rows, list) and len(rows) > cap:
            overflow[key] = overflow.get(key, 0) + len(rows) - cap
            trace[key] = rows[:cap]
        else:
            overflow.setdefault(key, 0)
    truncated_commands = 0
    for command in trace.get("commands", []):
        text = command.get("command")
        if isinstance(text, str) and len(text) > MAX_COMMAND_TEXT_CHARS:
            command["command"] = text[:MAX_COMMAND_TEXT_CHARS]
            command["commandTruncated"] = True
            truncated_commands += 1
        elif command.get("commandTruncated") is True:
            truncated_commands += 1
        elif isinstance(text, str):
            command.setdefault("commandTruncated", False)
    overflow["truncatedCommandTexts"] = truncated_commands
    return overflow


def attach_stream_evidence(
    trace: dict[str, Any], derived: dict[str, Any], *, command_count: int
) -> dict[str, Any]:
    """Add the harness-only reconstruction and the floors a censored run still proves.

    The build-side trace disappears entirely when a turn is killed before its
    terminal event. These fields are measured from this process's own read
    timestamps, so they remain present for a censored run and for a build that
    emits no timing block at all, which is what keeps the two variants
    comparable instead of silently dropping the censored side.
    """
    trace["streamDerived"] = derived
    censoring = trace.setdefault("censoring", {})
    # Directly counted on the stream, so the true value is at least this large
    # however the run ended.
    censoring["observedFloors"] = {
        "commandExecutionsAtLeast": command_count,
        "toolResultBoundariesAtLeast": len(derived["boundaries"]),
        "modelRoundsAtLeast": 1 if derived["approximateModelRounds"] else 0,
        "basis": "directly counted JSONL stream items",
    }
    # Deliberately kept out of the floors above. One generation can issue several
    # tool calls and a reasoning-only generation closes no boundary, so the round
    # count is not a bound in either direction; the idle sum omits the first
    # round's wait and includes host work between the result and the next item.
    censoring["approximations"] = {
        "approximateModelRounds": derived["approximateModelRounds"],
        "postToolIdleMsTotal": derived["postToolIdleMsTotal"],
        "basis": "harness-observed stream reconstruction",
    }
    return trace


def build_turn_trace(
    *,
    terminal_event: str | None,
    terminal_payload: dict[str, Any] | None,
    timed_out: bool,
    process_started_at_unix_ms: float,
    wall_clock_ms: float,
    turn_started_observed_ms: float | None,
    terminal_observed_ms: float | None,
    commands: list[dict[str, Any]],
    item_events: list[dict[str, Any]],
) -> dict[str, Any]:
    """Build a queryable request/tool/command trace and explicit censor record."""

    timing_value = (
        terminal_payload.get("timing") if isinstance(terminal_payload, dict) else None
    )
    timing = timing_value if isinstance(timing_value, dict) else None
    raw_requests = timing.get("modelRequests", []) if timing is not None else []
    raw_tool_calls = timing.get("toolCalls", []) if timing is not None else []
    # A timing block whose rows are `null` or any non-list must degrade to
    # `timing_unavailable` below, not raise mid-benchmark: `timing_valid`
    # already classifies that shape, so its rows simply read as empty here.
    valid_requests = [
        request
        for request in (raw_requests if isinstance(raw_requests, list) else [])
        if isinstance(request, dict)
    ]
    valid_tool_calls = [
        tool_call
        for tool_call in (raw_tool_calls if isinstance(raw_tool_calls, list) else [])
        if isinstance(tool_call, dict)
    ]
    retention_overflow = {
        "modelRequests": max(0, len(valid_requests) - MAX_RETAINED_MODEL_REQUESTS),
        "toolCalls": max(0, len(valid_tool_calls) - MAX_RETAINED_TOOL_CALLS),
        "commands": max(0, len(commands) - MAX_RETAINED_COMMANDS),
    }
    requests = valid_requests[:MAX_RETAINED_MODEL_REQUESTS]
    tool_calls = valid_tool_calls[:MAX_RETAINED_TOOL_CALLS]
    commands = commands[:MAX_RETAINED_COMMANDS]
    # The augmented rows below are the retained representation. Keeping the raw
    # arrays in `terminalTiming` as well would duplicate them and would bypass
    # every retention ceiling.
    terminal_timing = None
    if timing is not None:
        terminal_timing = {
            key: value
            for key, value in timing.items()
            if key not in {"modelRequests", "toolCalls"}
        }
        terminal_timing["retainedRows"] = {
            "modelRequests": "turnTrace.modelRequests",
            "toolCalls": "turnTrace.toolCalls",
        }
    runtime_started_at_unix_ms = (
        timing.get("startedAtUnixMs") if timing is not None else None
    )
    timing_valid = (
        timing is not None
        and isinstance(raw_requests, list)
        and isinstance(raw_tool_calls, list)
    )
    right_censored = terminal_event is None
    # A killed turn and a failed turn both end without a terminal timing record,
    # but for different reasons than a build that never emits one. Keeping the
    # three apart stops a censored run from being read as an uninstrumented one.
    if right_censored:
        status = "right_censored"
        timing_missing_reason = (
            "the agent process was killed at the timeout before any terminal turn event"
            if timed_out
            else "the process ended without emitting a terminal turn event"
        )
    elif timing_valid:
        status = "complete"
        timing_missing_reason = None
    elif terminal_event == "turn.failed":
        status = "terminal_failure_without_timing"
        timing_missing_reason = (
            "the turn reached `turn.failed`, which carries no timing block"
        )
    else:
        status = "timing_unavailable"
        timing_missing_reason = (
            "the terminal `timing` block is malformed: its model-request or "
            "tool-call rows are not lists"
            if timing is not None
            else "this build emits no `timing` block"
        )

    trace_commands = [dict(command) for command in commands]
    command_candidates = [
        (index, tool_call)
        for index, tool_call in enumerate(tool_calls)
        if _is_command_tool_call(tool_call)
    ]
    unmatched_candidate_indexes = {index for index, _ in command_candidates}
    unmatched_command_indexes = set(range(len(trace_commands)))
    command_to_tool: dict[int, tuple[int, str]] = {}
    chronological_fallback_reasons: list[str] = []

    # `codex exec` normally synthesizes JSONL item IDs, but retain an exact-ID
    # path for builds that expose a core identity now or in the future.
    for command_index, command in enumerate(trace_commands):
        item_id = command.get("itemId")
        if not isinstance(item_id, str):
            continue
        exact_matches = [
            candidate_index
            for candidate_index, tool_call in command_candidates
            if candidate_index in unmatched_candidate_indexes
            and item_id
            in {
                str(tool_call.get(key))
                for key in ("callId", "runtimeToolCallId", "executionId")
                if tool_call.get(key) is not None
            }
        ]
        if len(exact_matches) == 1:
            candidate_index = exact_matches[0]
            command_to_tool[command_index] = (candidate_index, "exact_identity")
            unmatched_candidate_indexes.remove(candidate_index)
            unmatched_command_indexes.remove(command_index)

    # Synthetic item IDs cannot be joined to core call IDs. Only correlate by
    # chronology when the remaining populations are complete and one-to-one;
    # never guess across an overflow, invalid runtime profile, missing command
    # completion, or extra command-like tool call.
    tool_call_timing_overflow = (
        timing.get("toolCallTimingOverflow", 0) if timing is not None else None
    )
    if timing is None:
        chronological_fallback_reasons.append("terminal timing is unavailable")
    elif timing.get("profileValid") is False:
        chronological_fallback_reasons.append("the runtime timing profile is invalid")
    if not isinstance(tool_call_timing_overflow, int) or isinstance(
        tool_call_timing_overflow, bool
    ):
        chronological_fallback_reasons.append(
            "toolCallTimingOverflow is missing or invalid"
        )
    elif tool_call_timing_overflow > 0:
        chronological_fallback_reasons.append(
            f"the runtime omitted {tool_call_timing_overflow} tool timing record(s)"
        )
    if retention_overflow["toolCalls"]:
        chronological_fallback_reasons.append(
            "the harness omitted "
            f"{retention_overflow['toolCalls']} tool timing record(s) from the trace"
        )
    incomplete_command_indexes = {
        index
        for index in unmatched_command_indexes
        if trace_commands[index].get("completedSequence") is None
    }
    if incomplete_command_indexes:
        chronological_fallback_reasons.append(
            f"{len(incomplete_command_indexes)} command item(s) lack completion events"
        )
    if len(unmatched_command_indexes) != len(unmatched_candidate_indexes):
        chronological_fallback_reasons.append(
            "unmatched command and command-tool populations are not one-to-one"
        )

    chronological_fallback_used = False
    if unmatched_command_indexes and not chronological_fallback_reasons:
        ordered_commands = sorted(
            unmatched_command_indexes,
            key=lambda index: (
                trace_commands[index].get("startedSequence")
                if trace_commands[index].get("startedSequence") is not None
                else trace_commands[index].get("completedSequence", 1 << 60)
            ),
        )

        def tool_order(index: int) -> tuple[float, int]:
            tool_call = tool_calls[index]
            for field in ("processSpawnedAtMs", "handlerEntryAtMs", "acceptedAtMs"):
                value = tool_call.get(field)
                if _is_number(value):
                    return float(value), index
            return float("inf"), index

        ordered_candidates = sorted(unmatched_candidate_indexes, key=tool_order)
        for command_index, candidate_index in zip(
            ordered_commands, ordered_candidates, strict=True
        ):
            command_to_tool[command_index] = (
                candidate_index,
                "one_to_one_chronological",
            )
            unmatched_command_indexes.remove(command_index)
            unmatched_candidate_indexes.remove(candidate_index)
        chronological_fallback_used = True

    request_ids_by_generation: dict[int, str | None] = {}
    for request in requests:
        generation_index = request.get("generationIndex")
        if isinstance(generation_index, int) and not isinstance(generation_index, bool):
            request_ids_by_generation.setdefault(
                generation_index,
                request.get("samplingRequestId"),
            )

    augmented_tool_calls: list[dict[str, Any]] = []
    command_index_by_tool: dict[int, int] = {
        tool_index: command_index
        for command_index, (tool_index, _) in command_to_tool.items()
    }
    for tool_index, tool_call in enumerate(tool_calls):
        augmented = dict(tool_call)
        augmented["latencyToNextAction"] = _tool_latency(tool_call, requests)
        augmented["absoluteTimestamps"] = _absolute_timestamps(
            tool_call,
            started_at_unix_ms=runtime_started_at_unix_ms,
            offset_fields=(
                ("acceptedAtMs", "acceptedAtUnixMs"),
                ("firstPollAtMs", "firstPollAtUnixMs"),
                ("parallelGateAdmittedAtMs", "parallelGateAdmittedAtUnixMs"),
                ("handlerEntryAtMs", "handlerEntryAtUnixMs"),
                ("handlerExitAtMs", "handlerExitAtUnixMs"),
                ("outputCollectedAtMs", "outputCollectedAtUnixMs"),
                ("processSpawnedAtMs", "processSpawnedAtUnixMs"),
                ("processExitedAtMs", "processExitedAtUnixMs"),
                ("deliveredAtMs", "deliveredAtUnixMs"),
                ("outputModelVisibleAtMs", "outputModelVisibleAtUnixMs"),
                ("modelResumedAtMs", "modelResumedAtUnixMs"),
            ),
        )
        command_index = command_index_by_tool.get(tool_index)
        augmented["commandItemId"] = (
            trace_commands[command_index].get("itemId")
            if command_index is not None
            else None
        )
        augmented_tool_calls.append(augmented)

    item_events_by_sequence = sorted(
        item_events,
        key=lambda item_event: item_event.get("sequence", 1 << 60),
    )
    for command_index, command in enumerate(trace_commands):
        tool_link = command_to_tool.get(command_index)
        if tool_link is None:
            command["toolLink"] = {
                "status": "unlinked",
                "method": None,
                "timingCallId": None,
                "generationIndex": None,
                "reason": "; ".join(chronological_fallback_reasons)
                or "no matching command-producing tool timing record",
            }
            command["requestLink"] = {
                "status": "unlinked",
                "generationIndex": None,
                "samplingRequestId": None,
            }
        else:
            tool_index, method = tool_link
            tool_call = tool_calls[tool_index]
            generation_index = _tool_generation_index(tool_call)
            request_present = generation_index in request_ids_by_generation
            command["toolLink"] = {
                "status": "linked",
                "method": method,
                "timingCallId": tool_call.get("callId"),
                "executionId": tool_call.get("executionId"),
                "samplingGenerationId": tool_call.get("samplingGenerationId"),
                "generationIndex": generation_index,
            }
            command["requestLink"] = {
                "status": "linked" if request_present else "request_missing",
                "generationIndex": generation_index,
                "samplingRequestId": request_ids_by_generation.get(generation_index),
                "viaToolCallId": tool_call.get("callId"),
                "commandToToolMethod": method,
            }
            command["runtimeLatencyToNextAction"] = _tool_latency(tool_call, requests)

        completed_sequence = command.get("completedSequence")
        next_item = next(
            (
                item_event
                for item_event in item_events_by_sequence
                if completed_sequence is not None
                and item_event.get("sequence", -1) > completed_sequence
                and item_event.get("eventType") in {"item.started", "item.completed"}
            ),
            None,
        )
        command["nextObservedAction"] = (
            None
            if next_item is None
            else {
                "eventType": next_item.get("eventType"),
                "itemId": next_item.get("itemId"),
                "itemType": next_item.get("itemType"),
                "sequence": next_item.get("sequence"),
                "observedMs": next_item.get("observedMs"),
                "observedAtUnixMs": next_item.get("observedAtUnixMs"),
                "latencyMs": _rounded_delta_ms(
                    next_item.get("observedMs"), command.get("completedObservedMs")
                ),
            }
        )

    commands_by_generation: dict[int, list[dict[str, Any]]] = {}
    for command in trace_commands:
        generation_index = command.get("requestLink", {}).get("generationIndex")
        if isinstance(generation_index, int) and not isinstance(generation_index, bool):
            commands_by_generation.setdefault(generation_index, []).append(command)
    tools_by_generation: dict[int, list[dict[str, Any]]] = {}
    for tool_call in augmented_tool_calls:
        generation_index = _tool_generation_index(tool_call)
        if generation_index is not None:
            tools_by_generation.setdefault(generation_index, []).append(tool_call)

    successful_tests: list[tuple[float, dict[str, Any]]] = []
    for command in trace_commands:
        latency = command.get("runtimeLatencyToNextAction", {})
        completion_at_ms = latency.get("completionAtMs")
        if (
            command.get("requiredTest") is True
            and command.get("passed") is True
            and _is_number(completion_at_ms)
        ):
            successful_tests.append((float(completion_at_ms), command))

    # Runtime-clock completion times of everything that could have written to the
    # workspace: a shell command the text shows as mutating, and any patch-style
    # tool call, which is how an edit normally arrives.
    mutation_times_ms: list[float] = []
    for command in trace_commands:
        completion_at_ms = command.get("runtimeLatencyToNextAction", {}).get(
            "completionAtMs"
        )
        if command.get("mutating") is True and _is_number(completion_at_ms):
            mutation_times_ms.append(float(completion_at_ms))
    for tool_call in tool_calls:
        tool_name = str(tool_call.get("toolName", "")).lower().replace("-", "_")
        if tool_name not in _MUTATING_TOOL_NAMES:
            continue
        _, completion_at_ms = _tool_completion_boundary(tool_call)
        if _is_number(completion_at_ms):
            mutation_times_ms.append(float(completion_at_ms))
    # A completed JSONL `file_change` is the direct runtime observation that an
    # edit happened. Convert its harness Unix timestamp into the timing ledger's
    # offset before comparing it with request dispatches.
    if _is_number(runtime_started_at_unix_ms):
        for item_event in item_events_by_sequence:
            if (
                item_event.get("eventType") == "item.completed"
                and item_event.get("itemType") == "file_change"
                and _is_number(item_event.get("observedAtUnixMs"))
            ):
                mutation_times_ms.append(
                    float(item_event["observedAtUnixMs"])
                    - float(runtime_started_at_unix_ms)
                )

    augmented_requests: list[dict[str, Any]] = []
    classification_complete = (
        timing is not None and timing.get("classificationComplete") is True
    )
    for request in requests:
        augmented = dict(request)
        augmented["absoluteTimestamps"] = _absolute_timestamps(
            request,
            started_at_unix_ms=runtime_started_at_unix_ms,
            offset_fields=(
                ("dispatchMs", "dispatchAtUnixMs"),
                ("firstModelOutputMs", "firstModelOutputAtUnixMs"),
                ("firstActionableOutputMs", "firstActionableOutputAtUnixMs"),
                ("completedMs", "completedAtUnixMs"),
            ),
        )
        generation_index = request.get("generationIndex")
        linked_commands = (
            commands_by_generation.get(generation_index, [])
            if isinstance(generation_index, int)
            else []
        )
        linked_tools = (
            tools_by_generation.get(generation_index, [])
            if isinstance(generation_index, int)
            else []
        )
        dispatch_ms = request.get("dispatchMs")
        prior_tests = [
            (completed_at_ms, command)
            for completed_at_ms, command in successful_tests
            if _is_number(dispatch_ms) and completed_at_ms <= dispatch_ms
        ]
        prior_successful_test = None
        if prior_tests:
            completed_at_ms, command = max(prior_tests, key=lambda pair: pair[0])
            prior_successful_test = {
                "itemId": command.get("itemId"),
                "command": command.get("command"),
                "generationIndex": command.get("requestLink", {}).get(
                    "generationIndex"
                ),
                "completedAtMs": completed_at_ms,
            }
        augmented["toolCallIds"] = [
            tool_call.get("callId") for tool_call in linked_tools
        ]
        augmented["commandItemIds"] = [
            command.get("itemId") for command in linked_commands
        ]
        augmented["priorSuccessfulRequiredTest"] = prior_successful_test
        upper_bound_ms = dispatch_ms if _is_number(dispatch_ms) else float("inf")
        intervening_mutation = prior_successful_test is not None and any(
            prior_successful_test["completedAtMs"] < mutation_at_ms <= upper_bound_ms
            for mutation_at_ms in mutation_times_ms
        )
        augmented["interveningWorkspaceMutation"] = intervening_mutation
        augmented["continuationClassification"] = classify_model_request(
            request,
            classification_complete=classification_complete,
            prior_successful_test=prior_successful_test,
            linked_commands=linked_commands,
            intervening_mutation=intervening_mutation,
        )
        augmented["narrative"] = describe_model_request(
            augmented, linked_commands=linked_commands
        )
        augmented_requests.append(augmented)

    logical_generations: list[dict[str, Any]] = []
    generation_indexes = sorted(
        {
            request.get("generationIndex")
            for request in requests
            if isinstance(request.get("generationIndex"), int)
            and not isinstance(request.get("generationIndex"), bool)
        }
        | set(tools_by_generation)
        | set(commands_by_generation)
    )
    for generation_index in generation_indexes:
        generation_requests = [
            request
            for request in requests
            if request.get("generationIndex") == generation_index
        ]
        logical_generations.append(
            {
                "generationIndex": generation_index,
                "samplingRequestIds": list(
                    dict.fromkeys(
                        request.get("samplingRequestId")
                        for request in generation_requests
                        if request.get("samplingRequestId") is not None
                    )
                ),
                "physicalAttemptIds": [
                    attempt_id
                    for request in generation_requests
                    for attempt_id in (
                        request.get("physicalAttemptIds")
                        if isinstance(request.get("physicalAttemptIds"), list)
                        else []
                    )
                ],
                "toolCallIds": [
                    tool_call.get("callId")
                    for tool_call in tools_by_generation.get(generation_index, [])
                ],
                "commandItemIds": [
                    command.get("itemId")
                    for command in commands_by_generation.get(generation_index, [])
                ],
            }
        )

    linkage_methods = Counter(
        command.get("toolLink", {}).get("method") or "unlinked"
        for command in trace_commands
    )
    return {
        "schemaVersion": TURN_TRACE_SCHEMA_VERSION,
        "status": status,
        "terminalEvent": terminal_event,
        "processStartedAtUnixMs": process_started_at_unix_ms,
        "turnStartedObservedMs": turn_started_observed_ms,
        "turnStartedObservedAtUnixMs": _unix_ms_from_offset(
            process_started_at_unix_ms, turn_started_observed_ms
        ),
        "terminalObservedMs": terminal_observed_ms,
        "terminalObservedAtUnixMs": _unix_ms_from_offset(
            process_started_at_unix_ms, terminal_observed_ms
        ),
        "terminalTiming": terminal_timing,
        "censoring": {
            "rightCensored": right_censored,
            "reason": (
                "timeout"
                if right_censored and timed_out
                else "process_ended_without_terminal_event"
                if right_censored
                else None
            ),
            "timingMissingReason": timing_missing_reason,
            "observedThroughMs": wall_clock_ms,
            "observedThroughUnixMs": _unix_ms_from_offset(
                process_started_at_unix_ms, wall_clock_ms
            ),
            "terminalRecordObserved": terminal_event is not None,
            "terminalTimingObserved": timing is not None,
        },
        "modelRequests": augmented_requests,
        "logicalGenerations": logical_generations,
        "toolCalls": augmented_tool_calls,
        "commands": trace_commands,
        "itemEvents": item_events,
        "retentionOverflow": retention_overflow,
        "linkage": {
            "commandToToolMethods": dict(sorted(linkage_methods.items())),
            "chronologicalFallback": {
                "used": chronological_fallback_used,
                "eligible": not chronological_fallback_reasons,
                "disabledReasons": chronological_fallback_reasons,
                "runtimeToolCallTimingOverflow": tool_call_timing_overflow,
                "note": (
                    "`codex exec --json` currently emits synthetic item IDs. "
                    "Ordinal linkage is derived, never exact, and is used only "
                    "for complete one-to-one populations with no runtime ledger "
                    "overflow."
                ),
            },
            "unmatchedCommandItemIds": [
                trace_commands[index].get("itemId")
                for index in sorted(unmatched_command_indexes)
            ],
            "unmatchedTimingCallIds": [
                tool_calls[index].get("callId")
                for index in sorted(unmatched_candidate_indexes)
            ],
        },
        "causalScope": {
            "requestToolLineage": "runtime generation identity",
            "commandToolLineage": (
                "exact identity when exposed; otherwise explicitly marked one-to-one chronology"
            ),
            "jsonlCommandItemIdentity": (
                "synthetic in the current exec JSONL contract; see each toolLink.method"
            ),
            "guidanceRuleAttribution": "not_observed",
            "statisticalCausality": "not_established",
            "note": (
                "The trace supports within-run identity and temporal claims. It does not "
                "prove that a named guidance rule caused stochastic model behavior."
            ),
        },
    }


def elapsed_measurements(
    *, started_ns: int, ended_ns: int, terminal_ns: int | None
) -> tuple[float | None, float]:
    completion_ms = (
        None
        if terminal_ns is None
        else round((terminal_ns - started_ns) / 1_000_000, 3)
    )
    wall_clock_ms = round((ended_ns - started_ns) / 1_000_000, 3)
    return completion_ms, wall_clock_ms


def stream_derived_rounds(events: list[dict[str, Any]]) -> dict[str, Any]:
    """Reconstruct model rounds from the harness's own view of the JSONL stream.

    This uses only timestamps this process took as it read each line, so it is
    available for a build that emits no `timing` block and for a run that was
    killed before any terminal event. A round closes when a tool item completes;
    `postToolIdleMs` is the observed gap from that completion to the next stream
    event, which is the harness-side proxy for the model round it triggered.

    These are approximations of, not substitutes for, the build's own records:
    the harness cannot see provider retries, and a round that emitted nothing
    before the process was killed leaves no boundary at all.
    """
    boundaries: list[dict[str, Any]] = []
    pre_first_output_ms: float | None = None
    round_index = 0
    pending: dict[str, Any] | None = None
    for event in events:
        if pre_first_output_ms is None and event["eventType"] in {
            "item.started",
            "item.completed",
        }:
            pre_first_output_ms = event["offsetMs"]
        if pending is not None:
            pending["postToolIdleMs"] = round(
                event["offsetMs"] - pending["closedAtMs"], 3
            )
            pending["nextEventType"] = event["eventType"]
            pending["nextItemType"] = event["itemType"]
            boundaries.append(pending)
            pending = None
        if (
            event["eventType"] == "item.completed"
            and event["itemType"] in _TOOL_ITEM_TYPES
        ):
            pending = {
                "roundIndex": round_index,
                "closedByItemId": event["itemId"],
                "closedByItemType": event["itemType"],
                "closedAtMs": event["offsetMs"],
                "postToolIdleMs": None,
                "nextEventType": None,
                "nextItemType": None,
            }
            round_index += 1
    if pending is not None:
        # The process ended (or was killed) before anything followed this tool
        # result, so the round it would have triggered is right-censored.
        pending["censored"] = True
        boundaries.append(pending)
    for boundary in boundaries:
        boundary.setdefault("censored", False)
    idle = [
        boundary["postToolIdleMs"]
        for boundary in boundaries
        if boundary["postToolIdleMs"] is not None
    ]
    closed_rounds = len(idle)
    return {
        "boundaries": boundaries,
        # An approximation, not a bound in either direction: one generation can
        # issue several tool calls, and a generation that only reasons closes no
        # boundary at all.
        "approximateModelRounds": (closed_rounds + 1) if events else 0,
        "closedRounds": closed_rounds,
        "censoredRounds": sum(boundary["censored"] for boundary in boundaries),
        "preFirstOutputMs": pre_first_output_ms,
        "postToolIdleMsTotal": round(sum(idle), 3) if idle else None,
        "postToolIdleMs": distribution(idle),
        "note": (
            "Derived from this harness's read timestamps only. Comparable across "
            "variants because it does not depend on build-side instrumentation, "
            "and it survives a killed run, but it cannot see provider retries or "
            "a round that produced no stream item. `postToolIdleMs` brackets the "
            "model wait it triggered: it excludes the first round's wait and "
            "includes any host work between the tool result and the next item, "
            "so it is an approximation rather than a bound."
        ),
    }


def classify_diagnostics(
    *,
    observed_text: str,
    timed_out: bool,
    exit_code: int,
    terminal_event: str | None,
    invalid_json_lines: int,
    verifier_passed: bool,
    required_test_passed: bool,
    command_execution_failures: int,
) -> list[dict[str, Any]]:
    lowered = observed_text.lower()
    diagnostics: list[dict[str, Any]] = []
    for category, patterns in _DIAGNOSTIC_PATTERNS.items():
        signals = [signal for needle, signal in patterns if needle in lowered]
        if signals:
            diagnostics.append({"category": category, "signals": sorted(set(signals))})

    execution_signals: list[str] = []
    if command_execution_failures:
        execution_signals.append(
            f"{command_execution_failures} command execution(s) did not complete successfully"
        )
    if timed_out:
        execution_signals.append("the agent process timed out")
    if exit_code != 0:
        execution_signals.append(f"the agent process exited with code {exit_code}")
    if terminal_event != "turn.completed":
        execution_signals.append(f"terminal event was {terminal_event!r}")
    if execution_signals:
        diagnostics.append(
            {"category": "execution_failure", "signals": execution_signals}
        )
    if not verifier_passed:
        diagnostics.append(
            {
                "category": "verifier_failure",
                "signals": ["the post-turn visible or hidden verifier failed"],
            }
        )
    if not required_test_passed:
        diagnostics.append(
            {
                "category": "task_contract_violation",
                "signals": [
                    f"no successful `{REQUIRED_TEST_COMMAND}` attempt was observed"
                ],
            }
        )
    if invalid_json_lines:
        diagnostics.append(
            {
                "category": "invalid_event_stream",
                "signals": [f"{invalid_json_lines} JSONL line(s) were invalid"],
            }
        )
    return diagnostics


def _run_agent_impl(
    *,
    binary: Path,
    label: str,
    repetition: int,
    model: str,
    reasoning_effort: str,
    personality: str,
    code_mode: str,
    auth_source: Path,
    timeout_seconds: int,
    _process_holder: list[subprocess.Popen[str]],
) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix=f"kd4-live-{label}-{repetition}-") as temp:
        temp_root = Path(temp)
        workspace = temp_root / "workspace"
        workspace.mkdir()
        fixture_revision = create_fixture(workspace)
        home = temp_root / "home"
        prepare_home(home, auth_source)

        command = build_agent_command(
            binary=binary,
            workspace=workspace,
            model=model,
            reasoning_effort=reasoning_effort,
            personality=personality,
            code_mode=code_mode,
        )
        env = os.environ.copy()
        env["CODEX_HOME"] = str(home)
        env["RUST_LOG"] = "error"
        env["NO_COLOR"] = "1"
        # One wall-clock reading paired with the monotonic origin. Every absolute
        # timestamp below is this origin plus a monotonic offset, so the record
        # never mixes two clocks that can drift apart mid-run.
        started_ns = time.perf_counter_ns()
        process_started_at_unix_ms = round(time.time() * 1_000, 3)
        process = spawn_owned_process(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
            env=env,
        )
        _process_holder.append(process)
        assert process.stdout and process.stderr
        stdout_queue: queue.Queue[tuple[int, str] | None] = queue.Queue(
            maxsize=MAX_QUEUED_STDOUT_LINES
        )
        stderr_lines: list[str] = []
        stderr_line_overflow = 0
        stdout_truncated_lines = 0

        def read_stdout() -> None:
            nonlocal stdout_truncated_lines
            assert process.stdout
            for line, truncated in bounded_text_lines(
                process.stdout, MAX_STREAM_LINE_CHARS
            ):
                if truncated:
                    stdout_truncated_lines += 1
                stdout_queue.put((time.perf_counter_ns(), line))
            stdout_queue.put(None)

        def read_stderr() -> None:
            nonlocal stderr_line_overflow
            assert process.stderr
            for line, truncated in bounded_text_lines(
                process.stderr, MAX_COMMAND_TEXT_CHARS
            ):
                if truncated:
                    stderr_line_overflow += 1
                if len(stderr_lines) == MAX_STDERR_LINES:
                    # Evict the oldest line: fatal errors print last, and the
                    # serialized stderr tail must hold the true tail.
                    del stderr_lines[0]
                    stderr_line_overflow += 1
                stderr_lines.append(line)

        # Daemon threads: a leaked grandchild can hold the inherited pipe open,
        # and a reader blocked on that pipe must not keep the interpreter alive.
        stdout_thread = threading.Thread(target=read_stdout, daemon=True)
        stderr_thread = threading.Thread(target=read_stderr, daemon=True)
        stdout_thread.start()
        stderr_thread.start()

        deadline = time.monotonic() + timeout_seconds
        event_counts: Counter[str] = Counter()
        item_counts: Counter[str] = Counter()
        invalid_json_lines = 0
        first_output_ns: int | None = None
        terminal_ns: int | None = None
        terminal_event: str | None = None
        final_message: str | None = None
        timed_out = False
        # Newest-line retention: on overflow the deque evicts its oldest entry,
        # because failure needles land at the end of a long transcript.
        observed_text_parts: deque[str] = deque(maxlen=MAX_DIAGNOSTIC_TEXT_LINES)
        diagnostic_text_overflow = 0
        required_test_attempts: list[dict[str, Any]] = []
        required_test_attempt_overflow = 0
        last_successful_required_test_sequence: int | None = None
        last_workspace_mutation_sequence: int | None = None
        command_execution_failures = 0
        actual_command_count = 0
        command_kind_counts: Counter[str] = Counter()
        mutating_command_count = 0
        model_wait_ms: float | None = None
        continuation_count: int | None = None
        stream_events: list[dict[str, Any]] = []
        stream_event_overflow = 0
        # Bounded per-command lifecycle records, keyed by JSONL item id and kept
        # in first-seen order so the trace can join them to the timing ledger.
        command_records: dict[str, dict[str, Any]] = {}
        command_order: list[str] = []
        command_record_overflow = 0
        item_events: list[dict[str, Any]] = []
        item_event_overflow = 0
        event_sequence = 0
        turn_started_observed_ms: float | None = None
        terminal_payload: dict[str, Any] | None = None

        draining = False
        drain_deadline: float | None = None
        while True:
            if draining:
                remaining = 0.0
            else:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    # Stop the agent, then keep consuming lines the reader thread
                    # already captured so the timed-out run still reports its
                    # events and diagnostics.
                    timed_out = True
                    terminate_process(process)
                    draining = True
                    drain_deadline = time.monotonic() + READER_JOIN_TIMEOUT_SECONDS
            try:
                if draining:
                    queued = stdout_queue.get(timeout=0.05)
                else:
                    queued = stdout_queue.get(timeout=min(0.25, remaining))
            except queue.Empty:
                if draining:
                    if (
                        not stdout_thread.is_alive()
                        or drain_deadline is None
                        or time.monotonic() >= drain_deadline
                    ):
                        break
                    continue
                if process.poll() is not None:
                    # The process is gone, but the reader can still be flushing
                    # its final buffered lines: between this `Empty` and an
                    # `is_alive()` check it may enqueue the terminal event and
                    # exit. Breaking on that check would drop those lines, so
                    # switch to the bounded drain instead.
                    draining = True
                    drain_deadline = time.monotonic() + READER_JOIN_TIMEOUT_SECONDS
                continue
            if queued is None:
                break
            observed_ns, line = queued
            if not line:
                continue
            if len(observed_text_parts) == MAX_DIAGNOSTIC_TEXT_LINES:
                diagnostic_text_overflow += 1
            observed_text_parts.append(line[:20_000])
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                invalid_json_lines += 1
                continue
            if not isinstance(event, dict):
                invalid_json_lines += 1
                continue
            event_type = str(event.get("type", "unknown"))
            event_counts[event_type] += 1
            item_value = event.get("item")
            item = item_value if isinstance(item_value, dict) else {}
            item_type = item.get("type")
            item_id = item.get("id") if isinstance(item.get("id"), str) else None
            if item_type:
                item_counts[str(item_type)] += 1
            event_sequence += 1
            offset_ms = round((observed_ns - started_ns) / 1_000_000, 3)
            if len(stream_events) < MAX_RETAINED_STREAM_EVENTS:
                stream_events.append(
                    {
                        "sequence": event_sequence,
                        "offsetMs": offset_ms,
                        "observedAtUnixMs": _unix_ms_from_offset(
                            process_started_at_unix_ms, offset_ms
                        ),
                        "eventType": event_type,
                        "itemType": None if item_type is None else str(item_type),
                        "itemId": item_id,
                    }
                )
            else:
                stream_event_overflow += 1
            if event_type in {"item.started", "item.updated", "item.completed"}:
                if len(item_events) < MAX_RETAINED_STREAM_EVENTS:
                    item_events.append(
                        {
                            "sequence": event_sequence,
                            "eventType": event_type,
                            "itemId": item_id,
                            "itemType": None if item_type is None else str(item_type),
                            "observedMs": offset_ms,
                            "observedAtUnixMs": _unix_ms_from_offset(
                                process_started_at_unix_ms, offset_ms
                            ),
                        }
                    )
                else:
                    item_event_overflow += 1
            if event_type == "item.completed" and item_type == "file_change":
                last_workspace_mutation_sequence = event_sequence
            if event_type == "turn.started" and turn_started_observed_ms is None:
                turn_started_observed_ms = offset_ms
            if (
                first_output_ns is None
                and event_type in {"item.started", "item.completed"}
                and item_type != "error"
            ):
                first_output_ns = observed_ns
            if event_type == "item.completed" and item_type == "agent_message":
                final_message = item.get("text")
            if (
                event_type in {"item.started", "item.updated", "item.completed"}
                and item_type == "command_execution"
            ):
                item_id_value = item.get("id")
                record_key = (
                    str(item_id_value)
                    if item_id_value is not None
                    else f"missing-item-id:{event_sequence}"
                )
                retain_record = (
                    record_key in command_records
                    or len(command_records) < MAX_RETAINED_COMMANDS
                )
                target_records = command_records if retain_record else {}
                target_order = command_order if retain_record else []
                record = record_command_event(
                    records=target_records,
                    order=target_order,
                    event_type=event_type,
                    item=item,
                    sequence=event_sequence,
                    observed_ms=offset_ms,
                    observed_at_unix_ms=_unix_ms_from_offset(
                        process_started_at_unix_ms, offset_ms
                    ),
                )
                if event_type == "item.completed":
                    actual_command_count += 1
                    command_kind_counts[str(record["kind"])] += 1
                    if record["mutating"] is True:
                        mutating_command_count += 1
                        last_workspace_mutation_sequence = event_sequence
                    if not retain_record:
                        command_record_overflow += 1
                    if record["status"] == "failed" or (
                        record["exitCode"] is not None and record["exitCode"] != 0
                    ):
                        command_execution_failures += 1
                    if record["requiredTest"]:
                        if record["passed"]:
                            last_successful_required_test_sequence = event_sequence
                        if (
                            len(required_test_attempts)
                            < MAX_RETAINED_REQUIRED_TEST_ATTEMPTS
                        ):
                            required_test_attempts.append(
                                {
                                    "command": record["command"][:1_000],
                                    "status": record["status"],
                                    "exitCode": record["exitCode"],
                                    "exitCodeReflectsSuite": record[
                                        "exitCodeReflectsSuite"
                                    ],
                                    "passed": record["passed"],
                                }
                            )
                        else:
                            required_test_attempt_overflow += 1
            if event_type in {"turn.completed", "turn.failed"} and terminal_ns is None:
                terminal_ns = observed_ns
                terminal_event = event_type
                model_wait_ms, continuation_count = turn_measurements(event)
                terminal_payload = event

        terminate_process(process)
        try:
            # `terminate_process` has already escalated to a tree kill, so this
            # only reaps. Bounded so an unreapable agent reports a run outcome
            # instead of stalling the benchmark.
            exit_code = process.wait(timeout=READER_JOIN_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            exit_code = -1
        ended_ns = time.perf_counter_ns()
        # Bounded so a reader still blocked on a pipe held open by a leaked
        # descendant cannot stall the benchmark. Joining without a timeout and
        # then asking `is_alive()` made `readersDrained` a tautology that always
        # reported True; with a bound the field reports something real.
        stdout_thread.join(timeout=READER_JOIN_TIMEOUT_SECONDS)
        stderr_thread.join(timeout=READER_JOIN_TIMEOUT_SECONDS)
        readers_drained = not stdout_thread.is_alive() and not stderr_thread.is_alive()
        if readers_drained:
            process.stdout.close()
            process.stderr.close()
        verifier_passed, verifier_failures = verify_fixture(workspace)

        reasons: list[str] = []
        if timed_out:
            reasons.append("agent timed out")
        if exit_code != 0:
            reasons.append(f"agent exited with code {exit_code}")
        if terminal_event != "turn.completed":
            reasons.append(f"terminal event was {terminal_event!r}")
        if invalid_json_lines:
            reasons.append(f"agent emitted {invalid_json_lines} invalid JSONL lines")
        if stdout_truncated_lines:
            reasons.append(
                f"agent emitted {stdout_truncated_lines} over-limit JSONL line(s)"
            )
        if not readers_drained:
            reasons.append("agent output readers did not drain after process-tree cleanup")
        reasons.extend(verifier_failures)
        outcome_correct = not reasons and verifier_passed
        required_test_passed = required_test_covers_final_workspace_state(
            last_successful_required_test_sequence,
            last_workspace_mutation_sequence,
        )
        added_files = added_workspace_files(workspace)
        task_contract_compliant = (
            outcome_correct and required_test_passed and not added_files
        )
        diagnostic_text = "\n".join(
            [
                *observed_text_parts,
                *stderr_lines,
                *reasons,
                final_message or "",
            ]
        )
        diagnostics = classify_diagnostics(
            observed_text=diagnostic_text,
            timed_out=timed_out,
            exit_code=exit_code,
            terminal_event=terminal_event,
            invalid_json_lines=invalid_json_lines,
            verifier_passed=verifier_passed,
            required_test_passed=required_test_passed,
            command_execution_failures=command_execution_failures,
        )
        completion_ms, wall_clock_ms = elapsed_measurements(
            started_ns=started_ns,
            ended_ns=ended_ns,
            terminal_ns=terminal_ns,
        )
        ttfo_ns = None if first_output_ns is None else first_output_ns - started_ns
        derived_rounds = stream_derived_rounds(stream_events)
        trace = build_turn_trace(
            terminal_event=terminal_event,
            terminal_payload=terminal_payload,
            timed_out=timed_out,
            process_started_at_unix_ms=process_started_at_unix_ms,
            wall_clock_ms=wall_clock_ms,
            turn_started_observed_ms=turn_started_observed_ms,
            terminal_observed_ms=(
                None
                if terminal_ns is None
                else round((terminal_ns - started_ns) / 1_000_000, 3)
            ),
            commands=[command_records[key] for key in command_order],
            item_events=item_events,
        )
        attach_stream_evidence(
            trace, derived_rounds, command_count=actual_command_count
        )
        trace["itemEventOverflow"] = item_event_overflow
        # Summarize over retained trace rows. Online command aggregates below
        # cover every observed command; row overflow is explicit for fields that
        # cannot be aggregated without retaining their full instrumentation.
        trace_summary = summarize_turn_trace(trace)
        trace_summary["commandKinds"] = dict(sorted(command_kind_counts.items()))
        trace_summary["mutatingCommands"] = mutating_command_count
        trace["retentionOverflow"] = truncate_turn_trace(trace)
        trace["retentionOverflow"].update(
            {
                "commandRecords": command_record_overflow,
                "requiredTestAttempts": required_test_attempt_overflow,
                "diagnosticTextLines": diagnostic_text_overflow,
                "stderrLines": stderr_line_overflow,
                "overLimitJsonlLines": stdout_truncated_lines,
            }
        )
        return {
            "variant": label,
            "repetition": repetition,
            "fixtureRevision": fixture_revision,
            "success": outcome_correct,
            "outcomeCorrect": outcome_correct,
            "taskContractCompliant": task_contract_compliant,
            "failureReasons": reasons,
            "complianceFailureReasons": (
                []
                if task_contract_compliant
                else [
                    reason
                    for reason in (
                        *reasons,
                        None
                        if required_test_passed
                        else (
                            f"no successful `{REQUIRED_TEST_COMMAND}` attempt covered "
                            "the final workspace state"
                        ),
                        None
                        if not added_files
                        else "files the fixture never created were added: "
                        + ", ".join(added_files[:10]),
                    )
                    if reason is not None
                ]
            ),
            "taskContract": {
                "requiredTestCommand": REQUIRED_TEST_COMMAND,
                "successfulTestObserved": required_test_passed,
                "lastSuccessfulTestSequence": last_successful_required_test_sequence,
                "lastWorkspaceMutationSequence": last_workspace_mutation_sequence,
                "testAttempts": required_test_attempts,
                "testAttemptOverflow": required_test_attempt_overflow,
                "editableFile": "duration.py",
                "addedFiles": added_files,
            },
            "diagnostics": diagnostics,
            "completionMs": completion_ms,
            "wallClockMs": wall_clock_ms,
            "modelWaitMs": model_wait_ms,
            "continuationCount": continuation_count,
            "actualCommandCount": actual_command_count,
            "ttfoMs": None if ttfo_ns is None else round(ttfo_ns / 1_000_000, 3),
            "exitCode": exit_code,
            "terminalEvent": terminal_event,
            "eventCounts": dict(sorted(event_counts.items())),
            "itemCounts": dict(sorted(item_counts.items())),
            "finalMessage": None if final_message is None else final_message[:500],
            "verifierPassed": verifier_passed,
            "readersDrained": readers_drained,
            # Bounded per-round evidence. Overflow counters make omitted rows
            # explicit while the top-level counts still cover the full stream.
            "turnTrace": trace,
            "turnTraceSummary": trace_summary,
            "continuationNarrative": [
                request["narrative"]
                for request in trace["modelRequests"]
                if request.get("isContinuation") is True
            ],
            "streamTimeline": stream_events,
            "streamTimelineOverflow": stream_event_overflow,
            "diagnosticTextOverflow": diagnostic_text_overflow,
            "stderrLineOverflow": stderr_line_overflow,
            "stderrTail": (
                "\n".join(stderr_lines[-10:])[-2000:]
                if not task_contract_compliant
                else ""
            ),
        }


def run_agent(
    *,
    binary: Path,
    label: str,
    repetition: int,
    model: str,
    reasoning_effort: str,
    personality: str,
    code_mode: str,
    auth_source: Path,
    timeout_seconds: int,
) -> dict[str, Any]:
    """Run one agent with unconditional process-tree cleanup."""
    processes: list[subprocess.Popen[str]] = []
    try:
        return _run_agent_impl(
            binary=binary,
            label=label,
            repetition=repetition,
            model=model,
            reasoning_effort=reasoning_effort,
            personality=personality,
            code_mode=code_mode,
            auth_source=auth_source,
            timeout_seconds=timeout_seconds,
            _process_holder=processes,
        )
    finally:
        for process in processes:
            terminate_process(process)
            try:
                process.wait(timeout=READER_JOIN_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                pass
            for stream in (process.stdout, process.stderr):
                if stream is not None and not stream.closed:
                    stream.close()


def distribution(values: list[float]) -> dict[str, Any] | None:
    if not values:
        return None
    return {
        "count": len(values),
        "averageMs": round(statistics.fmean(values), 3),
        "medianMs": round(statistics.median(values), 3),
        "minMs": round(min(values), 3),
        "maxMs": round(max(values), 3),
    }


def count_distribution(values: list[int]) -> dict[str, Any] | None:
    if not values:
        return None
    return {
        "count": len(values),
        "average": round(statistics.fmean(values), 3),
        "median": round(statistics.median(values), 3),
        "min": min(values),
        "max": max(values),
    }


def summarize(runs: list[dict[str, Any]]) -> dict[str, Any]:
    outcome_correct = [
        run for run in runs if run.get("outcomeCorrect", run.get("success", False))
    ]
    contract_compliant = [
        run for run in runs if run.get("taskContractCompliant", False)
    ]
    completion = [
        run["completionMs"]
        for run in outcome_correct
        if run.get("completionMs") is not None
    ]
    wall_clock = [
        run["wallClockMs"] for run in runs if run.get("wallClockMs") is not None
    ]
    ttfo = [
        run["ttfoMs"] for run in outcome_correct if run.get("ttfoMs") is not None
    ]
    model_wait = [
        run["modelWaitMs"] for run in runs if run.get("modelWaitMs") is not None
    ]
    continuations = [
        run["continuationCount"]
        for run in runs
        if run.get("continuationCount") is not None
    ]
    actual_commands = [run.get("actualCommandCount", 0) for run in runs]
    tests_ran = sum(
        run.get("taskContract", {}).get("successfulTestObserved", False) for run in runs
    )
    diagnostic_counts = Counter(
        diagnostic["category"]
        for run in runs
        for diagnostic in run.get("diagnostics", [])
    )
    outcome_rate = round(len(outcome_correct) / len(runs) * 100, 3)
    compliance_rate = round(len(contract_compliant) / len(runs) * 100, 3)
    # `timing` is emitted only by builds that carry the fork's instrumentation.
    # Absent values mean the variant cannot report the metric, which is not the
    # same as reporting a poor value, so say so explicitly rather than leaving a
    # bare null next to the other variant's numbers.
    timing_available_runs = sum(
        run.get("modelWaitMs") is not None and run.get("continuationCount") is not None
        for run in runs
    )
    timing_status = (
        "available"
        if timing_available_runs == len(runs)
        else "unavailable"
        if timing_available_runs == 0
        else "partial"
    )
    summaries = [
        run["turnTraceSummary"]
        for run in runs
        if isinstance(run.get("turnTraceSummary"), dict)
        and bool(run["turnTraceSummary"])
    ]
    traces = [
        run["turnTrace"]
        for run in runs
        if isinstance(run.get("turnTrace"), dict) and bool(run["turnTrace"])
    ]
    class_counts: Counter[str] = Counter()
    tag_counts: Counter[str] = Counter()
    interpretation_counts: Counter[str] = Counter()
    purpose_counts: Counter[str] = Counter()
    reason_counts: Counter[str] = Counter()
    disposition_counts: Counter[str] = Counter()
    command_kind_counts: Counter[str] = Counter()
    wait_by_class: dict[str, float] = {}
    for summary in summaries:
        class_counts.update(summary.get("byPrimaryClass", {}))
        tag_counts.update(summary.get("byTag", {}))
        interpretation_counts.update(summary.get("byInterpretation", {}))
        purpose_counts.update(summary.get("byPurpose", {}))
        reason_counts.update(summary.get("byReason", {}))
        disposition_counts.update(summary.get("byDisposition", {}))
        command_kind_counts.update(summary.get("commandKinds", {}))
        for name, value in summary.get("modelWaitMsByPrimaryClass", {}).items():
            wait_by_class[name] = round(wait_by_class.get(name, 0.0) + value, 3)
    observed_latencies = [
        command["nextObservedAction"]["latencyMs"]
        for trace in traces
        for command in trace.get("commands", [])
        if isinstance(command.get("nextObservedAction"), dict)
        and command["nextObservedAction"].get("latencyMs") is not None
    ]
    runtime_latencies = [
        command["runtimeLatencyToNextAction"]["completionToNextRequestDispatchMs"]
        for trace in traces
        for command in trace.get("commands", [])
        if isinstance(command.get("runtimeLatencyToNextAction"), dict)
        and command["runtimeLatencyToNextAction"].get(
            "completionToNextRequestDispatchMs"
        )
        is not None
    ]
    trace_statuses = Counter(trace.get("status", "absent") for trace in traces)
    if len(traces) < len(runs):
        trace_statuses["absent"] += len(runs) - len(traces)
    right_censored_runs = sum(
        trace.get("censoring", {}).get("rightCensored") is True for trace in traces
    )
    linkage_methods: Counter[str] = Counter()
    for trace in traces:
        linkage_methods.update(trace.get("linkage", {}).get("commandToToolMethods", {}))
    return {
        "runs": len(runs),
        # Retained for schema-v1 consumers: success means outcome correctness.
        "successfulRuns": len(outcome_correct),
        "failedRuns": len(runs) - len(outcome_correct),
        "successRatePercent": outcome_rate,
        "successfulCompletionTime": distribution(completion),
        "successfulTtfo": distribution(ttfo),
        "wallClock": distribution(wall_clock),
        "modelWait": distribution(model_wait),
        "continuationCount": count_distribution(continuations),
        "timingInstrumentation": {
            "available": timing_status == "available",
            "status": timing_status,
            "availableRuns": timing_available_runs,
            "unavailableRuns": len(runs) - timing_available_runs,
            "metrics": ["modelWait", "continuationCount"],
            "note": (
                "Emitted from the `timing` block of the terminal turn event. "
                "Builds without that instrumentation report null, which means "
                "the metric is unavailable for this variant, not that it "
                "measured worse. Do not compare these fields across variants "
                "unless both report available=true."
            ),
        },
        "actualCommandCount": count_distribution(actual_commands),
        "testsRan": {
            "runs": tests_ran,
            "ratePercent": round(tests_ran / len(runs) * 100, 3),
        },
        "outcomeCorrectness": {
            "correctRuns": len(outcome_correct),
            "incorrectRuns": len(runs) - len(outcome_correct),
            "ratePercent": outcome_rate,
            "correctCompletionTime": distribution(completion),
            "correctTtfo": distribution(ttfo),
        },
        "taskContractCompliance": {
            "compliantRuns": len(contract_compliant),
            "noncompliantRuns": len(runs) - len(contract_compliant),
            "ratePercent": compliance_rate,
        },
        "diagnosticCategoryCounts": dict(sorted(diagnostic_counts.items())),
        # Continuation structure. These break the single `continuationCount`
        # into the classes a reader can act on, and each count is backed by the
        # per-request rows in `turnTrace.modelRequests` of the same run.
        "continuationClassification": {
            "tracedRuns": len(summaries),
            "untracedRuns": len(runs) - len(summaries),
            # Only a build that emits model-request rows can classify anything.
            # An empty `byPrimaryClass` next to classifiedRuns=0 means the build
            # cannot report continuations, not that it made none.
            "classifiedRuns": sum(
                summary.get("recordedRequests", 0) > 0 for summary in summaries
            ),
            "byPrimaryClass": dict(sorted(class_counts.items())),
            "byTag": dict(sorted(tag_counts.items())),
            "byInterpretation": dict(sorted(interpretation_counts.items())),
            "byPurpose": dict(sorted(purpose_counts.items())),
            "byReason": dict(sorted(reason_counts.items())),
            "byDisposition": dict(sorted(disposition_counts.items())),
            "modelWaitMsByPrimaryClass": dict(sorted(wait_by_class.items())),
            "note": (
                "Classes are observational: they restate runtime-recorded purpose, "
                "attempt kind, state-change flags, and linked commands. The "
                "`necessary` class is the residual left when no other predicate "
                "matched, not a proof that the round was required."
            ),
        },
        "commandKindCounts": dict(sorted(command_kind_counts.items())),
        # Tool-completion-to-next-action latency. The observed variant is
        # measured by this harness and is therefore comparable between an
        # instrumented and an uninstrumented build; the runtime variant is
        # sharper but exists only where the build emits timing.
        "observedToolToNextActionMs": distribution(observed_latencies),
        "runtimeToolToNextRequestDispatchMs": distribution(runtime_latencies),
        "commandLinkageMethods": dict(sorted(linkage_methods.items())),
        # Censoring. A run killed at the timeout has no terminal timing record,
        # so it is absent from `modelWait` and `continuationCount` for a reason
        # that correlates with being slow. Reporting the count keeps those
        # distributions from being read as unbiased.
        "censoring": {
            "rightCensoredRuns": right_censored_runs,
            "traceStatusCounts": dict(sorted(trace_statuses.items())),
            "note": (
                "Right-censored runs are excluded from every build-emitted timing "
                "distribution above because no terminal timing record exists for "
                "them. Censoring is not random with respect to duration, so treat "
                "those distributions as conditional on reaching a terminal event."
            ),
        },
    }


def timing_metric_comparability(
    *,
    fork_label: str,
    fork_summary: dict[str, Any],
    upstream_label: str,
    upstream_summary: dict[str, Any],
) -> dict[str, Any]:
    variants = (
        (fork_label, fork_summary),
        (upstream_label, upstream_summary),
    )
    unavailable = [
        label
        for label, summary in variants
        if not summary["timingInstrumentation"]["available"]
    ]
    return {
        "metrics": [
            "modelWait",
            "continuationCount",
            "continuationClassification",
            "runtimeToolToNextRequestDispatchMs",
        ],
        "headToHeadComparable": not unavailable,
        "unavailableVariants": unavailable,
        "note": (
            "Compare these build-emitted metrics only when headToHeadComparable "
            "is true; a variant that emits no timing reports null rather than "
            "zero for all of them. Harness-measured completion, wall clock, TTFO, "
            "command count and kinds, observedToolToNextActionMs, correctness, "
            "and test execution remain comparable for both variants."
        ),
    }


PAIRED_METRICS = (
    "completionMs",
    "wallClockMs",
    "ttfoMs",
    "modelWaitMs",
    "continuationCount",
    "actualCommandCount",
    "nonProgressContinuations",
    "verificationContinuations",
    "retryContinuations",
    "recoveryContinuations",
    "redundantVerifications",
    "mutatingCommands",
    "observedToolToNextActionMedianMs",
)

# Metrics that exist only once a turn reaches its terminal event. Completion is
# measured by this harness at that event; the others come from its `timing`
# block. Everything else in `PAIRED_METRICS` is measured from the JSONL stream
# as the run proceeds and remains valid when the process is killed at timeout.
_TERMINAL_ONLY_METRICS = frozenset(
    {
        "completionMs",
        "modelWaitMs",
        "continuationCount",
        "nonProgressContinuations",
        "verificationContinuations",
        "retryContinuations",
        "recoveryContinuations",
        "redundantVerifications",
    }
)


def _run_metric(run: dict[str, Any], metric: str) -> float | int | None:
    """Read one comparable scalar off a run record."""
    if metric in {
        "completionMs",
        "wallClockMs",
        "ttfoMs",
        "modelWaitMs",
        "continuationCount",
    }:
        value = run.get(metric)
        return (
            value
            if isinstance(value, (int, float)) and not isinstance(value, bool)
            else None
        )
    if metric == "actualCommandCount":
        return run.get("actualCommandCount")
    summary = run.get("turnTraceSummary")
    if not isinstance(summary, dict):
        return None
    # Continuation structure exists only where the build emitted model-request
    # rows. Reporting a zero for a build that cannot report at all would make a
    # paired delta read as a difference in behavior rather than in instrumentation.
    classified = summary.get("recordedRequests", 0) > 0
    if metric in {
        "nonProgressContinuations",
        "verificationContinuations",
        "retryContinuations",
        "recoveryContinuations",
    }:
        if not classified:
            return None
        category = {
            "nonProgressContinuations": "non_progress",
            "verificationContinuations": "verification",
            "retryContinuations": "retry",
            "recoveryContinuations": "recovery",
        }[metric]
        return summary.get("byPrimaryClass", {}).get(category, 0)
    if metric == "redundantVerifications":
        if not classified:
            return None
        return summary.get("byInterpretation", {}).get("redundant_verification", 0)
    if metric == "mutatingCommands":
        return summary.get("mutatingCommands")
    if metric == "observedToolToNextActionMedianMs":
        observed = summary.get("observedToolToNextActionMs")
        return observed.get("medianMs") if isinstance(observed, dict) else None
    return None


def sign_test_p_value(positive: int, negative: int) -> float | None:
    """Exact two-sided sign test over the non-zero paired differences.

    Over five or fewer non-zero differences the only attainable two-sided
    p-values are 1.0, 0.625, 0.5, 0.375, 0.25, 0.125, and 0.0625. Six non-zero
    differences add 0.03125, which is why the balanced default uses six pairs.
    The count of non-zero differences sets the actual floor and drops below the
    repetition count whenever a pair ties or is excluded, so the value a metric
    reached is its own `signTestTwoSidedP`, not the design-wide best case.
    """
    trials = positive + negative
    if trials == 0:
        return None
    extreme = min(positive, negative)
    tail = sum(math.comb(trials, index) for index in range(extreme + 1))
    return min(1.0, round(2 * tail / (2**trials), 6))


def paired_comparison(
    pairs: list[dict[str, Any]], *, fork_label: str, upstream_label: str
) -> dict[str, Any]:
    """Per-repetition differences, which is the design this benchmark actually has.

    Both variants run the same fixture in the same repetition with the order
    alternated, so a within-pair difference removes the between-repetition
    variance that a variant-level average leaves in. It still does not identify a
    cause: it measures the fork build as a whole against the upstream build as a
    whole.
    """
    eligibility: list[dict[str, Any]] = []
    for pair in pairs:
        fork_run = pair["currentFork"]
        upstream_run = pair["upstreamC"]
        both_outcome_correct = all(
            run.get("outcomeCorrect", run.get("success", False)) is True
            for run in (fork_run, upstream_run)
        )
        both_contract_compliant = all(
            run.get("taskContractCompliant", False) is True
            for run in (fork_run, upstream_run)
        )
        eligibility.append(
            {
                "repetition": pair["repetition"],
                "bothOutcomeCorrect": both_outcome_correct,
                "bothTaskContractCompliant": both_contract_compliant,
                "eligibleForPerformance": (
                    both_outcome_correct and both_contract_compliant
                ),
            }
        )
    eligible_by_repetition = {
        entry["repetition"]: entry["eligibleForPerformance"] for entry in eligibility
    }
    eligible_pairs = sum(eligible_by_repetition.values())
    total_pairs = len(pairs)

    results: dict[str, Any] = {}
    for metric in PAIRED_METRICS:
        terminal_only = metric in _TERMINAL_ONLY_METRICS
        deltas: list[dict[str, Any]] = []
        for pair in pairs:
            fork_run = pair["currentFork"]
            upstream_run = pair["upstreamC"]
            fork_value = _run_metric(fork_run, metric)
            upstream_value = _run_metric(upstream_run, metric)
            censored = any(
                run.get("turnTrace", {}).get("censoring", {}).get("rightCensored")
                is True
                for run in (fork_run, upstream_run)
            )
            deltas.append(
                {
                    "repetition": pair["repetition"],
                    "fork": fork_value,
                    "upstream": upstream_value,
                    "delta": (
                        None
                        if fork_value is None or upstream_value is None
                        else round(fork_value - upstream_value, 3)
                    ),
                    "eitherSideRightCensored": censored,
                    "jointlyCorrectAndCompliant": eligible_by_repetition[
                        pair["repetition"]
                    ],
                }
            )
        # Censoring only invalidates a metric that needs the terminal turn event.
        # A killed run still has a real wall clock, a real TTFO, and a real count
        # of the commands this harness watched it run, and censoring correlates
        # with being slow, so excluding those pairs would drop exactly the slow
        # runs from a duration comparison and flatter whichever variant times out
        # more.
        usable = [
            entry["delta"]
            for entry in deltas
            if entry["delta"] is not None
            and entry["jointlyCorrectAndCompliant"]
            and not (terminal_only and entry["eitherSideRightCensored"])
        ]
        positive = sum(delta > 0 for delta in usable)
        negative = sum(delta < 0 for delta in usable)
        results[metric] = {
            "pairs": deltas,
            "censoringSensitive": terminal_only,
            "exclusionRule": (
                "only jointly correct and task-contract-compliant pairs are used; "
                "pairs with a right-censored side are also excluded because the "
                "metric only exists once a turn reaches its terminal event"
                if terminal_only
                else "only jointly correct and task-contract-compliant pairs are "
                "used; among those, right-censored pairs are kept because this "
                "metric is measured by the harness as the run proceeds"
            ),
            "usablePairs": len(usable),
            "excludedPairs": len(deltas) - len(usable),
            "censoredPairs": sum(
                entry["eitherSideRightCensored"] for entry in deltas
            ),
            "medianDelta": round(statistics.median(usable), 3) if usable else None,
            "meanDelta": round(statistics.fmean(usable), 3) if usable else None,
            "forkHigherPairs": positive,
            "forkLowerPairs": negative,
            "tiedPairs": len(usable) - positive - negative,
            "signTestTwoSidedP": sign_test_p_value(positive, negative),
        }
    return {
        "direction": f"delta = {fork_label} minus {upstream_label}",
        "jointSuccess": {
            "totalPairs": total_pairs,
            "bothOutcomeCorrectPairs": sum(
                entry["bothOutcomeCorrect"] for entry in eligibility
            ),
            "bothTaskContractCompliantPairs": sum(
                entry["bothTaskContractCompliant"] for entry in eligibility
            ),
            "eligiblePerformancePairs": eligible_pairs,
            "eligiblePerformanceRatePercent": (
                round(eligible_pairs / total_pairs * 100, 3) if total_pairs else None
            ),
            "pairs": eligibility,
        },
        "metrics": results,
        "terminalOnlyMetrics": sorted(_TERMINAL_ONLY_METRICS & set(PAIRED_METRICS)),
        "note": (
            "Performance deltas use only repetitions where both runs were correct "
            "and task-contract compliant; joint success is reported separately. "
            "Within that set, exclusion is per metric and is not random with "
            "respect to duration. A metric marked `censoringSensitive` needs the terminal "
            "turn event, so a killed run carries no value to difference and its "
            "pair is dropped. Every other metric here is measured by this "
            "harness while the run proceeds, so censored pairs are kept: "
            "dropping them would remove the slowest runs from the comparison. "
            "`censoredPairs` reports the censoring either way."
        ),
    }


def make_pair_order_balance(
    *, repetitions: int, fork_label: str, upstream_label: str
) -> dict[str, Any]:
    """How often each variant ran first, and whether that is balanced.

    The runner alternates strictly, which counterbalances execution order only
    for an even repetition count. Argument validation rejects odd counts; this
    helper still states the residual accurately for programmatic callers.
    """
    upstream_first = (repetitions + 1) // 2
    fork_first = repetitions // 2
    return {
        f"{upstream_label}First": upstream_first,
        f"{fork_label}First": fork_first,
        "balanced": upstream_first == fork_first,
        "note": (
            "Alternation fully counterbalances execution order only for an even "
            "repetition count. With an odd count one variant runs first once "
            "more than the other, so any order effect is not fully removed from "
            "the paired deltas."
        ),
    }


def attribution_scope(*, repetitions: int) -> dict[str, Any]:
    """State plainly what this report's evidence does and does not support."""
    return {
        "supportedClaims": [
            (
                "Within one run, which model round issued which command, using the "
                "runtime generation identity carried by the tool-call timing ledger."
            ),
            (
                "Within one run, the observed classification of each continuation "
                "and the runtime facts that classification restates."
            ),
            (
                "Within one run, the latency from a tool result to the next observed "
                "action, both as this harness saw it and, where the build emits "
                "timing, as the runtime recorded it."
            ),
            (
                "Across the paired repetitions, the per-pair difference in each "
                "reported metric together with its sign counts."
            ),
        ],
        "unsupportedClaims": [
            (
                "That a named guidance rule, prompt clause, or configuration caused "
                "a particular continuation. No guidance-rule attribution is "
                "observed anywhere in this pipeline."
            ),
            (
                "That an aggregate difference between the variants is caused by any "
                "single mechanism. Live model behavior is stochastic, so a "
                "difference over a handful of paired runs is an association."
            ),
            (
                "That a round classified `necessary` was in fact required. That "
                "class is the residual after the other predicates fail to match."
            ),
        ],
        "continuationClasses": {
            "labels": list(REQUEST_CLASSES),
            "precedence": list(CONTINUATION_CLASS_PRECEDENCE),
            "residual": "necessary",
        },
        "designLimits": {
            "repetitionsPerVariant": repetitions,
            "pairing": "same fixture and repetition, alternating execution order",
            "smallestAttainableTwoSidedSignTestP": sign_test_p_value(repetitions, 0),
            "censoring": (
                "Timed-out runs are right-censored: they carry no terminal timing "
                "record, and censoring correlates with being slow."
            ),
        },
        "toStrengthenAttribution": [
            (
                "Vary one guidance rule at a time while holding the binary constant, "
                "so a rule becomes an assigned condition rather than an unobserved "
                "one."
            ),
            (
                "Raise repetitions until the paired sign test can attain the intended "
                "alpha, or replace the point comparison with an interval."
            ),
            "Extend the fixture set so a result is not conditional on one task.",
        ],
    }


def make_report(args: argparse.Namespace) -> dict[str, Any]:
    fork_binary = args.fork_binary.resolve()
    upstream_binary = args.upstream_binary.resolve()
    fork_root = args.fork_root.resolve()
    upstream_root = args.upstream_root.resolve()
    auth_source = args.auth_source.resolve()
    for path in (fork_binary, upstream_binary, auth_source):
        if not path.is_file():
            raise FileNotFoundError(path)
    fork_source_state = exact_source_state(
        fork_root, args.fork_revision, args.fork_label
    )
    upstream_source_state = exact_source_state(
        upstream_root, args.upstream_revision, args.upstream_label
    )
    # Hash both binaries before any run. Hashing only afterwards left a rebuild
    # mid-benchmark undetectable whenever it landed on a clean commit: the source
    # checks would pass and the report would bind the results to a binary that
    # had not produced them.
    fork_binary_sha256 = sha256(fork_binary)
    upstream_binary_sha256 = sha256(upstream_binary)
    fork_identity = binary_identity(
        fork_binary,
        fork_root,
        fork_source_state,
        args.fork_label,
        build_command=args.fork_build_command,
        sha256_before_runs=fork_binary_sha256,
    )
    upstream_identity = binary_identity(
        upstream_binary,
        upstream_root,
        upstream_source_state,
        args.upstream_label,
        sha256_before_runs=upstream_binary_sha256,
    )
    binary_baselines = {
        "currentFork": (args.fork_label, fork_binary, fork_binary_sha256),
        "upstreamC": (
            args.upstream_label,
            upstream_binary,
            upstream_binary_sha256,
        ),
    }

    pairs: list[dict[str, Any]] = []
    for repetition in range(1, args.repetitions + 1):
        upstream_first = repetition % 2 == 1
        order = (
            (
                ("upstreamC", args.upstream_label, upstream_binary),
                ("currentFork", args.fork_label, fork_binary),
            )
            if upstream_first
            else (
                ("currentFork", args.fork_label, fork_binary),
                ("upstreamC", args.upstream_label, upstream_binary),
            )
        )
        runs: dict[str, dict[str, Any]] = {}
        for role, label, binary in order:
            _, _, expected_binary_sha256 = binary_baselines[role]
            require_binary_sha256(binary, expected_binary_sha256, label)
            print(f"pair {repetition}/{args.repetitions}: starting {label}", flush=True)
            run = run_agent(
                binary=binary,
                label=label,
                repetition=repetition,
                model=args.model,
                reasoning_effort=args.reasoning_effort,
                personality=args.personality,
                code_mode=args.code_mode,
                auth_source=auth_source,
                timeout_seconds=args.timeout_seconds,
            )
            require_binary_sha256(binary, expected_binary_sha256, label)
            runs[role] = run
            print(
                f"pair {repetition}/{args.repetitions}: {label} "
                f"outcomeCorrect={run['outcomeCorrect']} "
                f"taskContractCompliant={run['taskContractCompliant']} "
                f"completionMs={run['completionMs']} "
                f"modelWaitMs={run['modelWaitMs']} "
                f"continuations={run['continuationCount']} "
                f"commands={run['actualCommandCount']} failures={run['failureReasons']}",
                flush=True,
            )
        pairs.append(
            {
                "repetition": repetition,
                "order": (
                    f"{args.upstream_label},{args.fork_label}"
                    if upstream_first
                    else f"{args.fork_label},{args.upstream_label}"
                ),
                "currentFork": runs["currentFork"],
                "upstreamC": runs["upstreamC"],
            }
        )

    fork_runs = [pair["currentFork"] for pair in pairs]
    upstream_runs = [pair["upstreamC"] for pair in pairs]
    if (
        exact_source_state(fork_root, args.fork_revision, args.fork_label)
        != fork_source_state
    ):
        raise RuntimeError(
            f"{args.fork_label} source state changed during the benchmark"
        )
    if (
        exact_source_state(upstream_root, args.upstream_revision, args.upstream_label)
        != upstream_source_state
    ):
        raise RuntimeError(
            f"{args.upstream_label} source state changed during the benchmark"
        )
    for label, path, expected in binary_baselines.values():
        require_binary_sha256(path, expected, label)
    default_comparison = (
        args.fork_label == "currentFork" and args.upstream_label == "upstreamC"
    )
    fork_summary = summarize(fork_runs)
    upstream_summary = summarize(upstream_runs)
    timing_comparability = timing_metric_comparability(
        fork_label=args.fork_label,
        fork_summary=fork_summary,
        upstream_label=args.upstream_label,
        upstream_summary=upstream_summary,
    )
    paired = paired_comparison(
        pairs, fork_label=args.fork_label, upstream_label=args.upstream_label
    )
    return {
        "schemaVersion": REPORT_SCHEMA_VERSION,
        "kind": (
            "fork-vs-official-upstream-c-live-agent-task"
            if default_comparison
            else "paired-live-agent-task"
        ),
        "capturedAt": datetime.now(timezone.utc).isoformat(),
        "scope": "input-identical live coding task",
        "currentFork": fork_identity,
        "upstreamC": {
            **upstream_identity,
            "immutableReference": default_comparison,
        },
        "methodology": {
            "taskPrompt": TASK_PROMPT,
            "fixtureManifestSha256": text_sha256(
                json.dumps(fixture_manifest(), sort_keys=True, separators=(",", ":"))
            ),
            "fixtureFiles": fixture_manifest(),
            "outcomeCorrectnessCheck": "unchanged task files, visible unittest suite, and external hidden cases",
            "taskContractComplianceCheck": (
                f"outcome correctness, an observed successful `{REQUIRED_TEST_COMMAND}` "
                "command execution before the turn finished, and no file added "
                "beyond the fixture (the contract permits editing `duration.py` only); "
                "a test invocation whose exit code is unconditionally replaced by a "
                "trailing `||`, `;`, or `&` records an attempt but never a pass"
            ),
            "legacySuccessField": "retained as an alias for outcomeCorrect so schema-v1 outcome rates remain comparable",
            "sourceIdentityCheck": (
                "each supplied revision must equal HEAD and `git status --porcelain=v1 "
                "-z --untracked-files=all` must be empty before and after all runs"
            ),
            "binaryIdentityCheck": (
                "each binary is hashed before the first run and re-hashed immediately "
                "before and after each of its runs, plus after the last pair; a change "
                "aborts the report so results are never attributed to a binary that "
                "did not produce them"
            ),
            "verifierTimeoutSeconds": VERIFIER_TIMEOUT_SECONDS,
            "repetitionsPerVariant": args.repetitions,
            "pairOrder": f"alternating; {args.upstream_label} first in odd repetitions",
            # Strict alternation cannot balance an odd repetition count, so the
            # residual order effect is reported rather than described as
            # counterbalanced.
            "pairOrderBalance": make_pair_order_balance(
                repetitions=args.repetitions,
                fork_label=args.fork_label,
                upstream_label=args.upstream_label,
            ),
            "comparisonLabels": {
                "currentForkRole": args.fork_label,
                "upstreamCRole": args.upstream_label,
            },
            "model": args.model,
            "reasoningEffort": args.reasoning_effort,
            "personality": args.personality,
            "codeMode": f"{args.code_mode}; explicitly pinned for both variants",
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
            "home": "fresh CODEX_HOME per run with the same auth source and no user config",
            "completionMetric": "process launch through turn.completed JSONL event",
            "ttfoMetric": "process launch through first non-error item.started or item.completed JSONL event",
            "timeoutSeconds": args.timeout_seconds,
            "timingInstrumentationAsymmetry": (
                "modelWait and continuationCount come from a `timing` block that "
                "only instrumented builds emit. When one variant reports "
                "timingInstrumentation.available=false, those two metrics are "
                "not a valid head-to-head comparison; completionMs, wallClockMs, "
                "and ttfoMs are measured by this harness for both variants and "
                "remain comparable."
            ),
            "latencyInterpretation": (
                "This benchmark distinguishes TTFO from post-TTFO completion. Similar "
                "TTFO with much slower completion is evidence of a post-TTFO "
                "harness/tool-loop problem; it does not establish an inference-speed problem."
            ),
            "turnTrace": {
                "schemaVersion": TURN_TRACE_SCHEMA_VERSION,
                "runtimeEvidence": (
                    "Scalar terminal timing fields are retained, while model-request "
                    "and tool-call rows are stored once as bounded, augmented trace rows "
                    "with absolute timestamps, lineage, fixed observational "
                    "classifications, and reviewable narratives. Explicit overflow "
                    "counts report omitted rows."
                ),
                "commandEvidence": (
                    "Command lifecycles retain bounded command text plus its original "
                    "length, truncation flag, and full-text SHA-256, JSONL item ID, status, exit code, "
                    "sequence numbers, monotonic offsets, and derived Unix timestamps. "
                    "Aggregate command counts cover the full observed stream."
                ),
                "linkage": (
                    "Tool calls carry exact runtime generation identity. JSONL command "
                    "item IDs are synthetic, so fallback command-to-tool linkage is "
                    "explicitly marked ordinal and is disabled for overflowed, invalid, "
                    "incomplete, or non-one-to-one populations."
                ),
                "censoring": (
                    "A run without a terminal event is right-censored. The harness keeps "
                    "its observed stream, the counts it directly observed, and its "
                    "wall-clock observation instead of manufacturing terminal timing. "
                    "`censoring.observedFloors` holds only directly counted quantities; "
                    "the reconstructed round count and post-tool idle sum are bounds in "
                    "neither direction and are reported under `approximations`."
                ),
                "crossVariantComparability": (
                    "`streamDerived` and each command's `nextObservedAction` are measured "
                    "from this harness's own read timestamps, so tool-result-to-next-action "
                    "latency and round structure stay comparable between an instrumented "
                    "and an uninstrumented build, and survive a killed run. The runtime "
                    "`latencyToNextAction` fields are sharper but exist only where the "
                    "build emits timing."
                ),
            },
            "pairedAnalysis": (
                "Both variants run the same fixture in the same repetition with the order "
                "alternated, so results.pairedComparison differences each metric within a "
                "pair and reports the sign counts and the exact two-sided sign-test tail. "
                "Censoring is handled per metric: terminal-only metrics exclude a pair "
                "with a censored side, while harness-observed metrics retain it; every "
                "metric reports its censored and excluded pair counts."
            ),
            "attribution": (
                "results.attributionScope enumerates what the evidence supports and what "
                "it does not. No stage observes guidance rules as causes, so no round is "
                "attributed to one."
            ),
        },
        "results": {
            "currentFork": fork_summary,
            "upstreamC": upstream_summary,
            "timingMetricComparability": timing_comparability,
            "pairedComparison": paired,
            "attributionScope": attribution_scope(repetitions=args.repetitions),
            "pairs": pairs,
        },
        "limitations": [
            (
                "This is one fixed coding task repeated "
                f"{args.repetitions} times per variant, not a broad task suite."
            ),
            "Live model behavior is intentionally stochastic even with identical inputs and settings.",
            "Completion-time summaries include outcome-correct runs only; every incorrect or noncompliant run remains explicit in the pair records.",
            "The external verifier adjudicates success after the timed agent turn and is not included in completion time.",
            "The recorded build command is provenance supplied by the benchmark operator; the report independently binds the candidate to a clean commit/tree and binary SHA-256.",
            "modelWait and continuationCount depend on build-side timing instrumentation and are omitted for variants that do not emit it; they are not comparable unless both variants report timingInstrumentation.available=true.",
            "completionMs is recorded only for runs that reached a terminal turn event; runs without one report wallClockMs instead.",
            "Timed-out runs are right-censored: no terminal turn event means no terminal timing record, so they are absent from build-emitted timing distributions and terminal-only paired deltas. Harness-observed metrics keep those pairs because censoring correlates with being slow; results.pairs[].turnTrace.censoring records the floors each censored run still proves.",
            "`codex exec --json` renumbers thread items, so a command is joined to its model round by the timing ledger's call identity when the build exposes one and otherwise by an explicitly labelled one-to-one chronological match; results.pairs[].turnTrace.linkage names the method used and lists anything it refused to join.",
            "Continuation classes restate runtime-recorded facts (purpose, attempt kind, state-change flags, linked commands). They are observational labels, and the `necessary` class is the residual after the other predicates fail to match.",
            "No stage of this pipeline observes guidance rules, prompt clauses, or configuration as causes of a continuation, so the report cannot attribute a round to one; results.attributionScope states this and what would be needed to establish it.",
            "The exact paired sign test is reported with its attainable p-value floor; the small, single-task design does not by itself license a broad significance claim.",
            "Command kinds and the mutating flag are derived from the command text with a conservative allowlist; an unrecognized program is reported as `other` rather than assigned to a category the text does not establish.",
        ],
    }


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="kd4-live-self-test-") as temp:
        root_a = Path(temp) / "a"
        root_b = Path(temp) / "b"
        root_a.mkdir()
        root_b.mkdir()
        revision_a = create_fixture(root_a)
        revision_b = create_fixture(root_b)
        assert revision_a == revision_b
        source_state = exact_source_state(root_a, revision_a, "self-test fixture")
        assert source_state["commit"] == revision_a
        assert source_state["clean"]
        assert not verify_fixture(root_a)[0]
        (root_a / "duration.py").write_text(
            CORRECT_IMPLEMENTATION, encoding="utf-8", newline="\n"
        )
        passed, failures = verify_fixture(root_a)
        assert passed, failures
        # A worktree-rewriting git command must not disturb the protected files.
        # With an inherited `core.autocrlf=true` this used to rewrite them to
        # CRLF and fail a run that never edited them.
        subprocess.run(
            ["git", "checkout", "--", "."],
            cwd=root_a,
            check=True,
            capture_output=True,
        )
        for protected in ("README.md", "test_duration.py", "AGENTS.md"):
            assert sha256(root_a / protected) == text_sha256(
                FIXTURE_FILES[protected]
            ), f"{protected} changed after a git checkout"
    summary = summarize(
        [
            {
                "success": True,
                "outcomeCorrect": True,
                "taskContractCompliant": True,
                "completionMs": 100.0,
                "ttfoMs": 10.0,
                "diagnostics": [],
            },
            {
                "success": True,
                "outcomeCorrect": True,
                "taskContractCompliant": False,
                "completionMs": 300.0,
                "ttfoMs": 30.0,
                "diagnostics": [{"category": "task_contract_violation"}],
            },
            {
                "success": False,
                "outcomeCorrect": False,
                "taskContractCompliant": False,
                "completionMs": None,
                "wallClockMs": 600_000.0,
                "ttfoMs": None,
                "diagnostics": [{"category": "verifier_failure"}],
            },
        ]
    )
    assert summary["successRatePercent"] == 66.667
    assert summary["outcomeCorrectness"]["ratePercent"] == 66.667
    assert summary["taskContractCompliance"]["ratePercent"] == 33.333
    assert summary["successfulCompletionTime"]["averageMs"] == 200.0
    assert summary["successfulCompletionTime"]["medianMs"] == 200.0
    assert summary["diagnosticCategoryCounts"] == {
        "task_contract_violation": 1,
        "verifier_failure": 1,
    }
    # A run without a terminal event contributes no completion time.
    assert summary["successfulCompletionTime"]["count"] == 2
    assert summary["wallClock"]["count"] == 1
    assert summary["timingInstrumentation"]["available"] is False
    instrumented = summarize(
        [
            {
                "outcomeCorrect": True,
                "taskContractCompliant": True,
                "completionMs": 100.0,
                "wallClockMs": 110.0,
                "ttfoMs": 10.0,
                "modelWaitMs": 42.0,
                "continuationCount": 2,
                "diagnostics": [],
            }
        ]
    )
    assert instrumented["timingInstrumentation"]["available"] is True
    assert instrumented["modelWait"]["averageMs"] == 42.0
    assert instrumented["continuationCount"]["average"] == 2
    for accepted in (
        "python -m unittest -q",
        "python.exe -m unittest -q",
        "python3 -m unittest -q",
        "py -m unittest -q",
        "/usr/bin/python -m unittest -q",
        "./venv/bin/python -m unittest -q",
        '"C:\\Program Files\\Python\\python.exe" -m unittest -q',
    ):
        assert is_required_test_command(accepted), accepted
    for rejected in (
        "python -m unittest",
        "python -m unittest -v",
        "python -m unittest -qq",
        "python -m unittest discover -q",
        "notpython -m unittest -q",
        # Quoted rather than executed.
        "echo python -m unittest -q",
        # Narrowed to zero tests while still exiting 0.
        "python -m unittest -q -k nothing",
        "python -m unittest -q -p test_none.py",
        "python -m unittest -q test_duration",
    ):
        assert not is_required_test_command(rejected), rejected
    # A shell wrapper must not change the verdict: `command_display_string` joins
    # the executed argv, so the required suite normally arrives wrapped.
    for wrapped in (
        'bash -lc "python -m unittest -q"',
        "bash -lc 'python -m unittest -q'",
        'powershell.exe -NoProfile -Command "cd repo; python -m unittest -q"',
    ):
        assert is_required_test_command(wrapped), wrapped
    assert not is_required_test_command(
        'bash -lc "python -m unittest -q test_duration"'
    )
    # Detection and pass-proof are separate: a suffix that unconditionally
    # replaces the suite's exit code still records an attempt, never a pass.
    for trusted in (
        "python -m unittest -q",
        "cd repo && python -m unittest -q",
        "python -m unittest -q && echo done",
        "python -m unittest -q 2>&1 | tail -5",
        'bash -lc "python -m unittest -q"',
    ):
        assert required_test_exit_code_reflects_suite(trusted), trusted
    for masked in (
        "python -m unittest -q || true",
        "python -m unittest -q; echo done",
        'bash -lc "python -m unittest -q || true"',
    ):
        assert is_required_test_command(masked), masked
        assert not required_test_exit_code_reflects_suite(masked), masked
    for command_text, expected in (
        ("python -m unittest -q", ("required_test", False)),
        ("git status", ("inspection", False)),
        ("git checkout -- duration.py", ("mutation", True)),
        ('bash -lc "echo x > duration.py"', ("mutation", True)),
        ("some-unknown-tool", ("other", False)),
    ):
        assert classify_command(command_text) == expected[0], command_text
        assert command_is_mutating(command_text) is expected[1], command_text
    # Exact two-sided sign-test tails, including the floor at five repetitions.
    assert sign_test_p_value(0, 0) is None
    assert sign_test_p_value(5, 0) == 0.0625
    assert sign_test_p_value(3, 2) == 1.0
    scope = attribution_scope(repetitions=5)
    assert scope["designLimits"]["smallestAttainableTwoSidedSignTestP"] == 0.0625
    assert any("guidance rule" in claim for claim in scope["unsupportedClaims"])
    command = build_agent_command(
        binary=Path("codex"),
        workspace=Path("workspace"),
        model="test-model",
        reasoning_effort="high",
        personality="pragmatic",
        code_mode="enabled",
    )
    assert command.count("features.code_mode=true") == 1
    assert command[command.index("features.code_mode=true") - 1] == "-c"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--fork-binary", type=Path)
    parser.add_argument("--upstream-binary", type=Path)
    parser.add_argument("--fork-root", type=Path)
    parser.add_argument("--fork-revision")
    parser.add_argument("--fork-build-command")
    parser.add_argument("--fork-label", default="currentFork")
    parser.add_argument("--upstream-root", type=Path)
    parser.add_argument("--upstream-revision")
    parser.add_argument("--upstream-label", default="upstreamC")
    parser.add_argument(
        "--auth-source", type=Path, default=Path.home() / ".codex" / "auth.json"
    )
    parser.add_argument("--output", type=Path)
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--reasoning-effort", default="high")
    parser.add_argument("--personality", default="pragmatic")
    parser.add_argument(
        "--code-mode", choices=("enabled", "disabled"), default="enabled"
    )
    parser.add_argument("--repetitions", type=int, default=6)
    parser.add_argument("--timeout-seconds", type=int, default=600)
    args = parser.parse_args()
    if args.fork_label == args.upstream_label:
        parser.error(
            "--fork-label and --upstream-label must differ; identical labels make "
            "pairOrder and comparisonLabels ambiguous"
        )
    if args.repetitions <= 0 or args.timeout_seconds <= 0:
        parser.error("repetitions and timeout-seconds must be positive")
    if args.repetitions % 2:
        parser.error(
            "--repetitions must be even so alternating pair order is balanced"
        )
    if args.self_test:
        return args
    required = (
        "fork_binary",
        "upstream_binary",
        "fork_root",
        "fork_revision",
        "fork_build_command",
        "upstream_root",
        "upstream_revision",
        "output",
    )
    missing = [name for name in required if getattr(args, name) is None]
    if missing:
        parser.error("missing required arguments: " + ", ".join(missing))
    return args


def main() -> None:
    args = parse_args()
    if args.self_test:
        self_test()
        print("self-test passed")
        return
    report = make_report(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report["results"], indent=2), flush=True)


if __name__ == "__main__":
    main()
