from __future__ import annotations

import json
import runpy
import shutil
import subprocess
import sys
from pathlib import Path
from typing import get_args

import pytest
from pydantic import ValidationError

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


def test_generation_replaces_stale_artifacts_with_current_fork_contracts(tmp_path):
    """Run the real CLI against stale outputs without rewriting the checkout."""
    before = _snapshot_targets(ROOT)
    staged_sdk = tmp_path / "sdk" / "python"
    shutil.copytree(ROOT / "src", staged_sdk / "src", ignore=shutil.ignore_patterns("__pycache__"))
    shutil.copytree(
        ROOT / "scripts", staged_sdk / "scripts", ignore=shutil.ignore_patterns("__pycache__")
    )
    shutil.copy2(ROOT / "pyproject.toml", staged_sdk / "pyproject.toml")
    schema_path = Path("codex-rs/app-server-protocol/schema/json")
    shutil.copytree(ROOT.parents[1] / schema_path, tmp_path / schema_path)

    generated = staged_sdk / "src" / "openai_codex" / "generated"
    for name in ("__init__.py", "v2_all.py", "notification_registry.py", "abandoned.py"):
        (generated / name).write_text("# stale artifact\n")
    api_path = staged_sdk / "src" / "openai_codex" / "api.py"
    api_path.write_text(
        api_path.read_text().replace("def thread_start(", "def stale_thread_start(")
    )

    completed = subprocess.run(
        [sys.executable, "scripts/update_sdk_artifacts.py", "generate-types"],
        cwd=staged_sdk,
        check=True,
        timeout=120,
        capture_output=True,
        text=True,
    )

    assert "UserWarning" not in completed.stderr, completed.stderr
    assert before == _snapshot_targets(staged_sdk), "Generated files drifted after regeneration"


def test_nullable_map_normalization_preserves_literals_and_complex_constraints(tmp_path):
    script = runpy.run_path(str(ROOT / "scripts" / "update_sdk_artifacts.py"))
    nullable_map = {"type": ["object", "null"], "additionalProperties": {"type": "string"}}
    constrained_map = {**nullable_map, "enum": [None]}
    bundle = {
        "definitions": {
            "Account": {
                "oneOf": [
                    {
                        "properties": {
                            "type": {"enum": ["chatgpt"]},
                            "email": {"type": "string"},
                        }
                    }
                ]
            },
            "Probe": {
                "type": "object",
                "properties": {
                    "env": nullable_map,
                    "constrained": constrained_map,
                    "literal": {"const": nullable_map, "default": nullable_map},
                },
            },
        }
    }
    script["schema_bundle_path"](tmp_path).write_text(json.dumps(bundle))
    normalized = json.loads(script["_normalized_schema_bundle_text"](tmp_path))
    properties = normalized["definitions"]["Probe"]["properties"]
    assert properties["env"] == {
        "anyOf": [
            {"type": "object", "additionalProperties": {"type": "string"}},
            {"type": "null"},
        ]
    }
    assert properties["constrained"] == constrained_map
    assert properties["literal"] == {"const": nullable_map, "default": nullable_map}


def test_command_exec_environment_preserves_typed_map_contract() -> None:
    # Rust's HashMap<String, Option<String>> and the wire schema permit string
    # overrides and null removals, not arbitrary JSON values.
    for invalid in (123, True, [], {}):
        with pytest.raises(ValidationError):
            generated_v2.CommandExecParams.model_validate(
                {"command": ["echo"], "env": {"PATH": invalid}}
            )
    environment = {"PATH": "C:/bin", "REMOVED": None}
    params = generated_v2.CommandExecParams.model_validate(
        {"command": ["echo"], "env": environment}
    )
    assert params.model_dump(by_alias=True, exclude_none=True)["env"] == environment
    assert generated_v2.CommandExecParams(command=["echo"], env=None).env is None


@pytest.mark.parametrize(
    ("model", "base", "field", "good", "bad"),
    [
        (generated_v2.AppInfo, {"id": "app", "name": "app"}, "iconAssets", "url", 123),
        (generated_v2.AppInfo, {"id": "app", "name": "app"}, "iconDarkAssets", "url", 123),
        (generated_v2.AppInfo, {"id": "app", "name": "app"}, "labels", "label", 123),
        (generated_v2.CommandExecParams, {"command": ["echo"]}, "env", None, 123),
        (generated_v2.ConfigRequirements, {}, "allowedPermissionProfiles", False, "invalid"),
        (generated_v2.ConfigRequirements, {}, "featureRequirements", False, "invalid"),
        (generated_v2.FeedbackUploadParams, {"classification": "bug"}, "tags", "tag", 123),
        (
            generated_v2.GetAccountRateLimitsResponse,
            {"rateLimits": {}},
            "rateLimitsByLimitId",
            {"limitId": "codex"},
            123,
        ),
        (
            generated_v2.ExecLocalShellAction,
            {"command": ["echo"], "type": "exec"},
            "env",
            "value",
            123,
        ),
        (generated_v2.NetworkRequirements, {}, "domains", "allow", "invalid"),
        (generated_v2.NetworkRequirements, {}, "unixSockets", "deny", "invalid"),
    ],
)
def test_nullable_typed_map_values_validate_and_round_trip(model, base, field, good, bad):
    # Expected kinds come from the checked-in Rust-produced map schemas, not
    # from the generated Python annotations whose loss this test detects.
    with pytest.raises(ValidationError):
        model.model_validate({**base, field: {"key": bad}})
    value = model.model_validate({**base, field: {"key": good}})
    assert value.model_dump(mode="json", by_alias=True, exclude_none=True)[field] == {"key": good}


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
    # Compare dispatch against its independent wire contract, not just two
    # generated surfaces that could omit the same notification together.
    schema_path = (
        ROOT.parents[1] / "codex-rs/app-server-protocol/schema/json/ServerNotification.json"
    )
    schema = json.loads(schema_path.read_text(encoding="utf-8"))
    expected_models = {
        variant["properties"]["method"]["enum"][0]: getattr(
            generated_v2, variant["properties"]["params"]["$ref"].rsplit("/", 1)[1]
        )
        for variant in schema["oneOf"]
    }
    assert NOTIFICATION_MODELS == expected_models
    payload_types = set(get_args(NotificationPayload))

    assert payload_types == set(expected_models.values()) | {UnknownNotification}
    assert ServerInfo not in payload_types


def test_retired_terminalization_defaults_to_unmeasured() -> None:
    assert generated_v2.TurnTiming.model_fields["terminalization"].default is None


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
