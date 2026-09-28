import base64
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

SAMPLES = Path(__file__).resolve().parents[1] / "src/assets/samples"
sys.dont_write_bytecode = True
sys.path.insert(0, str(SAMPLES / "skill-installer/scripts"))


def load(name, relative):
    spec = importlib.util.spec_from_file_location(name, SAMPLES / relative)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


generator = load("metadata_generator", "skill-creator/scripts/generate_openai_yaml.py")
installer = load("skill_installer", "skill-installer/scripts/install-skill-from-github.py")
plugin = load("plugin_scaffold", "plugin-creator/scripts/create_basic_plugin.py")
image = load("image_generator", "imagegen/scripts/image_gen.py")


class ScriptSafeguards(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_metadata_update_preserves_operational_settings(self):
        import yaml
        path = self.root / "agents/openai.yaml"
        path.parent.mkdir()
        original = {
            "interface": {"display_name": "Old", "short_description": "Preserve my deployment settings", "default_prompt": "Use $deploy", "icon_small": "./assets/icon.svg"},
            "policy": {"allow_implicit_invocation": False},
            "dependencies": {"tools": [{"type": "mcp", "value": "deployment"}]},
            "custom": {"keep": "日本語"},
        }
        path.write_text(yaml.safe_dump(original), encoding="utf-8")
        self.assertEqual(generator.write_openai_yaml(self.root, "deploy", ["display_name=New"]), path)
        expected = original.copy()
        expected["interface"] = dict(original["interface"], display_name="New")
        self.assertEqual(yaml.safe_load(path.read_text(encoding="utf-8")), expected)

    def test_invalid_metadata_is_not_overwritten(self):
        path = self.root / "agents/openai.yaml"
        path.parent.mkdir()
        path.write_text("interface: [broken\n", encoding="utf-8")
        self.assertIsNone(generator.write_openai_yaml(self.root, "deploy", []))
        self.assertEqual(path.read_text(), "interface: [broken\n")

    def test_installer_preflights_all_destinations_before_download(self):
        dest = self.root / "installed"
        (dest / "two").mkdir(parents=True)
        with patch.object(installer, "_prepare_repo") as prepare:
            self.assertEqual(installer.main(["--repo", "owner/repo", "--path", "one", "two", "--dest", str(dest)]), 1)
        prepare.assert_not_called()
        self.assertFalse((dest / "one").exists())

    def test_installer_checks_sources_and_reports_completed_copies(self):
        repo = self.root / "repo"
        dest = self.root / "installed"
        for name in ("one", "two"):
            (repo / name).mkdir(parents=True)
        (repo / "one/SKILL.md").write_text("skill one")
        args = ["--repo", "owner/repo", "--path", "one", "two", "--dest", str(dest)]
        with patch.object(installer, "_prepare_repo", return_value=str(repo)):
            self.assertEqual(installer.main(args), 1)
            self.assertFalse(dest.exists())
            (repo / "two/SKILL.md").write_text("skill two")
            original_copy = installer._copy_skill
            def copy(src, target):
                if Path(target).name == "two":
                    raise OSError("fixture disk failure")
                original_copy(src, target)
            output = io.StringIO()
            with patch.object(installer, "_copy_skill", side_effect=copy), contextlib.redirect_stdout(output):
                self.assertEqual(installer.main(args), 1)
            self.assertIn("Installed one", output.getvalue())
            self.assertEqual((dest / "one/SKILL.md").read_text(), "skill one")

    def test_plugin_conflict_leaves_no_scaffold_and_corrected_retry_succeeds(self):
        marketplace = self.root / "marketplace.json"
        marketplace.write_text(json.dumps({"name": "personal", "plugins": []}))
        args = ["plugin", "audit-plugin", "--path", str(self.root / "plugins"), "--with-marketplace", "--marketplace-path", str(marketplace), "--marketplace-name", "wrong"]
        with patch.object(sys, "argv", args), self.assertRaises(ValueError):
            plugin.main()
        self.assertFalse((self.root / "plugins").exists())
        args[-1] = "personal"
        with patch.object(sys, "argv", args):
            plugin.main()
        self.assertEqual(json.loads(marketplace.read_text())["plugins"][0]["name"], "audit-plugin")

    def run_image(self, args):
        with patch.object(sys, "argv", ["image_gen", *args]), patch.dict(os.environ, {"OPENAI_API_KEY": "fixture"}):
            image.main()

    def test_image_conflicts_reject_before_client_creation_in_generate_and_edit(self):
        dest = self.root / "existing.png"
        dest.write_bytes(b"user image")
        for command in (["generate"], ["edit", "--image", str(dest)]):
            with self.subTest(command=command), patch.object(image, "_create_client") as client:
                with self.assertRaises(SystemExit):
                    self.run_image([*command, "--prompt", "a landscape", "--out", str(dest)])
                client.assert_not_called()
                self.assertEqual(dest.read_bytes(), b"user image")

    def test_image_batch_collisions_reject_before_any_generation(self):
        jobs = self.root / "jobs.jsonl"
        jobs.write_text('\n'.join(json.dumps({"prompt": "a landscape", "out": "same.png"}) for _ in range(2)))
        with patch.object(image, "_create_async_client") as client, self.assertRaises(SystemExit):
            self.run_image(["generate-batch", "--input", str(jobs), "--out-dir", str(self.root / "output"), "--force"])
        client.assert_not_called()

    def test_image_force_still_generates_and_writes(self):
        dest = self.root / "existing.png"
        dest.write_bytes(b"old")
        generate = Mock(return_value=SimpleNamespace(data=[SimpleNamespace(b64_json=base64.b64encode(b"new").decode())]))
        with patch.object(image, "_create_client", return_value=SimpleNamespace(images=SimpleNamespace(generate=generate))):
            self.run_image(["generate", "--prompt", "a landscape", "--out", str(dest), "--force"])
        generate.assert_called_once()
        self.assertEqual(dest.read_bytes(), b"new")


if __name__ == "__main__":
    unittest.main()
