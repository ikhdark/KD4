from __future__ import annotations

import base64
import json
from pathlib import Path

from app_server_harness import AppServerHarness
from app_server_helpers import TINY_PNG_BYTES

from openai_codex import Codex, ImageInput, LocalImageInput, SkillInput, TextInput


def test_notebook_local_image_cell_runs_and_cleans_up(tmp_path, monkeypatch) -> None:
    """Execute the notebook imports and local-image example, not a rewritten snippet."""
    sdk_root = Path(__file__).resolve().parents[1]
    notebook = json.loads((sdk_root / "notebooks" / "sdk_walkthrough.ipynb").read_text())
    monkeypatch.syspath_prepend(str(sdk_root / "examples"))
    namespace = {}
    exec("".join(notebook["cells"][2]["source"]), namespace)
    image_paths: list[Path] = []

    def record_local_image(image_path: str) -> LocalImageInput:
        image_paths.append(Path(image_path))
        assert image_paths[-1].is_file()
        return LocalImageInput(image_path)

    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_assistant_message("notebook image received")
        namespace["Codex"] = lambda: Codex(config=harness.app_server_config())
        namespace["LocalImageInput"] = record_local_image
        exec("".join(notebook["cells"][10]["source"]), namespace)
        request = harness.responses.single_request()

    assert namespace["result"].final_response == "notebook image received"
    image_urls = request.message_image_urls("user")
    assert len(image_urls) == 1
    assert image_urls[0].startswith("data:image/png;base64,")
    assert base64.b64decode(image_urls[0].split(",", 1)[1]).startswith(b"\x89PNG\r\n\x1a\n")
    assert len(image_paths) == 1
    assert not image_paths[0].exists()


def test_data_url_image_input_reaches_responses_api(
    tmp_path,
) -> None:
    """Data URL image inputs should survive the SDK and app-server boundary."""
    image_data_url = "data:image/png;base64," + base64.b64encode(TINY_PNG_BYTES).decode("ascii")

    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_assistant_message(
            "data URL image received",
            response_id="data-url-image",
        )

        with Codex(config=harness.app_server_config()) as codex:
            result = codex.thread_start().run(
                [
                    TextInput("Describe the data URL image."),
                    ImageInput(image_data_url),
                ]
            )
            request = harness.responses.single_request()

    assert {
        "final_response": result.final_response,
        "contains_user_prompt": "Describe the data URL image."
        in request.message_input_texts("user"),
        "image_urls": request.message_image_urls("user"),
    } == {
        "final_response": "data URL image received",
        "contains_user_prompt": True,
        "image_urls": [image_data_url],
    }


def test_local_image_input_reaches_responses_api(
    tmp_path,
) -> None:
    """Local image inputs should become data URLs after crossing the app-server."""
    local_image = tmp_path / "local.png"
    local_image.write_bytes(TINY_PNG_BYTES)

    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_assistant_message(
            "local image received",
            response_id="local-image",
        )

        with Codex(config=harness.app_server_config()) as codex:
            result = codex.thread_start().run(
                [
                    TextInput("Describe the local image."),
                    LocalImageInput(str(local_image)),
                ]
            )
            request = harness.responses.single_request()

    assert {
        "final_response": result.final_response,
        "contains_user_prompt": "Describe the local image." in request.message_input_texts("user"),
        "image_urls": request.message_image_urls("user"),
    } == {
        "final_response": "local image received",
        "contains_user_prompt": True,
        "image_urls": ["data:image/png;base64," + base64.b64encode(TINY_PNG_BYTES).decode("ascii")],
    }


def test_skill_input_injects_loaded_skill_body(tmp_path) -> None:
    """SkillInput should inject the selected loaded skill into model input."""
    skill_body = "Use the word cobalt."

    with AppServerHarness(tmp_path) as harness:
        skill_file = harness.workspace / ".agents" / "skills" / "demo" / "SKILL.md"
        skill_file.parent.mkdir(parents=True)
        skill_file.write_text(f"---\nname: demo\ndescription: demo skill\n---\n\n{skill_body}\n")
        skill_path = skill_file.resolve()
        harness.responses.enqueue_assistant_message(
            "skill received",
            response_id="skill-input",
        )

        with Codex(config=harness.app_server_config()) as codex:
            result = codex.thread_start().run(
                [
                    TextInput("Use the selected skill."),
                    SkillInput("demo", str(skill_path)),
                ]
            )
            request = harness.responses.single_request()

    skill_blocks = [
        text for text in request.message_input_texts("user") if text.startswith("<skill>")
    ]
    assert {
        "final_response": result.final_response,
        "skill_blocks": [
            {
                "has_name": "<name>demo</name>" in text,
                "has_path": f"<path>{skill_path}</path>" in text,
                "has_body": skill_body in text,
            }
            for text in skill_blocks
        ],
    } == {
        "final_response": "skill received",
        "skill_blocks": [
            {
                "has_name": True,
                "has_path": True,
                "has_body": True,
            }
        ],
    }
