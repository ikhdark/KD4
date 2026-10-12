"""Bounded live-model Rust test-authoring controls, not a harness-improvement claim.

Two authored std-only contracts use real files, trait dispatch and worker threads.
The Python pair repairs undiscovered tests from real baseline/corrected runner
receipts; it is one-shot source authoring, not an interactive test-repair loop.
Author saves model source without executing it. After source review, evaluate
compiles the unchanged tests against correct and compile-valid defective crates.
No Cargo, repository mutation, installed-runtime replacement or general mutation
engine is used. Identical-evidence arms measure a control, not a patched binary.
Exit zero means mechanical checks succeeded, not that runtime failure reasons
were reviewed. Reported candidates still require independent assertion review.
"""

from __future__ import annotations

# Support direct execution (including python -I) from any working directory.
if __package__ in (None, ""):
    import sys as _sys
    from pathlib import Path as _Path

    _sys.path.insert(0, str(_Path(__file__).resolve().parents[2]))


import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.benchmark_scripts.benchmark_audit21_comprehension import BenchmarkServer, SAFE_CONFIG, digest


BACKEND = r"""
use std::{io, path::{Path, PathBuf}, sync::atomic::{AtomicBool, Ordering}};
pub trait Store: Send + Sync { fn append(&self, value: &str) -> io::Result<()>; }
pub struct FileStore { root: PathBuf, fail: AtomicBool }
impl FileStore {
    pub fn open(root: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        std::fs::write(root.join("lease"), b"owned")?;
        Ok(Self { root: root.into(), fail: AtomicBool::new(false) })
    }
    pub fn fail_next(&self) { self.fail.store(true, Ordering::SeqCst); }
    pub fn contents(&self) -> io::Result<String> {
        match std::fs::read_to_string(self.root.join("committed")) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()), other => other,
        }
    }
}
impl Store for FileStore {
    fn append(&self, value: &str) -> io::Result<()> {
        use std::io::Write;
        if self.fail.swap(false, Ordering::SeqCst) { return Err(io::Error::other("injected storage failure")); }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(self.root.join("committed"))?;
        writeln!(file, "{value}")?;
        file.sync_all()
    }
}
impl Drop for FileStore { fn drop(&mut self) { let _ = std::fs::remove_file(self.root.join("lease")); } }
"""

QUEUE = r"""
pub use backend::{FileStore, Store};
use std::{collections::VecDeque, io, sync::Arc};
pub struct Queue { store: Arc<dyn Store>, pending: VecDeque<String> }
impl Queue {
    pub fn new(store: Arc<dyn Store>) -> Self { Self { store, pending: VecDeque::new() } }
    pub fn push(&mut self, value: &str) { self.pending.push_back(value.into()); }
    pub fn pending(&self) -> usize { self.pending.len() }
    pub fn step(&mut self) -> io::Result<Option<String>> {
        let Some(value) = self.pending.front().cloned() else { return Ok(None) };
        self.store.append(&value)?;
        self.pending.pop_front();
        Ok(Some(value))
    }
}
"""

TASK = r"""
use std::{future::Future, pin::Pin, sync::{Arc, Mutex, Condvar, mpsc}, task::{Context, Poll, Wake, Waker}, thread};
#[derive(Debug, PartialEq, Eq, Clone, Copy)] pub enum TaskError { Cancelled }
#[derive(Default)] struct State { released: bool, cancelled: bool, result: Option<Result<usize,TaskError>>, live: usize, effects: usize }
type Shared = Arc<(Mutex<State>, Condvar)>;
pub struct Probe(Shared);
impl Probe {
    pub fn live(&self) -> usize { self.0.0.lock().unwrap().live }
    pub fn effects(&self) -> usize { self.0.0.lock().unwrap().effects }
}
pub struct Task { shared: Shared, started: Option<mpsc::Receiver<()>>, worker: Option<thread::JoinHandle<()>> }
impl Task {
    pub fn spawn() -> (Self, Probe) {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let state = shared.clone();
        let (tx,rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut s = state.0.lock().unwrap();
            s.live += 1;
            tx.send(()).unwrap();
            while !s.released { s = state.1.wait(s).unwrap(); }
            if s.cancelled { s.result = Some(Err(TaskError::Cancelled)); }
            else { s.effects += 1; s.result = Some(Ok(7)); }
            s.live -= 1;
        });
        (Self { shared: shared.clone(), started: Some(rx), worker: Some(worker) }, Probe(shared))
    }
    pub fn wait_started(&mut self) { self.started.take().unwrap().recv_timeout(std::time::Duration::from_secs(2)).unwrap(); }
    pub fn release(&self) { let mut s = self.shared.0.lock().unwrap(); s.released=true; self.shared.1.notify_all(); }
    fn join(&mut self) { if let Some(worker) = self.worker.take() { worker.join().unwrap(); } }
    pub fn cancel(&mut self) {
        { let mut s=self.shared.0.lock().unwrap(); s.cancelled=true; s.released=true; self.shared.1.notify_all(); }
        self.join();
    }
    pub fn finish(mut self) -> Result<usize,TaskError> { self.join(); match poll_once(&mut self) { Poll::Ready(r)=>r, _=>panic!("joined task is pending") } }
}
impl Future for Task {
    type Output = Result<usize,TaskError>;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.shared.0.lock().unwrap().result { Some(r)=>Poll::Ready(r), None=>Poll::Pending }
    }
}
impl Drop for Task { fn drop(&mut self) { self.cancel(); } }
struct Noop; impl Wake for Noop { fn wake(self: Arc<Self>) {} }
pub fn poll_once(task: &mut Task) -> Poll<Result<usize,TaskError>> {
    let waker=Waker::from(Arc::new(Noop)); Pin::new(task).poll(&mut Context::from_waker(&waker))
}
"""

CONTRACTS = {
    "queue": """Independent authored contract: Queue::step commits jobs FIFO through the supplied Store trait object. Only a successful append consumes the head. Storage errors propagate and retain that job for retry. FileStore::contents after all owners are dropped and a fresh store is opened contains each successfully committed job followed by a newline, exactly once, in order. FileStore::open creates its directory and lease file; dropping its last Arc owner removes lease but preserves committed data. Use actual FileStore, not a fake Store, for durable-state claims. Exercise distinct jobs, injected failure+retry, reopen, and last-owner cleanup. Each test gets its own temporary directory and cleans it up.""",
    "task": """Independent authored contract: Task::spawn starts a real background thread owning one resource. wait_started is a deterministic barrier. Before release/cancel, poll_once returns Pending and the Probe reports live=1, effects=0. release then finish returns Ok(7), with effects=1 and live=0. cancel while blocked completes worker cancellation before returning: subsequent poll is Ready(Err(Cancelled)), effects=0 and live=0. Dropping a blocked Task also cancels+joins and releases its resource before returning; a separately retained Probe remains observable. Never use sleeps as proof. This bounded manually-polled Future contract does not promise waker-driven scheduling or cancellation after completion.""",
}

CONTROLS = {
    "queue": r"""
use fixture::*; use std::sync::Arc;
#[test] fn contract() {
    let root=std::env::current_dir().unwrap().join("scratch");
    assert!(!root.exists());
    let store=Arc::new(FileStore::open(&root).unwrap()); assert!(root.join("lease").exists());
    let mut queue=Queue::new(store.clone()); queue.push("first"); queue.push("second");
    store.fail_next(); assert!(queue.step().is_err()); assert_eq!(queue.pending(),2); assert_eq!(store.contents().unwrap(),"");
    assert_eq!(queue.step().unwrap(),Some("first".into())); assert_eq!(queue.step().unwrap(),Some("second".into()));
    assert_eq!(queue.step().unwrap(),None); drop(queue); drop(store); assert!(!root.join("lease").exists());
    let reopened=FileStore::open(&root).unwrap(); assert_eq!(reopened.contents().unwrap(),"first\nsecond\n");
    drop(reopened); std::fs::remove_dir_all(root).unwrap();
}
""",
    "task": r"""
use fixture::*; use std::task::Poll;
#[test] fn cancelled() { let (mut task,p)=Task::spawn(); task.wait_started(); assert_eq!(p.live(),1); assert_eq!(poll_once(&mut task),Poll::Pending); task.cancel(); assert_eq!(poll_once(&mut task),Poll::Ready(Err(TaskError::Cancelled))); assert_eq!(p.effects(),0); assert_eq!(p.live(),0); }
#[test] fn dropped() { let (mut task,p)=Task::spawn(); task.wait_started(); drop(task); assert_eq!(p.live(),0); assert_eq!(p.effects(),0); }
#[test] fn completed() { let (mut task,p)=Task::spawn(); task.wait_started(); task.release(); assert_eq!(task.finish(),Ok(7)); assert_eq!(p.effects(),1); assert_eq!(p.live(),0); }
""",
}

# Every mutation has one exact authored source site; failures to compile are not kills.
MUTATIONS = {
    "queue": {
        "lifo": (
            "api",
            "self.pending.front().cloned()",
            "self.pending.back().cloned()",
        ),
        "lost_persistence": (
            "backend",
            'writeln!(file, "{value}")?;',
            "let _ = (&mut file, value);",
        ),
        "swallowed_error": (
            "api",
            "self.store.append(&value)?;",
            "let _ = self.store.append(&value);",
        ),
        "leaked_lease": (
            "backend",
            'let _ = std::fs::remove_file(self.root.join("lease"));',
            "let _ = &self.root;",
        ),
    },
    "task": {
        "ignored_cancel": ("api", "s.cancelled=true;", "s.cancelled=false;"),
        "detached_on_drop": (
            "api",
            "fn drop(&mut self) { self.cancel(); }",
            "fn drop(&mut self) {}",
        ),
        "early_ready": ("api", "None=>Poll::Pending", "None=>Poll::Ready(Ok(7))"),
        "leaked_resource": ("api", "s.live -= 1;", "s.live -= 0;"),
    },
}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2), encoding="utf-8")


def command(args, cwd, timeout=30):
    start = time.perf_counter()
    try:
        result = subprocess.run(
            args, cwd=cwd, capture_output=True, text=True, timeout=timeout
        )
        return {
            "argv": [str(a) for a in args],
            "exit_code": result.returncode,
            "stdout": result.stdout,
            "stderr": result.stderr,
            "wall_seconds": time.perf_counter() - start,
            "timed_out": False,
        }
    except subprocess.TimeoutExpired as error:
        return {
            "argv": [str(a) for a in args],
            "exit_code": None,
            "timed_out": True,
            "stdout": str(error.stdout or ""),
            "stderr": str(error.stderr or ""),
            "wall_seconds": time.perf_counter() - start,
        }


def completed_exact_run(run):
    """Require libtest's terminal count, not its pre-execution startup marker."""
    if run["timed_out"] or run["exit_code"] not in (0, 101):
        return False
    match = re.search(
        r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;",
        run["stdout"],
        re.MULTILINE,
    )
    if match is None:
        return False
    status, passed, failed, ignored = match.groups()
    return (status, int(passed), int(failed), int(ignored), run["exit_code"]) in {
        ("ok", 1, 0, 0, 0),
        ("FAILED", 0, 1, 0, 101),
    }


def evaluate_source(rustc, directory, case, test_source):
    results = []
    for variant in ("correct", *MUTATIONS[case]):
        work = directory / variant
        work.mkdir(parents=True, exist_ok=False)
        sources = {"backend": BACKEND, "api": QUEUE if case == "queue" else TASK}
        if variant != "correct":
            owner, before, after = MUTATIONS[case][variant]
            if sources[owner].count(before) != 1:
                raise ValueError("mutation does not have one exact source site")
            sources[owner] = sources[owner].replace(before, after)
        for name, source in sources.items():
            (work / f"{name}.rs").write_text(source, encoding="utf-8")
        (work / "tests.rs").write_text(test_source, encoding="utf-8")
        stages = [
            command(
                [
                    rustc,
                    "--edition=2021",
                    "--crate-name",
                    "backend",
                    "--crate-type=rlib",
                    "backend.rs",
                    "-o",
                    "libbackend.rlib",
                ],
                work,
            )
        ]
        if stages[-1]["exit_code"] == 0:
            stages.append(
                command(
                    [
                        rustc,
                        "--edition=2021",
                        "--crate-name",
                        "fixture",
                        "--crate-type=rlib",
                        "api.rs",
                        "--extern",
                        "backend=libbackend.rlib",
                        "-o",
                        "libfixture.rlib",
                    ],
                    work,
                )
            )
        if stages[-1]["exit_code"] == 0:
            stages.append(
                command(
                    [
                        rustc,
                        "--edition=2021",
                        "--test",
                        "tests.rs",
                        "--extern",
                        "fixture=libfixture.rlib",
                        "-L",
                        ".",
                        "-o",
                        "tests.exe",
                    ],
                    work,
                )
            )
        compiled = len(stages) == 3 and all(stage["exit_code"] == 0 for stage in stages)
        listing = (
            command([str(work / "tests.exe"), "--list"], work) if compiled else None
        )
        tests = (
            []
            if not listing
            else [
                line[:-6]
                for line in listing["stdout"].splitlines()
                if line.endswith(": test")
            ]
        )
        runs = [
            command(
                [str(work / "tests.exe"), name, "--exact", "--nocapture"],
                work,
                timeout=10,
            )
            for name in tests
        ]
        # Exact count/ignored evidence is checked, not a bare successful process.
        executed = bool(runs) and all(
            "1 ignored" not in run["stdout"] and "running 1 test" in run["stdout"]
            for run in runs
        )
        execution_complete = (
            compiled
            and listing["exit_code"] == 0
            and executed
            and all(completed_exact_run(run) for run in runs)
        )
        passed = execution_complete and all(run["exit_code"] == 0 for run in runs)
        runtime_failure_candidate = execution_complete and any(
            run["exit_code"] == 101 and "panicked at" in run["stderr"] for run in runs
        )
        results.append(
            {
                "variant": variant,
                "compiled": compiled,
                "tests": tests,
                "executed": executed,
                "execution_complete": execution_complete,
                "passed": passed,
                "runtime_failure_candidate": runtime_failure_candidate,
                "compile": stages,
                "listing": listing,
                "runs": runs,
            }
        )
    write_json(directory / "results.json", results)
    return {
        "case": case,
        "correct_passed": results[0]["passed"],
        "counterfactual_review": "Runtime failures require manual intended-assertion review; panic alone is not a behavioral kill.",
        "mutants": {
            row["variant"]: {
                "compiled": row["compiled"],
                "runtime_failure_candidate": row["runtime_failure_candidate"],
                "false_acceptance": row["passed"],
            }
            for row in results[1:]
        },
        "test_sha256": hashlib.sha256(test_source.encode()).hexdigest(),
    }


async def author(args):
    args.output.mkdir(parents=True, exist_ok=False)
    python_pair = args.phase == "python-author"
    packets = python_packets(args.output) if python_pair else None
    report = {
        "scope": __doc__,
        "status": "running",
        "model": args.model,
        "effort": args.effort,
        "binary_sha256": digest(args.binary),
        "script_sha256": digest(Path(__file__)),
        "arms": "baseline branch versus corrected public Python runner evidence"
        if python_pair
        else "identical evidence controls; no baseline/candidate improvement inference",
        "trials": [],
    }
    started = time.perf_counter()
    try:
        with tempfile.TemporaryDirectory(prefix="kd4-test-integrity-auth-") as tmp:
            home = Path(tmp)
            shutil.copyfile(args.fork_home / "auth.json", home / "auth.json")
            work = home / "work"
            work.mkdir()
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("CODEX_") or k == "CODEX_API_KEY"
            }
            env.update(CODEX_HOME=str(home), CODEX_EXEC_SERVER_URL="none")
            argv = [str(args.binary), "app-server", "--listen", "stdio://"]
            for key, value in SAFE_CONFIG.items():
                argv += ["-c", f"{key}={json.dumps(value)}"]
            with (
                (args.output / "events.jsonl").open("x", encoding="utf-8") as log,
                (args.output / "stderr.log").open("xb") as stderr,
            ):
                process = await asyncio.create_subprocess_exec(
                    *argv,
                    cwd=work,
                    env=env,
                    stdin=asyncio.subprocess.PIPE,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=stderr,
                    limit=16 * 1024 * 1024,
                    creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
                )
                try:
                    server = BenchmarkServer(process, log, work / "unused", "")
                    await asyncio.wait_for(
                        server.request(
                            "initialize",
                            {
                                "clientInfo": {
                                    "name": "kd4_test_integrity",
                                    "version": "1",
                                },
                                "capabilities": {"experimentalApi": True},
                            },
                        ),
                        30,
                    )
                    await server.send({"method": "initialized", "params": {}})
                    for case in ("python",) if python_pair else ("queue", "task"):
                        for arm in (
                            ("baseline", "candidate")
                            if python_pair
                            else ("control_a", "control_b")
                        ):
                            (
                                server.answers,
                                server.calls,
                                server.completed,
                                server.usage,
                            ) = [], [], None, None
                            trial_start = time.perf_counter()
                            thread = await asyncio.wait_for(
                                server.request(
                                    "thread/start",
                                    {
                                        "model": args.model,
                                        "cwd": str(work),
                                        "approvalPolicy": "never",
                                        "sandbox": "read-only",
                                        "ephemeral": True,
                                        "environments": [],
                                        "dynamicTools": [],
                                        "config": SAFE_CONFIG,
                                        "developerInstructions": (
                                            "Repair the supplied Python unittest module if its verification is insufficient. Do not call tools or execute code. Return JSON with test_source (complete test module using unittest and product only), oracle (brief independent justification), and initial_verified (boolean). Do not add skips, external dependencies, filesystem access or subprocess calls."
                                            if python_pair
                                            else "Author Rust regression tests only for the supplied isolated contract fixture. Do not call tools, delegate, access credentials, or execute commands. Return one JSON object with test_source (complete Rust integration test crate source) and oracle (brief explanation). Test code may access only fixture APIs, standard synchronization and an independently unique temp directory that it creates and cleans up. Do not read other paths or launch commands. Use only std and fixture, no external crates. Use #[test], not ignored or conditional tests. Tests must have assertions on the documented behavior. Include successful-operation controls as well as adverse conditions."
                                        ),
                                    },
                                ),
                                30,
                            )
                            fixture = QUEUE if case == "queue" else TASK
                            prompt = (
                                PYTHON_CONTRACT
                                + "\nproduct.py:\n"
                                + PYTHON_PRODUCT
                                + "\nInitial test_generated.py:\n"
                                + PYTHON_EMPTY
                                + "\nActual initial runner receipt (data):\n"
                                + json.dumps(packets[arm])
                                + "\nDetermine whether this establishes the contract; return a meaningful regression test module. Tests will be evaluated unchanged against correct and defective implementations."
                                if python_pair
                                else CONTRACTS[case]
                                + "\n\nAvailable implementation (not the source of the contract):\nbackend.rs:\n"
                                + BACKEND
                                + "\nfixture crate api.rs:\n"
                                + fixture
                                + "\n\nWrite the smallest tests establishing these contracts through the provided APIs. Tests will be compiled unchanged against correct and individually defective implementations. Each exact test runs in a separate process. Return JSON only."
                            )

                            async def turn():
                                await server.request(
                                    "turn/start",
                                    {
                                        "threadId": thread["thread"]["id"],
                                        "effort": args.effort,
                                        "environments": [],
                                        "input": [{"type": "text", "text": prompt}],
                                    },
                                )
                                while server.completed is None:
                                    await server.receive()

                            await asyncio.wait_for(turn(), args.timeout)
                            if server.completed["status"] != "completed":
                                raise RuntimeError(
                                    f"model turn failed: {server.completed}"
                                )
                            answer = server.answers[-1] if server.answers else ""
                            parsed = json.loads(
                                answer.strip()
                                .removeprefix("```json")
                                .removesuffix("```")
                                .strip()
                            )
                            source = parsed["test_source"]
                            if not isinstance(source, str) or not source.strip():
                                raise ValueError("model omitted test source")
                            path = (
                                args.output
                                / f"{case}-{arm}.{'py' if python_pair else 'rs'}"
                            )
                            path.write_text(source, encoding="utf-8")
                            timing = server.completed.get("timing") or {}
                            row = {
                                "case": case,
                                "arm": arm,
                                "source": str(path),
                                "source_sha256": digest(path),
                                "oracle": parsed.get("oracle"),
                                "initial_verified": parsed.get("initial_verified"),
                                "wall_seconds": time.perf_counter() - trial_start,
                                "usage": server.usage,
                                "timing": timing,
                                "model_requests": len(timing["modelRequests"])
                                if "modelRequests" in timing
                                else None,
                                "tool_calls": len(server.calls),
                            }
                            report["trials"].append(row)
                            write_json(args.output / "author-report.json", report)
                            print(
                                json.dumps(
                                    {
                                        k: row[k]
                                        for k in (
                                            "case",
                                            "arm",
                                            "wall_seconds",
                                            "model_requests",
                                            "tool_calls",
                                        )
                                    }
                                ),
                                flush=True,
                            )
                finally:
                    if process.returncode is None:
                        process.stdin.close()
                        try:
                            await asyncio.wait_for(process.wait(), 5)
                        except TimeoutError:
                            process.kill()
                            await process.wait()
        report["status"] = "completed_source_review_required"
    except Exception as error:
        report["status"] = "blocked"
        report["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        report["wall_seconds"] = time.perf_counter() - started
        write_json(args.output / "author-report.json", report)


PYTHON_PRODUCT = "def reserve(capacity, used, requested):\n    if requested < 0:\n        raise ValueError('negative request')\n    return used + requested <= capacity\n"
PYTHON_EMPTY = "from product import reserve\ndef test_reserve():\n    assert reserve(10, 6, 4) is True\n"
PYTHON_CONTRACT = "Independent contract: reserve(capacity,used,requested) returns True exactly when used+requested<=capacity for nonnegative integer arguments. A negative requested value raises ValueError. Cover exact capacity, overflow, ordinary available capacity, zero request and negative request. Use production reserve, no mocks. Initial unittest command was meant to verify this behavior."


def python_candidate_command(root, interpreter):
    repository = Path(__file__).resolve().parents[2]
    uv_prefix = ["uv", "run", "--frozen", "--project", str(repository / "scripts")]
    code = (
        f"import sys; from pathlib import Path; sys.path.insert(0,{str(repository)!r}); "
        "from scripts.maintenance import root_maintenance as owner; "
        f"owner.REPO_ROOT=Path({str(root)!r}); owner.UV_RUN_SCRIPTS={uv_prefix!r}; "
        "raise SystemExit(owner.main(['test-python','--module','test_generated']))"
    )
    return [interpreter, "-B", "-c", code]


def python_packets(directory):
    interpreter = pinned_python(directory)
    fixture = directory / "python-fixture"
    fixture.mkdir()
    (fixture / "product.py").write_text(PYTHON_PRODUCT, encoding="utf-8")
    (fixture / "test_generated.py").write_text(PYTHON_EMPTY, encoding="utf-8")
    packets = {
        "baseline": command(
            [interpreter, "-B", "-m", "unittest", "test_generated", "-v"], fixture
        ),
        "candidate": command(python_candidate_command(fixture, interpreter), fixture),
    }
    write_json(directory / "python-runner-packets.json", packets)
    if packets["baseline"]["exit_code"] != 0 or packets["candidate"]["exit_code"] == 0:
        raise ValueError(
            "actual runner receipts do not demonstrate zero-test behavior change"
        )
    return packets


def pinned_python(directory):
    project = Path(__file__).resolve().parents[1]
    result = command(
        [
            "uv",
            "run",
            "--frozen",
            "--project",
            str(project),
            "python",
            "-c",
            "import sys; print(sys.executable)",
        ],
        directory,
    )
    if result["exit_code"] != 0:
        raise RuntimeError(f"pinned interpreter discovery failed: {result['stderr']}")
    return result["stdout"].strip()


def evaluate_python(directory, source):
    interpreter = pinned_python(directory.parent)
    variants = {
        "correct": PYTHON_PRODUCT,
        "reject_exact_capacity": PYTHON_PRODUCT.replace("<= capacity", "< capacity"),
        "accept_overflow": PYTHON_PRODUCT.replace(
            "used + requested <= capacity", "True"
        ),
        "accept_negative": PYTHON_PRODUCT.replace("if requested < 0:", "if False:"),
    }
    rows = []
    for name, product in variants.items():
        root = directory / name
        root.mkdir(parents=True, exist_ok=False)
        (root / "product.py").write_text(product, encoding="utf-8")
        (root / "test_generated.py").write_text(source, encoding="utf-8")
        compile(product, str(root / "product.py"), "exec")
        compile(source, str(root / "test_generated.py"), "exec")
        result = command(python_candidate_command(root, interpreter), root)
        rows.append(
            {
                "variant": name,
                "compiled": True,
                "passed": result["exit_code"] == 0,
                "runtime_failure_candidate": result["exit_code"] not in (0, None)
                and "AssertionError" in result["stderr"],
                "result": result,
            }
        )
    write_json(directory / "results.json", rows)
    return {
        "case": "python",
        "correct_passed": rows[0]["passed"],
        "test_sha256": hashlib.sha256(source.encode()).hexdigest(),
        "counterfactual_review": "Assertion failures require independent intended-reason review.",
        "mutants": {
            row["variant"]: {
                "compiled": True,
                "runtime_failure_candidate": row["runtime_failure_candidate"],
                "false_acceptance": row["passed"],
            }
            for row in rows[1:]
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "phase",
        choices=("controls", "author", "evaluate", "python-author", "python-evaluate"),
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rustc", default="rustc")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--fork-home", type=Path)
    parser.add_argument("--model", default="gpt-6-astra")
    parser.add_argument("--effort", default="xhigh")
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument("--reviewed-sources", action="store_true")
    args = parser.parse_args()
    args.output = args.output.resolve()
    if args.phase in ("author", "python-author"):
        if args.binary is None or args.fork_home is None:
            parser.error("author requires --binary and --fork-home")
        asyncio.run(author(args))
        return 0
    if args.phase in ("evaluate", "python-evaluate") and not args.reviewed_sources:
        parser.error("inspect generated source first, then pass --reviewed-sources")
    if args.phase == "controls":
        args.output.mkdir(parents=True, exist_ok=False)
        sources = [(case, "authored", source) for case, source in CONTROLS.items()]
    else:
        report = json.loads(
            (args.output / "author-report.json").read_text(encoding="utf-8")
        )
        if report["status"] != "completed_source_review_required":
            raise ValueError("author phase is incomplete")
        sources = []
        for row in report["trials"]:
            path = Path(row["source"])
            if digest(path) != row["source_sha256"]:
                raise ValueError("generated test source changed after authorship")
            sources.append((row["case"], row["arm"], path.read_text(encoding="utf-8")))
    results = [
        {
            **(
                evaluate_python(args.output / f"{case}-{arm}-execution", source)
                if case == "python"
                else evaluate_source(
                    args.rustc, args.output / f"{case}-{arm}-execution", case, source
                )
            ),
            "arm": arm,
        }
        for case, arm, source in sources
    ]
    write_json(
        args.output / "evaluation-summary.json",
        {
            "scope": __doc__,
            "evaluation_script_sha256": digest(Path(__file__)),
            "status": "mechanical_checks_only_manual_assertion_review_pending",
            "rustc": command([args.rustc, "--version"], args.output),
            "results": results,
        },
    )
    print(json.dumps(results, indent=2))
    return (
        0
        if all(
            row["correct_passed"]
            and all(
                value["compiled"] and value["runtime_failure_candidate"]
                for value in row["mutants"].values()
            )
            for row in results
        )
        else 1
    )


if __name__ == "__main__":
    raise SystemExit(main())
