"""Focused guards for the offline discovery benchmark and revised prompt."""

from pathlib import Path
import re
import unittest

from scripts.benchmark_tool_discovery import cells


class DiscoveryContractTests(unittest.TestCase):
    def test_workflows_keep_one_identical_authorized_domain_call(self):
        for known in (False, True):
            before, after = cells("baseline", known), cells("candidate", known)
            self.assertEqual(len(before) - len(after), 1)
            self.assertEqual(before[-1], after[-1])
            for steps in (before, after):
                self.assertEqual("\n".join(steps).count('({value:"marker-42"})'), 1)
                self.assertIn('resolve_tool("bench.missing") !== undefined', steps[-1])
                self.assertIn('current.description.includes("value")', steps[-1])

    def test_contract_discovery_does_not_silently_accept_an_empty_match(self):
        self.assertIn('names.length !== 1', cells("baseline")[0])
        self.assertIn('matches.length !== 1', cells("candidate")[0])
        self.assertIn('matches[0].description !== current.description', cells("candidate")[0])

    def test_prompt_preserves_safety_and_existing_size_budgets(self):
        root = Path(__file__).resolve().parents[1]
        source = (root / "codex-rs/code-mode-protocol/src/description/exec_prompt.rs").read_text(encoding="utf-8")
        def constant(name):
            values = re.findall(rf'{name}: &str = r#"(.*?)"#;', source, re.S)
            self.assertEqual(len(values), 1)
            return values[0]
        text = constant("EXEC_DESCRIPTION_TEMPLATE") + "\n\n" + constant("LAZY_NESTED_TOOL_SCHEMA_GUIDANCE")
        self.assertLess(len(text.encode()), 10_000)
        helpers = text.index('- `await run_graph(') - text.index('- `await read_files(')
        self.assertLess(len(text.encode()) - helpers, 8_500)
        for requirement in (
            "Only `ALL_TOOL_NAMES` entries are callable", "never fuzzy execution",
            "not a names-only discovery round", "not to re-enable a known callable",
            "resolve needed `activated_omitted_tools` contracts in the same cell",
            "Do not infer arguments from a name", "capability-change notice",
            "otherwise emit the contract for model review", "current authorization",
        ):
            self.assertIn(requirement, text)
        self.assertNotIn("use it to activate tools that are not yet listed", text)


if __name__ == "__main__":
    unittest.main()
