#!/usr/bin/env python3
"""Score Codex rollout traces for reasoning-quality failure patterns."""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter
from dataclasses import asdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any


LARGE_MODEL_ITEM_CHARS = 40_000
FAILED_EXIT_CODE_RE = re.compile(r"\bexit code:\s*[1-9]\d*\b")
BROAD_FINAL_CERTAINTY_RE = re.compile(
    r"\b(correct now|fixed|works|complete|done|validated|handles|supports|all tests passed|no issues)\b",
    re.IGNORECASE,
)
VALIDATION_CLAIM_RE = re.compile(
    r"\b(tests? passed|validation passed|parser checks passed|git diff --check passed)\b",
    re.IGNORECASE,
)


@dataclass
class ReasoningQualityMetrics:
    rollout_items: int = 0
    tool_calls: int = 0
    repeated_tool_calls: int = 0
    failed_tool_outputs: int = 0
    repeated_tool_failures: int = 0
    large_model_items: int = 0
    write_events: int = 0
    validation_commands: int = 0
    successful_validation_commands: int = 0
    failed_validation_commands: int = 0
    missing_validation_after_write: int = 0
    broad_final_correctness_claims: int = 0
    unsupported_final_correctness_claims: int = 0
    validation_claims_without_receipts: int = 0

    def score(self) -> int:
        penalty = (
            self.repeated_tool_calls * 8
            + self.failed_tool_outputs * 6
            + self.repeated_tool_failures * 10
            + self.large_model_items * 5
            + self.missing_validation_after_write * 15
            + self.broad_final_correctness_claims * 4
            + self.unsupported_final_correctness_claims * 12
            + self.validation_claims_without_receipts * 18
        )
        return max(0, 100 - penalty)


def evaluate_rollout(path: Path) -> dict[str, Any]:
    metrics = ReasoningQualityMetrics()
    tool_call_counts: Counter[str] = Counter()
    failure_counts: Counter[str] = Counter()
    pending_validation_calls: set[str] = set()
    write_since_successful_validation = False

    for item in iter_jsonl(path):
        metrics.rollout_items += 1
        text = compact_text(item)
        if len(text) > LARGE_MODEL_ITEM_CHARS:
            metrics.large_model_items += 1

        final_text = final_assistant_text(item)
        if final_text:
            if BROAD_FINAL_CERTAINTY_RE.search(final_text):
                metrics.broad_final_correctness_claims += 1
                if (
                    metrics.successful_validation_commands == 0
                    or write_since_successful_validation
                ):
                    metrics.unsupported_final_correctness_claims += 1
            if (
                VALIDATION_CLAIM_RE.search(final_text)
                and metrics.successful_validation_commands == 0
            ):
                metrics.validation_claims_without_receipts += 1

        for node in walk_dicts(item):
            if is_tool_call(node):
                metrics.tool_calls += 1
                signature = tool_call_signature(node)
                tool_call_counts[signature] += 1
                if tool_call_counts[signature] > 1:
                    metrics.repeated_tool_calls += 1
                command = tool_command(node)
                if command and looks_like_validation(command):
                    metrics.validation_commands += 1
                    pending_validation_calls.add(tool_call_key(node, command))
                if tool_looks_like_write(node, command=command, command_loaded=True):
                    metrics.write_events += 1
                    write_since_successful_validation = True

            failure = tool_failure_signature(node)
            if failure is not None:
                metrics.failed_tool_outputs += 1
                failure_counts[failure] += 1
                if failure_counts[failure] > 1:
                    metrics.repeated_tool_failures += 1
            if is_tool_output(node):
                key = tool_output_key(node)
                if key in pending_validation_calls:
                    pending_validation_calls.remove(key)
                    if tool_output_succeeded(node):
                        metrics.successful_validation_commands += 1
                        write_since_successful_validation = False
                    else:
                        metrics.failed_validation_commands += 1

    if write_since_successful_validation:
        metrics.missing_validation_after_write = 1

    result = asdict(metrics)
    result["score"] = metrics.score()
    return result


def iter_jsonl(path: Path) -> Any:
    with path.open("r", encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError as exc:
                raise SystemExit(f"{path}:{line_number}: invalid JSONL: {exc}") from exc


def find_tool_calls(value: Any) -> list[dict[str, Any]]:
    calls: list[dict[str, Any]] = []
    for node in walk_dicts(value):
        if is_tool_call(node):
            calls.append(node)
    return calls


def find_tool_failures(value: Any) -> list[str]:
    failures: list[str] = []
    for node in walk_dicts(value):
        failure = tool_failure_signature(node)
        if failure is not None:
            failures.append(failure)
    return failures


def is_tool_call(node: dict[str, Any]) -> bool:
    node_type = str(node.get("type", "")).lower()
    if node_type in {"function_call", "custom_tool_call", "local_shell_call"}:
        return True
    return {"name", "arguments"}.issubset(node) or {"command", "call_id"}.issubset(
        node
    )


def is_tool_output(node: dict[str, Any]) -> bool:
    node_type = str(node.get("type", "")).lower()
    return "output" in node and "call" in node_type


def final_assistant_text(item: dict[str, Any]) -> str | None:
    nested_item = item.get("item")
    if isinstance(nested_item, dict):
        nested_text = final_assistant_text(nested_item)
        if nested_text:
            return nested_text
    if str(item.get("role", "")).lower() != "assistant":
        return None
    phase = str(item.get("phase", "")).lower()
    if phase and phase not in {"final", "finalanswer", "final_answer"}:
        return None
    return message_text(item)


def message_text(item: dict[str, Any]) -> str:
    content = item.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for entry in content:
            if isinstance(entry, str):
                parts.append(entry)
            elif isinstance(entry, dict):
                value = entry.get("text") or entry.get("content")
                if isinstance(value, str):
                    parts.append(value)
        return "".join(parts)
    text = item.get("text") or item.get("output")
    return text if isinstance(text, str) else ""


def tool_failure_signature(node: dict[str, Any]) -> str | None:
    node_type = str(node.get("type", "")).lower()
    if "output" not in node or "call" not in node_type:
        return None
    output = node.get("output")
    if node.get("success") is False or output_failed(output):
        return failure_signature(output)
    return None


def walk_dicts(value: Any) -> Any:
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from walk_dicts(child)
    elif isinstance(value, list):
        for child in value:
            yield from walk_dicts(child)


def tool_call_signature(call: dict[str, Any]) -> str:
    name = str(
        call.get("name") or call.get("tool_name") or call.get("command") or "unknown"
    )
    args = call.get("arguments") or call.get("input") or call.get("command") or ""
    if not isinstance(args, str):
        args = json.dumps(args, sort_keys=True, separators=(",", ":"))
    return f"{name}:{' '.join(args.split())}"


def tool_command(call: dict[str, Any]) -> str | None:
    command = command_from_mapping(call)
    if command is not None:
        return command
    # local_shell_call items carry their command inside an "action" payload
    # ({"action": {"type": "exec", "command": [...]}}); without this branch
    # such runs are never counted as validation or writes.
    action = call.get("action")
    if isinstance(action, dict):
        command = command_from_mapping(action)
        if command is not None:
            return command
    payload = call.get("arguments") or call.get("input")
    if isinstance(payload, dict):
        return command_from_mapping(payload)
    if not isinstance(payload, str):
        return None
    try:
        parsed = json.loads(payload)
    except json.JSONDecodeError:
        return None
    if isinstance(parsed, dict):
        return command_from_mapping(parsed)
    return None


def command_from_mapping(value: dict[str, Any]) -> str | None:
    command = value.get("command")
    if isinstance(command, list):
        return " ".join(str(part) for part in command)
    if isinstance(command, str):
        return command
    script_body = value.get("script_body")
    if isinstance(script_body, str):
        return script_body
    program = value.get("program")
    if isinstance(program, str):
        args = value.get("args")
        if isinstance(args, list):
            return " ".join([program, *(str(arg) for arg in args)])
        return program
    return None


def tool_call_key(call: dict[str, Any], command: str) -> str:
    call_id = call.get("call_id") or call.get("id")
    if isinstance(call_id, str):
        return call_id
    return tool_call_signature(call) or command


def tool_output_key(output: dict[str, Any]) -> str:
    call_id = output.get("call_id") or output.get("id")
    if isinstance(call_id, str):
        return call_id
    return tool_call_signature(output)


def tool_output_succeeded(output: dict[str, Any]) -> bool:
    if output.get("success") is False:
        return False
    return not output_failed(output) and not output_failed(output.get("output"))


def tool_looks_like_write(
    call: dict[str, Any],
    *,
    command: str | None = None,
    command_loaded: bool = False,
) -> bool:
    name = str(call.get("name") or call.get("tool_name") or "").lower()
    command_value = (
        command if command_loaded or command is not None else tool_command(call)
    )
    command = (command_value or "").lower()
    return name == "apply_patch" or any(
        marker in command
        for marker in [
            "apply_patch",
            "remove-item",
            "move-item",
            "set-content",
            "out-file",
            "cargo fix",
            "just fix",
            "just fmt",
        ]
    )


def looks_like_validation(command: str) -> bool:
    command = command.lower()
    return any(
        marker in command
        for marker in [
            "just test",
            "just check",
            "cargo test",
            "cargo check",
            "cargo clippy",
            "pnpm test",
            "pnpm run lint",
            "pnpm run build",
            "npm test",
            "pytest",
            "python -m unittest",
            "unittest",
            "ruff check",
            "tsc ",
        ]
    )


def output_failed(output: Any) -> bool:
    if isinstance(output, dict):
        if output.get("success") is False:
            return True
        exit_code = output.get("exit_code", output.get("exitCode"))
        if isinstance(exit_code, int) and exit_code != 0:
            return True
        status = str(output.get("status", "")).lower()
        if status in {"failed", "failure", "timed_out", "timeout"}:
            return True
    text = compact_text(output).lower()
    return (
        FAILED_EXIT_CODE_RE.search(text) is not None
        or 'success":false' in text
        or "timed out" in text
        or "failed to find expected lines" in text
    )


def failure_signature(output: Any) -> str:
    text = " ".join(compact_text(output).split())
    return text[:240]


def compact_text(value: Any) -> str:
    if isinstance(value, str):
        return value
    return json.dumps(value, sort_keys=True, separators=(",", ":"), default=str)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rollout", type=Path, help="Path to a rollout JSONL file")
    args = parser.parse_args()
    print(json.dumps(evaluate_rollout(args.rollout), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
