---
name: harness-tools
description: Inventory repository sources, audit Codex sessions and tool outputs, inspect validation receipts, and compare session performance. Use for file inventories, supplied session logs, repeated-work investigations, or evidence-backed performance comparisons in any repository.
---

# Harness Tools

These executable tools are installed with the harness, independently of the target
repository. Resolve `scripts/harness_tools.py` relative to this SKILL.md, not the
working directory. Keep the working directory in the user's repository; pass
explicit repository, session, and report paths. Do not search for a KD4 checkout.

Use the existing Python 3.11+ interpreter (`python` or `python3`). Invoke with
`-I -B` to isolate imports from the target repository and avoid bytecode writes:

```text
python -I -B /absolute/skill/path/scripts/harness_tools.py --describe
```

The returned catalog identifies commands and their contract flags. Read only the
needed contract, reuse it, and execute the owning command. Do not read helper
implementations merely to discover invocation syntax. In legacy examples printed
by an owner, replace `python scripts/NAME.py` with the installed launcher and its
matching command; never assume the target repository contains those scripts.

## Commands

| Command | Contract | Use |
| --- | --- | --- |
| `inventory` | `--describe` | Git-backed source inventory with explicit `--root`, retained queries/state and bounded output. Git is required for source enumeration, not for routine status checks. |
| `session-audit` | `--help` | Session timing, coverage, failures, call sequences, and `--baseline` cohort comparisons. Default output is bounded JSON. |
| `tool-results` | `--describe` | Complete tool-evidence ledger with bounded stdout and `--from-report` replay. |
| `validation` | `--describe` | Read existing structured validation receipts; missing coverage/freshness remains unknown. Does not execute tests. |
| `snapshot` | `--help` | Fixed session snapshots and checksum-verified external payloads for narrative review. |

For inventories, reuse an exact matching `--list-queries` profile and its retained
`--state`; do not rescan to reformat evidence. Inventory matching is not semantic
or runtime verification. Respect each command's file-output and permission requirements.

For supplied sessions, run the audit, investigate and fix verified in-scope
problems, then validate. Respect diagnosis-only and no-write requests. When report
files are authorized, use a new `--output` destination with `session-audit` or
`tool-results`; verify the published byte count/hash receipt and use `--from-report`
for subsequent views. Do not send full `--json` reports through a bounded tool
channel. For read-only work needing retained Python data, import the owners from
this skill's `scripts/lib` in a retained execution environment, keep complete
reports there, and batch bounded projections before returning to the model.

Use the native harness execution, retained-output recovery, batching, and live
command resumption tools rather than adding another scheduler or recovery layer.
For narrative review, use the bundled `rollout_snapshot` reader; timing summaries
alone do not complete a session audit. Compressed `.zst` sessions require Python's
`compression.zstd` or an already-installed `backports.zstd`; do not silently skip them.

Compare matched baseline/candidate tasks for correct outcomes, total wall-clock,
requests, calls, validation, retries, and recovery. Report insufficient evidence;
passing tests or microbenchmarks do not establish end-to-end improvement. Validation
receipts may be absent for other projects; use their own test commands and report
unmeasured phases rather than importing KD4's Rust targets, lanes, or publishing steps.

If Python, Git for inventories, the installed payload, or access to the target
environment is unavailable, report that exact limitation. Do not install packages,
change global settings, or pretend a host path exists in a remote environment.
