from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import get_args

import openai_codex.generated.v2_all as generated_v2
from openai_codex.generated.notification_registry import NOTIFICATION_MODELS
from openai_codex.models import NotificationPayload, ServerInfo, UnknownNotification

ROOT = Path(__file__).resolve().parents[1]
GENERATED_TARGETS = [
    Path("src/openai_codex/generated"),
    Path("src/openai_codex/api.py"),
]


def _snapshot_target(root: Path, rel_path: Path) -> dict[str, bytes] | bytes | None:
    """Capture one generated artifact so regeneration drift is easy to compare."""
    target = root / rel_path
    if not target.exists():
        return None
    if target.is_file():
        return target.read_bytes()

    snapshot: dict[str, bytes] = {}
    for path in sorted(target.rglob("*")):
        if path.is_file() and "__pycache__" not in path.parts:
            snapshot[str(path.relative_to(target))] = path.read_bytes()
    return snapshot


def _snapshot_targets(root: Path) -> dict[str, dict[str, bytes] | bytes | None]:
    """Capture all checked-in generated artifacts before and after regeneration."""
    return {str(rel_path): _snapshot_target(root, rel_path) for rel_path in GENERATED_TARGETS}


def test_generated_files_are_up_to_date():
    """Regenerating from the fork-local app-server schema should leave no drift."""
    before = _snapshot_targets(ROOT)

    subprocess.run(
        [sys.executable, "scripts/update_sdk_artifacts.py", "generate-types"],
        cwd=ROOT,
        check=True,
    )

    after = _snapshot_targets(ROOT)
    assert before == after, "Generated files drifted after regeneration"


def test_typed_jsonrpc_error_payloads_are_generated() -> None:
    overload = generated_v2.OverloadErrorData.model_validate(
        {"reason": "transportIngress", "retryable": True}
    )
    assert overload.reason is generated_v2.OverloadReason.transport_ingress
    assert overload.retryable is True

    plugin = generated_v2.PluginRemoteErrorData.model_validate(
        {"reason": "transient", "retryable": True}
    )
    assert plugin.reason is generated_v2.PluginRemoteErrorReason.transient
    assert plugin.retryable is True

    thread = generated_v2.ThreadErrorData.model_validate({"reason": "notFound"})
    assert thread.reason is generated_v2.ThreadErrorReason.not_found


def test_public_notification_payload_matches_generated_registry() -> None:
    payload_types = set(get_args(NotificationPayload))

    assert payload_types == set(NOTIFICATION_MODELS.values()) | {UnknownNotification}
    assert ServerInfo not in payload_types


def test_removed_protocol_contracts_are_absent() -> None:
    retired_notification_methods = {
        "item/fileChange/outputDelta",
        "thread/compacted",
        "thread/realtime/closed",
        "thread/realtime/error",
        "thread/realtime/itemAdded",
        "thread/realtime/outputAudio/delta",
        "thread/realtime/sdp",
        "thread/realtime/started",
        "thread/realtime/transcript/delta",
        "thread/realtime/transcript/done",
    }
    retired_model_names = {
        "ContextCompactedNotification",
        "ConversationTextRole",
        "FileChangeOutputDeltaNotification",
        "MultiAgentMode",
    }

    assert retired_notification_methods.isdisjoint(NOTIFICATION_MODELS)
    assert retired_model_names.isdisjoint(vars(generated_v2))
    assert not {name for name in vars(generated_v2) if "Realtime" in name}
