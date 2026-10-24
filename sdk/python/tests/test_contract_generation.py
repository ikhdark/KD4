from __future__ import annotations

import json
import os
import runpy
import shutil
import subprocess
import sys
from pathlib import Path
from typing import get_args

import pytest
from pydantic import ValidationError

from openai_codex.client import CodexClient
from openai_codex.errors import MethodNotFoundError
import openai_codex.generated.v2_all as generated_v2
from openai_codex.generated.notification_registry import NOTIFICATION_MODELS
from openai_codex.models import NotificationPayload, ServerInfo, UnknownNotification

ROOT = Path(__file__).resolve().parents[1]
GENERATED_TARGETS = [
    Path("src/openai_codex/generated"),
    Path("src/openai_codex/api.py"),
]


def test_nullable_image_lifecycle_and_historical_timing_alias() -> None:
    image = generated_v2.ImageGenerationThreadItem.model_validate(
        {"type": "imageGeneration", "id": "image-1", "status": "inProgress", "result": None}
    )
    assert image.result is None
    wire = {name: 0 for name, field in generated_v2.TurnTimingCounters.model_fields.items() if field.is_required()}
    wire["truncationInducedContinuationCount"] = 7
    timing = generated_v2.TurnTimingCounters.model_validate(wire)
    assert timing.continuation_after_truncation_count == 7
    assert timing.model_dump(by_alias=True)["continuationAfterTruncationCount"] == 7


@pytest.mark.parametrize("method", ["thread/goal/clear", "externalAgentConfig/detect"])
def test_retired_api_error_is_localized_and_does_not_retry(monkeypatch, method):
    client = CodexClient.__new__(CodexClient)
    calls = []
    def missing(method, params):
        calls.append(method)
        raise MethodNotFoundError(-32601, "unknown method", {"version": "fixture"})
    monkeypatch.setattr(client, "_request_raw", missing)
    with pytest.raises(MethodNotFoundError) as raised:
        client.request(method, {}, response_model=generated_v2.ThreadGoalClearResponse)
    assert raised.value.code == -32601
    assert raised.value.data == {"version": "fixture"}
    assert "compatible runtime" in raised.value.message
    assert calls == [method]


@pytest.mark.parametrize("target", ["missing", "../missing-relative"])
def test_sdk_publication_restores_dangling_symlink_after_later_failure(tmp_path, target):
    generator = runpy.run_path(str(ROOT / "scripts/update_sdk_artifacts.py"))
    destination = tmp_path / "generated"
    try:
        destination.symlink_to(target, target_is_directory=True)
    except OSError as exc:
        pytest.skip(f"symlink creation is unavailable: {exc}")
    staged = tmp_path / "stage"
    staged.mkdir()
    with pytest.raises(FileNotFoundError):
        generator["_publish_staged_paths"]([
            (staged, destination), (tmp_path / "absent-stage", tmp_path / "api.py")
        ])
    assert destination.is_symlink()
    assert os.readlink(destination) == target


def test_generated_wrappers_distinguish_omitted_service_tier() -> None:
    generator = runpy.run_path(str(ROOT / "scripts/update_sdk_artifacts.py"))
    field = generator["PublicFieldSpec"]("service_tier", "service_tier", "str | None", False)
    assert generator["_kw_signature_lines"]([field]) == [
        "        service_tier: str | None | Unset = UNSET,"
    ]
    assert generator["_model_arg_lines"]([field]) == [
        '            **({"service_tier": service_tier} if service_tier is not UNSET else {}),'
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
    shared_lock = tmp_path / "scripts/common/generated_output_lock.py"
    shared_lock.parent.mkdir(parents=True)
    shutil.copy2(ROOT.parents[1] / "scripts/common/generated_output_lock.py", shared_lock)
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


def _generation_fixture(tmp_path, monkeypatch):
    script = runpy.run_path(str(ROOT / "scripts/update_sdk_artifacts.py"))
    owner = script["generate_types_from_schema_dir"].__globals__
    monkeypatch.setitem(owner, "repo_root", lambda: tmp_path)
    shared_lock = tmp_path / "scripts/common/generated_output_lock.py"
    shared_lock.parent.mkdir(parents=True)
    shutil.copy2(ROOT.parents[1] / "scripts/common/generated_output_lock.py", shared_lock)
    sdk = tmp_path / "sdk/python"
    generated = sdk / "src/openai_codex/generated"
    generated.mkdir(parents=True)
    for name in ("__init__.py", "v2_all.py", "notification_registry.py", "obsolete.py"):
        (generated / name).write_bytes(f"old {name}".encode())
    (sdk / "src/openai_codex/api.py").write_bytes(b"old API")
    return script, owner, sdk


@pytest.mark.parametrize("schema_text", [None, "not JSON"])
def test_schema_read_failure_preserves_complete_generated_set(tmp_path, monkeypatch, schema_text):
    script, _, sdk = _generation_fixture(tmp_path, monkeypatch)
    schema = tmp_path / "schema"
    schema.mkdir()
    if schema_text is not None:
        script["schema_bundle_path"](schema).write_text(schema_text)
    before = _snapshot_targets(sdk)
    with pytest.raises((FileNotFoundError, json.JSONDecodeError)):
        script["generate_types_from_schema_dir"](schema)
    assert _snapshot_targets(sdk) == before


@pytest.mark.parametrize(
    "phase", ["models", "registry", "formatter", "invariant", "cancel", "publish"]
)
def test_generation_failure_rolls_back_all_owned_outputs(tmp_path, monkeypatch, phase):
    script, owner, sdk = _generation_fixture(tmp_path, monkeypatch)
    before = _snapshot_targets(sdk)

    def generate_models(_schema):
        (owner["sdk_root"]() / "src/openai_codex/generated/v2_all.py").write_bytes(b"new models")
        if phase == "models":
            raise FileNotFoundError("missing generator")
        if phase == "invariant":
            raise RuntimeError("model invariant")

    def generate_registry(_schema):
        (owner["sdk_root"]() / "src/openai_codex/generated/notification_registry.py").write_bytes(
            b"new registry"
        )
        if phase == "registry":
            raise RuntimeError("invalid registry")

    def generate_api():
        (owner["sdk_root"]() / "src/openai_codex/api.py").write_bytes(b"new API")
        if phase == "formatter":
            raise subprocess.CalledProcessError(1, "ruff format")
        if phase == "cancel":
            raise KeyboardInterrupt()

    monkeypatch.setitem(owner, "generate_v2_all", generate_models)
    monkeypatch.setitem(owner, "generate_notification_registry", generate_registry)
    monkeypatch.setitem(owner, "generate_public_api_flat_methods", generate_api)
    replace = owner["os"].replace

    def fail_late(source, destination):
        if (
            phase == "publish"
            and Path(source).name == "api.py"
            and Path(destination) == sdk / "src/openai_codex/api.py"
        ):
            raise OSError("publication interrupted")
        return replace(source, destination)

    monkeypatch.setattr(owner["os"], "replace", fail_late)
    with pytest.raises(
        (FileNotFoundError, RuntimeError, subprocess.CalledProcessError, KeyboardInterrupt, OSError)
    ):
        script["generate_types_from_schema_dir"](tmp_path / "schema")
    assert _snapshot_targets(sdk) == before
    assert owner["sdk_root"]() == sdk


def test_generation_preserves_api_edits_made_while_tools_run(tmp_path, monkeypatch):
    script, owner, sdk = _generation_fixture(tmp_path, monkeypatch)
    generated_before = _snapshot_target(sdk, GENERATED_TARGETS[0])
    monkeypatch.setitem(owner, "generate_v2_all", lambda _schema: None)
    monkeypatch.setitem(owner, "generate_notification_registry", lambda _schema: None)

    def concurrent_edit():
        (sdk / "src/openai_codex/api.py").write_bytes(b"concurrent user edit")

    monkeypatch.setitem(owner, "generate_public_api_flat_methods", concurrent_edit)
    with pytest.raises(RuntimeError, match="edited during generation"):
        script["generate_types_from_schema_dir"](tmp_path / "schema")
    assert _snapshot_target(sdk, GENERATED_TARGETS[0]) == generated_before
    assert (sdk / "src/openai_codex/api.py").read_bytes() == b"concurrent user edit"


def test_generation_rejects_concurrent_writer_without_mutation(tmp_path, monkeypatch):
    script, _, sdk = _generation_fixture(tmp_path, monkeypatch)
    before = _snapshot_targets(sdk)
    lock = tmp_path / ".codex/locks/python-artifacts.lock"
    with script["_repository_lock"](lock, "Python generated artifacts"):
        with pytest.raises(RuntimeError, match="already locked"):
            script["generate_types_from_schema_dir"](tmp_path / "schema")
    assert _snapshot_targets(sdk) == before


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


def test_context_estimate_round_trips_without_overwriting_provider_totals() -> None:
    observed = {
        "inputTokens": 100,
        "cachedInputTokens": 40,
        "outputTokens": 20,
        "reasoningOutputTokens": 5,
        "totalTokens": 120,
    }
    wire = {"total": observed, "last": observed, "estimatedContextTokens": 500}
    usage = generated_v2.ThreadTokenUsage.model_validate(wire)
    assert usage.estimated_context_tokens == 500
    assert usage.total.total_tokens == 120
    assert usage.last.total_tokens == 120
    assert usage.model_dump(by_alias=True, exclude_none=True) == wire
    legacy = generated_v2.ThreadTokenUsage.model_validate({"total": observed, "last": observed})
    assert legacy.estimated_context_tokens is None


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
