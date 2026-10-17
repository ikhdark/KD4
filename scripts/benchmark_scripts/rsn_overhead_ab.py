"""Matched A/B benchmark for the KD4 reasoning-overhead toggles (rsn_overhead_toggles.py).

usage:
  python -B rsn_overhead_ab.py setup
  python -B rsn_overhead_ab.py run <batch> <task> <arm>[.<rep>][,...]
  python -B rsn_overhead_ab.py analyze <batch-prefix> [...]

All runs of one `run` invocation start together, each in its own copy of the
task's git-committed master workspace, through one toggled binary; arms differ
only in KD4_RSN_* variables. `analyze` reads the copied rollouts (resolving
payload-artifact stubs), grades each workspace, and prints per-run rows and
per-(task, arm) means with sample standard deviations.
"""

import glob
import itertools
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = os.path.join(tempfile.gettempdir(), "kd4-rsn")
BIN = os.path.join(ROOT, "bin", "codex.exe")
HOME = r"C:\Users\kuh\Desktop\LOCAL-KD"
COMMON = [
    "-s",
    "danger-full-access",
    "-c",
    'approval_policy="never"',
    "-c",
    'model="gpt-6-astra"',
    "-c",
    'model_reasoning_effort="high"',
    "-c",
    'model_reasoning_summary="concise"',
    "--json",
]
ON = "1"
ARMS = {
    "base": {},
    "comb": {"KD4_RSN_NESTED": ON, "KD4_RSN_SCOPE": ON, "KD4_RSN_RESULT": ON},
    "nested": {"KD4_RSN_NESTED": ON},
    "scope": {"KD4_RSN_SCOPE": ON},
    "result": {"KD4_RSN_RESULT": ON},
}
TASKS = {
    "t1": (
        "master",
        "list all prompt/guidance/templates in codex-rs, put it into a .txt file.",
    ),
    "t2": (
        "master_t2",
        "In pytools/semver_tool, prerelease versions sort incorrectly and bump() is not "
        "implemented. Fix the ordering to follow SemVer 2.0.0 precedence and implement bump() "
        "as its docstring specifies. Run the package tests before finishing.",
    ),
    # Toggle smoke check: no grader, no exhaustive-scope wording.
    "smoke": ("master", "Reply with exactly OK."),
}

# T1: dedicated prompt/guidance/template assets every correct inventory names.
T1_REQUIRED = """
core/templates/agents/orchestrator.md
models-manager/templates/collaboration_mode/default.md
models-manager/templates/collaboration_mode/plan.md
prompts/templates/apply_patch_tool_instructions.md
prompts/templates/compact/incremental_prompt.md
prompts/templates/compact/prompt.md
prompts/templates/compact/summary_prefix.md
prompts/templates/goals/budget_limit.md
prompts/templates/goals/continuation.md
prompts/templates/goals/objective_updated.md
prompts/templates/permissions/approval_policy/never.md
prompts/templates/permissions/approval_policy/on_request.md
prompts/templates/permissions/approval_policy/on_request_rule_request_permission.md
prompts/templates/permissions/approval_policy/unless_trusted.md
prompts/templates/permissions/sandbox_mode/danger_full_access.md
prompts/templates/permissions/sandbox_mode/read_only.md
prompts/templates/permissions/sandbox_mode/workspace_write.md
prompts/templates/review/exit_interrupted.xml
prompts/templates/review/exit_success.xml
prompts/templates/review/rubric.md
protocol/src/prompts/base_instructions/default.md
skills/src/assets/samples/imagegen/SKILL.md
skills/src/assets/samples/imagegen/references/prompting.md
skills/src/assets/samples/imagegen/references/sample-prompts.md
skills/src/assets/samples/openai-docs/SKILL.md
skills/src/assets/samples/openai-docs/references/prompting-guide.md
skills/src/assets/samples/plugin-creator/SKILL.md
skills/src/assets/samples/skill-creator/SKILL.md
skills/src/assets/samples/skill-installer/SKILL.md
tui/prompt_for_init_command.md
""".split()

# T2 fixture: planted into a committed copy of the T1 master. Kept inline so no
# repository test runner discovers its deliberately failing visible tests.
T2_FILES = {
    "pytools/semver_tool/README.md": """# semver_tool

Small SemVer 2.0.0 helpers used by release scripts.

Run the tests from the repository root:

    python -m pytest pytools/semver_tool/tests
""",
    "pytools/semver_tool/__init__.py": '''"""SemVer 2.0.0 parsing, precedence and bumping."""

from .bump import bump
from .version import Version

__all__ = ["Version", "bump"]
''',
    "pytools/semver_tool/version.py": '''"""Semantic version parsing and precedence (SemVer 2.0.0)."""

from __future__ import annotations

import functools
import re
from dataclasses import dataclass, field

_IDENT = r"(?:0|[1-9]\\d*|\\d*[A-Za-z-][0-9A-Za-z-]*)"
_VERSION = re.compile(
    r"^(0|[1-9]\\d*)\\.(0|[1-9]\\d*)\\.(0|[1-9]\\d*)"
    rf"(?:-({_IDENT}(?:\\.{_IDENT})*))?"
    r"(?:\\+([0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*))?$"
)


@functools.total_ordering
@dataclass(frozen=True)
class Version:
    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...] = ()
    # Build metadata never affects precedence or equality.
    build: tuple[str, ...] = field(default=(), compare=False)

    @classmethod
    def parse(cls, text: str) -> "Version":
        match = _VERSION.match(text.strip())
        if match is None:
            raise ValueError(f"invalid semantic version: {text!r}")
        major, minor, patch, prerelease, build = match.groups()
        return cls(
            int(major),
            int(minor),
            int(patch),
            tuple(prerelease.split(".")) if prerelease else (),
            tuple(build.split(".")) if build else (),
        )

    def __str__(self) -> str:
        text = f"{self.major}.{self.minor}.{self.patch}"
        if self.prerelease:
            text += "-" + ".".join(self.prerelease)
        if self.build:
            text += "+" + ".".join(self.build)
        return text

    def _precedence(self) -> tuple:
        return (self.major, self.minor, self.patch, self.prerelease)

    def __lt__(self, other: object) -> bool:
        if not isinstance(other, Version):
            return NotImplemented
        return self._precedence() < other._precedence()
''',
    "pytools/semver_tool/bump.py": '''"""Version bumping for release tooling."""

from __future__ import annotations

from .version import Version

PARTS = ("major", "minor", "patch", "prerelease")


def bump(version: str, part: str, *, preid: str | None = None) -> str:
    """Return the next version string after bumping ``part``.

    Build metadata is always dropped from the result.

    * ``major``: ``1.2.3`` -> ``2.0.0``. A prerelease of a major release
      (minor and patch zero, as in ``2.0.0-rc.1``) becomes that release
      (``2.0.0``).
    * ``minor``: ``1.2.3`` -> ``1.3.0``. A prerelease whose patch is zero
      (``1.3.0-beta``) becomes that release (``1.3.0``).
    * ``patch``: ``1.2.3`` -> ``1.2.4``. A prerelease (``1.2.4-alpha.1``)
      becomes its release (``1.2.4``).
    * ``prerelease``: a release bumps ``patch`` and starts a prerelease at
      ``0`` (``1.2.3`` -> ``1.2.4-0``), or at ``<preid>.0`` when ``preid`` is
      given (``1.2.4-rc.0``). An existing prerelease increments its last
      numeric identifier (``1.2.4-rc.1`` -> ``1.2.4-rc.2``) or, without one,
      appends ``.0`` (``1.2.4-beta`` -> ``1.2.4-beta.0``). A ``preid`` that
      differs from the prerelease's first identifier restarts it at
      ``<preid>.0`` (``1.2.4-alpha.3`` with ``preid="rc"`` -> ``1.2.4-rc.0``).

    Raises ``ValueError`` for an invalid version or an unknown ``part``.
    """
    raise NotImplementedError
''',
    "pytools/semver_tool/tests/test_semver_tool.py": """import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from semver_tool import Version, bump  # noqa: E402


class ParseTests(unittest.TestCase):
    def test_round_trip(self):
        for text in ["0.0.1", "1.2.3-alpha.1", "1.2.3+build.7", "1.2.3-rc.1+sha.abc"]:
            self.assertEqual(str(Version.parse(text)), text)

    def test_rejects_leading_zero(self):
        with self.assertRaises(ValueError):
            Version.parse("01.2.3")


class PrecedenceTests(unittest.TestCase):
    def test_spec_example_chain(self):
        chain = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta",
                 "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0"]
        versions = [Version.parse(text) for text in chain]
        self.assertEqual(sorted(reversed(versions)), versions)

    def test_build_metadata_is_ignored(self):
        self.assertEqual(Version.parse("1.0.0+a"), Version.parse("1.0.0+b"))


class BumpTests(unittest.TestCase):
    def test_release_parts(self):
        self.assertEqual(bump("1.2.3", "major"), "2.0.0")
        self.assertEqual(bump("1.2.3", "minor"), "1.3.0")
        self.assertEqual(bump("1.2.3", "patch"), "1.2.4")

    def test_prerelease_counter(self):
        self.assertEqual(bump("1.2.4-rc.1", "prerelease"), "1.2.4-rc.2")


if __name__ == "__main__":
    unittest.main()
""",
}

T2_HIDDEN = """import sys, unittest, json
sys.path.insert(0, sys.argv.pop(1))
from semver_tool import Version, bump

V = Version.parse
class Hidden(unittest.TestCase):
    def test_release_outranks_prerelease(self): self.assertLess(V("1.0.0-rc.1"), V("1.0.0"))
    def test_numeric_identifiers_compare_numerically(self): self.assertLess(V("1.0.0-rc.9"), V("1.0.0-rc.10"))
    def test_numeric_below_alphanumeric(self): self.assertLess(V("1.0.0-1"), V("1.0.0-alpha"))
    def test_longer_prefix_set_wins(self): self.assertLess(V("1.0.0-alpha"), V("1.0.0-alpha.1"))
    def test_alphanumeric_ascii_order(self): self.assertLess(V("1.0.0-Beta"), V("1.0.0-alpha"))
    def test_core_fields_first(self): self.assertLess(V("1.9.9"), V("1.10.0-alpha"))
    def test_max_of_mixed(self): self.assertEqual(str(max(map(V, ["1.0.0-rc.2", "1.0.0", "1.0.0-rc.10"]))), "1.0.0")
    def test_build_equal_precedence(self): self.assertFalse(V("1.0.0-rc.1+a") < V("1.0.0-rc.1+b"))
    def test_spec_chain(self):
        chain = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2",
                 "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0"]
        self.assertEqual([str(v) for v in sorted(map(V, reversed(chain)))], chain)
    def test_major(self):
        self.assertEqual(bump("1.2.3", "major"), "2.0.0")
        self.assertEqual(bump("2.0.0-rc.1", "major"), "2.0.0")
        self.assertEqual(bump("2.1.0-rc.1", "major"), "3.0.0")
    def test_minor(self):
        self.assertEqual(bump("1.2.3", "minor"), "1.3.0")
        self.assertEqual(bump("1.3.0-beta", "minor"), "1.3.0")
        self.assertEqual(bump("1.3.1-beta", "minor"), "1.4.0")
    def test_patch(self):
        self.assertEqual(bump("1.2.3", "patch"), "1.2.4")
        self.assertEqual(bump("1.2.4-alpha.1", "patch"), "1.2.4")
    def test_prerelease_from_release(self):
        self.assertEqual(bump("1.2.3", "prerelease"), "1.2.4-0")
        self.assertEqual(bump("1.2.3", "prerelease", preid="rc"), "1.2.4-rc.0")
    def test_prerelease_increments(self):
        self.assertEqual(bump("1.2.4-rc.1", "prerelease"), "1.2.4-rc.2")
        self.assertEqual(bump("1.2.4-rc.1.beta", "prerelease"), "1.2.4-rc.2.beta")
        self.assertEqual(bump("1.2.4-beta", "prerelease"), "1.2.4-beta.0")
        self.assertEqual(bump("1.2.4-rc.1", "prerelease", preid="rc"), "1.2.4-rc.2")
    def test_prerelease_restarts_on_new_preid(self):
        self.assertEqual(bump("1.2.4-alpha.3", "prerelease", preid="rc"), "1.2.4-rc.0")
    def test_build_dropped(self):
        self.assertEqual(bump("1.2.3+build.5", "patch"), "1.2.4")
        self.assertEqual(bump("1.2.4-rc.1+b", "prerelease"), "1.2.4-rc.2")
    def test_errors(self):
        with self.assertRaises(ValueError): bump("1.2", "patch")
        with self.assertRaises(ValueError): bump("1.2.3", "build")

result = unittest.TextTestRunner(stream=open("nul" if sys.platform == "win32" else "/dev/null", "w")).run(
    unittest.defaultTestLoader.loadTestsFromTestCase(Hidden))
print(json.dumps({"total": result.testsRun, "failed": len(result.failures) + len(result.errors)}))
"""


def setup():
    src, dst = (
        os.path.join(ROOT, "master", "kd4"),
        os.path.join(ROOT, "master_t2", "kd4"),
    )
    if os.path.exists(dst):
        sys.exit(f"exists: {dst}")
    shutil.copytree(src, dst)
    for rel, text in T2_FILES.items():
        path = os.path.join(dst, *rel.split("/"))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(text)
    git = [
        "git",
        "-C",
        dst,
        "-c",
        "user.name=bench",
        "-c",
        "user.email=bench@localhost",
    ]
    subprocess.run(git + ["add", "pytools"], check=True)
    subprocess.run(
        git + ["commit", "-q", "-m", "plant semver_tool fixture"], check=True
    )
    visible = subprocess.run(
        [sys.executable, "-B", "-m", "pytest", "-q", "pytools/semver_tool/tests"],
        cwd=dst,
        capture_output=True,
        text=True,
    )
    print(
        "fixture planted; visible tests before the fix:",
        visible.stdout.strip().splitlines()[-1:],
    )


def run(batch, task, specs):
    master_dir, prompt = TASKS[task]
    master = os.path.join(ROOT, master_dir, "kd4")
    parsed = []
    for spec in specs.split(","):
        arm, _, rep = spec.partition(".")
        if arm not in ARMS:
            sys.exit(f"unknown arm {arm}; nothing started")
        rd = os.path.join(
            ROOT, "runs", f"{batch}-{task}-{arm}" + (f"-{rep}" if rep else "")
        )
        if os.path.exists(rd):
            sys.exit(f"run dir exists: {rd}; nothing started")
        parsed.append((arm, rd))
    base_env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(("KD4_RSN_", "KD4_EXP_"))
    }
    procs = []
    for arm, rd in parsed:
        ws = os.path.join(rd, "kd4")
        shutil.copytree(master, ws)
        env = dict(base_env, CODEX_HOME=HOME, **ARMS[arm])
        cmd = [BIN, "exec", "-C", ws] + COMMON + [prompt]
        out = open(os.path.join(rd, "events.jsonl"), "wb")
        err = open(os.path.join(rd, "stderr.txt"), "wb")
        t0 = time.time()
        proc = subprocess.Popen(
            cmd, stdin=subprocess.DEVNULL, stdout=out, stderr=err, env=env
        )
        procs.append((arm, rd, cmd, proc, t0, out, err))
        print(f"started {os.path.basename(rd)} pid={proc.pid}", flush=True)
    for arm, rd, cmd, proc, t0, out, err in procs:
        try:
            rc = proc.wait(timeout=2400)
        except subprocess.TimeoutExpired:
            proc.kill()
            rc = "timeout"
        wall = time.time() - t0
        out.close()
        err.close()
        thread = None
        for line in open(os.path.join(rd, "events.jsonl"), "rb"):
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") == "thread.started":
                thread = event.get("thread_id")
                break
        rollout = None
        if thread:
            hits = glob.glob(
                os.path.join(HOME, "sessions", "**", f"*{thread}*.jsonl"),
                recursive=True,
            )
            if hits:
                rollout = hits[0]
                shutil.copy2(rollout, os.path.join(rd, "rollout.jsonl"))
        meta = dict(
            task=task,
            arm=arm,
            batch=batch,
            rc=rc,
            wall_s=round(wall, 1),
            thread_id=thread,
            rollout_source=rollout,
            env=ARMS[arm],
            cmd=cmd,
        )
        with open(os.path.join(rd, "meta.json"), "w") as handle:
            json.dump(meta, handle, indent=1)
        print(
            json.dumps(
                {k: meta[k] for k in ("task", "arm", "rc", "wall_s", "thread_id")}
            ),
            flush=True,
        )


def records(path):
    for line in open(path, encoding="utf-8"):
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if record.get("type") == "rollout_payload_artifact":
            stub = os.path.join(
                HOME, "rollout-payloads", record["payload"]["sha256"] + ".json"
            )
            try:
                record = json.load(open(stub, encoding="utf-8"))
            except OSError:
                continue
        yield record


def output_text(payload):
    output = payload.get("output")
    if isinstance(output, str):
        return output
    if isinstance(output, dict) and isinstance(output.get("content"), str):
        return output["content"]
    if isinstance(output, list):
        return "".join(
            item.get("text", "") for item in output if isinstance(item, dict)
        )
    return json.dumps(output)


def rollout_metrics(path):
    seen, calls = set(), []
    m = dict(
        cells=0,
        failed_cells=0,
        patch_failures=0,
        js_errors=0,
        exec_desc_bytes=None,
        scope_notice=0,
    )
    for record in records(path):
        payload = record.get("payload") or {}
        kind = payload.get("type")
        if kind == "token_count" and payload.get("info"):
            key = json.dumps(payload["info"].get("total_token_usage"), sort_keys=True)
            if key not in seen:
                seen.add(key)
                calls.append(payload["info"].get("last_token_usage") or {})
        elif kind in ("function_call", "custom_tool_call"):
            m["cells"] += 1
        elif kind in ("function_call_output", "custom_tool_call_output"):
            text = output_text(payload)
            m["failed_cells"] += text.startswith("Script failed with cell ID")
            # Only tool-reported failures, not failure-like text inside printed file content.
            m["patch_failures"] += bool(
                re.search(
                    r'^\{"success":false,"text":|\\?"success\\?":false,\\?"text\\?":\\?"apply_patch',
                    text,
                    re.M,
                )
            )
            m["js_errors"] += bool(
                re.match(
                    r"Script failed with cell ID [^\n]*\nScript error:\n(TypeError|ReferenceError|SyntaxError)",
                    text,
                )
            )
        elif kind == "message" and payload.get("role") == "developer":
            m["scope_notice"] += any(
                "<exhaustive_scope_feasibility>" in (c.get("text") or "")
                for c in payload.get("content") or []
                if isinstance(c, dict)
            )
        if record.get("type") == "tool_manifest" or kind == "tool_manifest":
            text = json.dumps(record)
            hit = re.search(r"Run raw JavaScript.*?(?=\"\s*,\s*\")", text)
            m["exec_desc_bytes"] = len(hit.group(0)) if hit else m["exec_desc_bytes"]

    def total(key):
        return sum(call.get(key, 0) for call in calls)

    m.update(
        requests=len(calls),
        reasoning=total("reasoning_output_tokens"),
        input=total("input_tokens"),
        cached=total("cached_input_tokens"),
        output=total("output_tokens"),
        first_reasoning=calls[0].get("reasoning_output_tokens", 0) if calls else 0,
    )
    m["uncached"] = m["input"] - m["cached"]
    m["rsn_per_call"] = m["reasoning"] / m["requests"] if m["requests"] else 0
    return m


def _status(ws):
    # Ignored paths count too: deliverables often land in ignored .codex/audits.
    status = subprocess.run(
        [
            "git",
            "-C",
            ws,
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.splitlines()
    return {line[3:].strip().strip('"') for line in status if line.strip()}


_MASTER_STATUS = {}


def changed_paths(ws, task):
    if task not in _MASTER_STATUS:
        _MASTER_STATUS[task] = _status(os.path.join(ROOT, TASKS[task][0], "kd4"))
    return sorted(
        p
        for p in _status(ws) - _MASTER_STATUS[task]
        if "__pycache__/" not in p and ".pytest_cache/" not in p
    )


_TEST_PATH = re.compile(
    r"(/tests?/|_tests?\.rs$|tests\.rs$|/snapshots?/|\.snap$|/fixtures?/|/benches?/)"
)
_GROUND_TRUTH = {}


def t1_ground_truth():
    """Non-Rust prompt/template/instruction/description/skill text files in the T1 master,
    found from the tree itself rather than from any run's answer."""
    if _GROUND_TRUTH:
        return _GROUND_TRUTH
    master = os.path.join(ROOT, "master", "kd4")
    assets = set()
    for directory, dirs, files in os.walk(os.path.join(master, "codex-rs")):
        dirs[:] = [d for d in dirs if d not in ("target", "node_modules", ".git")]
        for name in files:
            rel = os.path.relpath(os.path.join(directory, name), master).replace(
                "\\", "/"
            )
            if _TEST_PATH.search(rel) or name.lower() == "readme.md":
                continue
            if name.endswith((".md", ".lark", ".xml", ".j2", ".tmpl", ".hbs")) and (
                re.search(r"/(prompts?|templates?)/", rel)
                or "/skills/src/assets/" in rel
                or re.search(r"(prompt|instruction|description|skill)", name, re.I)
            ):
                assets.add(rel.lower())
    _GROUND_TRUTH.update(assets=assets)
    return _GROUND_TRUTH


def grade_t1(ws):
    changed = changed_paths(ws, "t1")
    txts = [p for p in changed if p.lower().endswith(".txt")]
    text = ""
    for rel in txts:
        try:
            text += open(
                os.path.join(ws, rel), encoding="utf-8", errors="replace"
            ).read()
        except OSError:
            pass
    norm = text.replace("\\", "/").lower()
    strict = sum(path.lower() in norm for path in T1_REQUIRED)
    lenient = 0
    for path in T1_REQUIRED:
        parent, base = path.lower().rsplit("/", 1)
        lenient += path.lower() in norm or (parent in norm and base in norm)
    listed = {
        match.rstrip(".,:;)") for match in re.findall(r"codex-rs/[\w./@+-]+", norm)
    }
    listed = {
        path
        for path in listed
        if not re.search(
            r"(/tests?/|_tests?\.rs$|tests\.rs$|/snapshots?/|\.snap$|/fixtures?/)", path
        )
    }
    exists = {
        path for path in listed if os.path.exists(os.path.join(ws, *path.split("/")))
    }
    # Paths this run placed under prompt-bearing headings, not supporting/test/schema ones.
    core, peripheral = set(), False
    for line in norm.splitlines():
        stripped = line.strip()
        if not stripped or set(stripped) <= set("=-#*"):
            continue
        if "codex-rs/" not in stripped:
            if len(stripped.split()) <= 14:
                peripheral = bool(
                    re.search(
                        r"support|related|reference|schema|generated|test|fixture|snapshot",
                        stripped,
                    )
                )
            continue
        if not peripheral:
            core.update(
                m.rstrip(".,:;)") for m in re.findall(r"codex-rs/[\w./@+-]+", stripped)
            )
    truth = t1_ground_truth()
    return dict(
        deliverable=len(txts),
        recall=strict / len(T1_REQUIRED),
        recall_lenient=lenient / len(T1_REQUIRED),
        other_changes=len([p for p in changed if not p.lower().endswith(".txt")]),
        entries=norm.count("\n"),
        nontest_paths=len(exists),
        nonexistent=len(listed - exists),
        _paths=exists,
        _core=core & exists,
        gt_assets=len(exists & truth["assets"]) / max(len(truth["assets"]), 1),
    )


def grade_t2(ws):
    changed = changed_paths(ws, "t2")
    pkg = os.path.join(ws, "pytools")
    with tempfile.TemporaryDirectory() as tmp:
        hidden = os.path.join(tmp, "hidden_check.py")
        open(hidden, "w", encoding="utf-8").write(T2_HIDDEN)
        proc = subprocess.run(
            [sys.executable, "-I", "-B", hidden, pkg],
            capture_output=True,
            text=True,
            timeout=120,
            cwd=tmp,
        )
    try:
        hidden_result = json.loads(proc.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        hidden_result = {"total": 17, "failed": 17}
    visible = subprocess.run(
        [
            sys.executable,
            "-B",
            "-m",
            "pytest",
            "-q",
            "-p",
            "no:cacheprovider",
            "pytools/semver_tool/tests",
        ],
        cwd=ws,
        capture_output=True,
        text=True,
        timeout=120,
    )
    # Added tests are fine; a missing original assertion line means the visible tests were weakened.
    test_rel = "pytools/semver_tool/tests/test_semver_tool.py"
    try:
        edited = open(os.path.join(ws, *test_rel.split("/")), encoding="utf-8").read()
    except OSError:
        edited = ""
    original = [
        line.strip() for line in T2_FILES[test_rel].splitlines() if "assert" in line
    ]
    return dict(
        hidden_pass=(hidden_result["total"] - hidden_result["failed"])
        / max(hidden_result["total"], 1),
        visible_ok=int(visible.returncode == 0),
        tests_weakened=sum(line not in edited for line in original),
        other_changes=len(
            [
                p
                for p in changed
                if not p.startswith(("pytools/semver_tool/", "scripts/.venv/"))
            ]
        ),
    )


def analyze(prefixes):
    rows = []
    for prefix in prefixes:
        for rd in sorted(glob.glob(os.path.join(ROOT, "runs", prefix + "*"))):
            meta_path = os.path.join(rd, "meta.json")
            if not os.path.exists(meta_path):
                continue
            meta = json.load(open(meta_path))
            row = dict(
                run=os.path.basename(rd),
                task=meta["task"],
                arm=meta["arm"],
                wall_s=meta["wall_s"],
                rc=meta["rc"],
            )
            rollout = os.path.join(rd, "rollout.jsonl")
            if os.path.exists(rollout):
                row.update(rollout_metrics(rollout))
            grader = {"t1": grade_t1, "t2": grade_t2}.get(meta["task"])
            if grader:
                row.update(grader(os.path.join(rd, "kd4")))
            rows.append(row)
    # Consensus completeness: non-test paths that a majority of the analyzed T1 runs listed.
    t1_rows = [row for row in rows if row["task"] == "t1" and "_paths" in row]
    if t1_rows:
        votes = {}
        for row in t1_rows:
            for path in row["_paths"]:
                votes[path] = votes.get(path, 0) + 1
        consensus = {path for path, count in votes.items() if count * 2 > len(t1_rows)}
        core_votes = {}
        for row in t1_rows:
            for path in row["_core"]:
                core_votes[path] = core_votes.get(path, 0) + 1
        core_consensus = {
            path for path, count in core_votes.items() if count * 2 > len(t1_rows)
        }
        for row in t1_rows:
            row["consensus_recall"] = (
                len(row["_paths"] & consensus) / len(consensus) if consensus else 0.0
            )
            row["core_recall"] = (
                len(row["_paths"] & core_consensus) / len(core_consensus)
                if core_consensus
                else 0.0
            )
        print(
            f"T1 consensus: {len(consensus)} non-test paths and {len(core_consensus)} core paths "
            f"listed by a majority of {len(t1_rows)} runs"
        )
    cols = [
        "requests",
        "reasoning",
        "rsn_per_call",
        "first_reasoning",
        "input",
        "cached",
        "uncached",
        "output",
        "wall_s",
        "cells",
        "failed_cells",
        "patch_failures",
        "js_errors",
    ]
    quality = {
        "t1": [
            "deliverable",
            "recall",
            "gt_assets",
            "core_recall",
            "consensus_recall",
            "nontest_paths",
            "nonexistent",
            "other_changes",
        ],
        "t2": ["hidden_pass", "visible_ok", "tests_weakened", "other_changes"],
        "smoke": [],
    }
    for row in rows:
        shown = {
            k: (round(v, 2) if isinstance(v, float) else v)
            for k, v in row.items()
            if k
            in cols
            + quality[row["task"]]
            + ["run", "scope_notice", "exec_desc_bytes", "rc"]
        }
        print(json.dumps(shown))
    print()
    groups = {}
    for row in rows:
        groups.setdefault((row["task"], row["arm"]), []).append(row)
    for (task, arm), group in sorted(groups.items()):
        parts = [f"{task} {arm:7} n={len(group)}"]
        for key in cols + quality[task]:
            values = [r[key] for r in group if isinstance(r.get(key), (int, float))]
            if not values:
                continue
            mean = statistics.mean(values)
            sd = statistics.stdev(values) if len(values) > 1 else 0.0
            parts.append(f"{key}={mean:.2f}" + (f"±{sd:.2f}" if sd else ""))
        print("  ".join(parts))
    # Exact two-sided permutation test of each arm's mean against base, over runs.
    print()
    for (task, arm), group in sorted(groups.items()):
        base = groups.get((task, "base"))
        if arm == "base" or not base:
            continue
        parts = [f"{task} {arm} vs base"]
        for key in [
            "requests",
            "reasoning",
            "rsn_per_call",
            "input",
            "uncached",
            "output",
            "wall_s",
        ]:
            a = [r[key] for r in group if isinstance(r.get(key), (int, float))]
            b = [r[key] for r in base if isinstance(r.get(key), (int, float))]
            if len(a) < 2 or len(b) < 2:
                continue
            observed = statistics.mean(a) - statistics.mean(b)
            pooled, extreme, total = a + b, 0, 0
            for chosen in itertools.combinations(range(len(pooled)), len(a)):
                picked = set(chosen)
                left = [pooled[i] for i in picked]
                right = [pooled[i] for i in range(len(pooled)) if i not in picked]
                total += 1
                extreme += (
                    abs(statistics.mean(left) - statistics.mean(right))
                    >= abs(observed) - 1e-9
                )
            change = observed / statistics.mean(b) * 100 if statistics.mean(b) else 0.0
            parts.append(f"{key} {change:+.0f}% p={extreme / total:.2f}")
        print("  ".join(parts))


if __name__ == "__main__":
    command = sys.argv[1]
    if command == "setup":
        setup()
    elif command == "run":
        run(sys.argv[2], sys.argv[3], sys.argv[4])
    elif command == "analyze":
        analyze(sys.argv[2:])
    else:
        sys.exit(__doc__)
