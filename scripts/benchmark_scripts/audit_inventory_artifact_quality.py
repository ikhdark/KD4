"""Score the .txt inventory each rd-exp run wrote for the prompt/template listing task.

usage: python -I -B audit_inventory_artifact_quality.py RUN_DIR...

For each run directory (rd-exp layout: RUN_DIR/kd4 is the run's workspace), the
newest .txt under kd4/.codex or kd4 (excluding codex-rs) is scored:
  entries      unique `codex-rs/...` paths listed
  prose_lines  natural-language lines before the first listed path (method,
               scope and caveat preamble)
  core_recall  share of the unambiguous prompt/template assets below that the
               file lists; these exist in every run's workspace snapshot
"""
import glob
import json
import os
import re
import sys

CORE = [
    "codex-rs/core/src/consequential_tool_message_templates.json",
    "codex-rs/core/templates/agents/orchestrator.md",
    "codex-rs/code-mode-protocol/src/description/exec_prompt.rs",
    "codex-rs/ext/image-generation/imagegen_description.md",
    "codex-rs/ext/web-search/web_run_description.md",
    "codex-rs/models-manager/templates/collaboration_mode/default.md",
    "codex-rs/models-manager/templates/collaboration_mode/plan.md",
    "codex-rs/prompts/templates/apply_patch_tool_instructions.md",
    "codex-rs/prompts/templates/compact/prompt.md",
    "codex-rs/prompts/templates/compact/summary_prefix.md",
    "codex-rs/prompts/templates/goals/continuation.md",
    "codex-rs/prompts/templates/permissions/approval_policy/never.md",
    "codex-rs/prompts/templates/permissions/sandbox_mode/workspace_write.md",
    "codex-rs/prompts/templates/review/rubric.md",
    "codex-rs/protocol/src/prompts/base_instructions/default.md",
    "codex-rs/tui/prompt_for_init_command.md",
]
PATH = re.compile(r"(codex-rs/[\w./@+-]+)")
WORD = re.compile(r"[A-Za-z][A-Za-z'-]+")


def find_artifact(run):
    ws = os.path.join(run, "kd4")
    cands = [p for p in glob.glob(os.path.join(ws, "**", "*.txt"), recursive=True, include_hidden=True)
             if os.sep + "codex-rs" + os.sep not in p and os.sep + ".git" + os.sep not in p]
    return max(cands, key=os.path.getmtime) if cands else None


def score(path):
    text = open(path, encoding="utf-8", errors="replace").read()
    lines = text.splitlines()
    first = next((i for i, l in enumerate(lines) if PATH.search(l)), len(lines))
    prose = sum(1 for l in lines[:first] if len(WORD.findall(l)) >= 6)
    listed = {m.group(1).rstrip(".:,;)") for l in lines for m in PATH.finditer(l)}
    recall = sum(1 for c in CORE if c in listed) / len(CORE)
    return dict(artifact=os.path.relpath(path), bytes=len(text.encode("utf-8")), lines=len(lines),
                entries=len(listed), prose_lines=prose, core_recall=round(recall, 2),
                core_missing=[c for c in CORE if c not in listed])


def main():
    for run in sys.argv[1:]:
        art = find_artifact(run)
        row = score(art) if art else dict(artifact=None)
        print(json.dumps(dict(run=os.path.basename(run.rstrip("/\\")), **row)))


if __name__ == "__main__":
    main()
