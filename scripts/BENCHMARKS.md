# Benchmark the Windows fork

This setup keeps the local **Windows `codex.exe` and its matching code-mode host**
on Windows. Harbor runs task commands and verifiers in Linux Modal sandboxes.
It never downloads upstream Codex or builds a Linux fork.

## What is being measured

`scripts.harbor_windows_codex:WindowsCodex` is a custom Harbor agent. It talks to
the fork's existing app-server over stdio and exposes one dynamic tool,
`sandbox_terminal`, backed by Harbor's environment execution API. Native host
execution environments are disabled (`CODEX_EXEC_SERVER_URL=none` and empty
thread/turn environments). Web search, plugins, apps, hooks and subagents are
disabled. The model, reasoning loop and code-mode implementation remain the
Windows fork's; the terminal tool interface and serialization are custom.

Do **not** label these results as measurements of the native Windows tool suite,
the standard Harbor Codex adapter, or an officially certified leaderboard run.
Commands are serialized, noninteractive, and limited to 300 seconds per call;
background process groups are stopped when the call ends. Longer tasks need
multiple bounded commands. Full command output stays in the
sandbox at the returned output path; the first 24 KB is returned to the model.

## Prerequisites

- Windows fork runtime bundle, with `codex.exe` and `codex-code-mode-host.exe`
  together. No PATH-based `codex` lookup or fallback is used.
- Python 3.11+ and uv. The already installed tool versions used here are
  `harbor==0.24.0` and `modal==1.6.1`. If reinstalling:
  `uv tool install 'harbor[modal]==0.24.0' --with 'modal==1.6.1' --with-executables-from modal`.
- Authenticate Modal yourself with `modal setup`; this may open a browser.
  Terminal-Bench 4 includes GPU tasks, so the account needs the relevant capacity.
- Fork login (`auth.json` in the fork home), or a host-side `OPENAI_API_KEY` or
  `CODEX_API_KEY`. Never use Harbor `--ae` with this adapter: Harbor injects those
  variables into the task sandbox. The adapter rejects them.

The fork's config is read only to choose the model and reasoning effort. A
per-trial temporary home contains only a copy of fork authentication; user
instructions/config, MCP servers, skills, and personal plugins are not copied.
The temporary home is removed after the app-server stops. The fork Desktop home
and official upstream home are not changed. A refreshed temporary login is not
copied back; reauthenticate the fork if its source login expires.

Only OpenAI provider model IDs are supported by this launcher. A custom provider
in your personal config is not imported; pass an explicit compatible model.

## Safe preflight (no model calls or sandbox creation)

Run from the checkout root in PowerShell. Set `$forkHome` to your fork home, not
the official `.codex` home:

```powershell
$forkHome = $env:CODEX_HOME
python scripts\benchmark_agents.py terminal --fork-home $forkHome --check
python scripts\benchmark_agents.py swe --fork-home $forkHome --check
```

Without `--execute`, the launcher only prints a plan. `--check` additionally
checks the pinned dependencies, runtime prerequisites, SWE cache checksums, and
Harbor's resolved configuration. It does not prove account access, GPU capacity,
image availability, or live model access. `--harbor-python` can point to a
different Python environment containing the pinned Harbor/Modal versions.

## Run a single task only when ready for charges

```powershell
python scripts\benchmark_agents.py terminal --fork-home $forkHome --job-name tb4-smoke --execute
python scripts\benchmark_agents.py swe --fork-home $forkHome --job-name swe-smoke --execute
```

Defaults: one task, one concurrent sandbox, one attempt, no retries. Use
`--task <name>` to select a task, `--tasks <count>` to increase the limit,
`--concurrency <count>` to increase concurrency, and `--model <id>` /
`--reasoning-effort <level>` for explicit model settings. Existing job names are
rejected rather than resumed or overwritten. Nothing is published to a registry
or leaderboard. Model prompts and commands still go to your model provider, and
task execution goes to Modal; this is not an offline/local-compute benchmark.

Jobs are kept under `<fork-home>\benchmarks\jobs`. Each trial contains
`agent/runtime.json` with the binary hashes, `app-server.jsonl`, stderr, and
terminal call records. These may contain task/code content; keep them private.

## SWE-bench Pro V2: capture, then fresh-sandbox regrade

The cache prepared by the previous session is pinned to revision
`66f92766bba642462d4bbe5479e83f91f9211862`, under
`<fork-home>\benchmarks\swe-bench-pro-v2-66f92766bba6`. Preflight verifies the
original archive hash and all 6,436 checksummed files. Do not modify this cache.

SWE solving preserves the task's no-network agent phase, disables web tools,
captures `agent/model.patch`, and **does not run the verifier in the agent's
sandbox**. Model calls happen on Windows, so no model-host network exception is
needed inside the sandbox. The solve budget reserves cleanup time within the
task's 50-minute limit. Patches are relative to the initial commit, so committing
changes during a solve does not make those changes disappear from the patch.

After the solve job finishes:

```powershell
python scripts\benchmark_agents.py regrade --fork-home $forkHome `
  --source-job "$forkHome\benchmarks\jobs\swe-smoke" --check
python scripts\benchmark_agents.py regrade --fork-home $forkHome `
  --source-job "$forkHome\benchmarks\jobs\swe-smoke" --job-name swe-smoke-regrade --execute
```

Regrade selects every source task and applies only its patch to a fresh sandbox,
then Harbor runs the original verifier. Missing or ambiguous patches and failed
applications are errors, not silently accepted empty patches. An explicitly
captured empty patch is valid and should fail the benchmark tests. Report the
fresh regrade score, retain the solve logs, and disclose this custom harness.
Live network-isolation and reference/empty-patch probes are still required
before trusting benchmark scores; no live score is established by unit tests.

## Offline validation

Use Harbor's Python so the tests can import its adapter contract:

```powershell
$harborPython = Join-Path (uv tool dir) 'harbor\Scripts\python.exe'
$env:CODEX_BENCHMARK_TEST_BINARY = Join-Path $forkHome 'bin\codex.exe'
& $harborPython -m unittest scripts.test_harbor_windows_codex -v
```

The native smoke tests use a scripted loopback model and fake Harbor environment,
not a real provider or Modal. They exercise the actual Windows executable,
dynamic tool round trips, code mode, cleanup and host-tool exclusion.
For an additional syntax-only check of generated Linux shell commands, set
`CODEX_BENCHMARK_TEST_BASH` to an installed Bash executable before running tests.

References: [Terminal-Bench 4 release](https://github.com/harbor-framework/terminal-bench/releases/tag/v4.0.0),
[run instructions](https://www.tbench.ai/run),
[SWE-bench Pro V2 protocol](https://github.com/scaleapi/SWE-bench_Pro-os/blob/main/v2/README.md).
