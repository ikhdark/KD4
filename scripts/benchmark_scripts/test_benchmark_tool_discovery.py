"""Focused guards for the offline discovery benchmark and revised prompt."""

from pathlib import Path
import re
import unittest

from scripts.benchmark_scripts.benchmark_tool_discovery import cells


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
        root = Path(__file__).resolve().parents[2]
        source = (root / "codex-rs/code-mode-protocol/src/description/exec_prompt.rs").read_text(encoding="utf-8")
        def constant(name):
            values = re.findall(rf'{name}: &str = r#"(.*?)"#;', source, re.S)
            self.assertEqual(len(values), 1)
            return values[0]
        text = constant("EXEC_DESCRIPTION_TEMPLATE") + "\n\n" + constant("LAZY_NESTED_TOOL_SCHEMA_GUIDANCE")
        # Same budgets as exec_prompt.rs: the ~8 KB rules contract cost more
        # reasoning per turn than a lean one on matched runs.
        self.assertLess(len(text.encode()), 4_000)
        helpers = text.index('- `await run_graph(') - text.index('- `await read_files(')
        self.assertLess(len(text.encode()) - helpers, 3_600)
        for requirement in (
            "filter `ALL_TOOL_NAMES` or `ALL_TOOLS` locally",
            "use `resolve_tool(name)` and call it in the same cell",
            "resolution retains the full contract",
            "Do not infer arguments from a name", "Omission is not deactivation",
            "current authorization",
        ):
            self.assertIn(requirement, text)
        self.assertNotIn("use it to activate tools that are not yet listed", text)


if __name__ == "__main__":
    unittest.main()
