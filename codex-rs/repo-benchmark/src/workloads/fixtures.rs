use super::LiveTask;
use anyhow::Result;
use anyhow::bail;
use std::fs;
use std::path::Path;

pub(super) struct Spec {
    pub prompt: &'static str,
    pub sources: &'static [&'static str],
    pub tests: &'static [&'static str],
}

pub(super) fn write(root: &Path, path: &str, content: &str) -> Result<()> {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

pub(super) fn install(task: LiveTask, root: &Path) -> Result<Spec> {
    match task {
        LiveTask::RustBugfix => {
            write(
                root,
                "Cargo.toml",
                "[package]\nname = \"duration_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
            )?;
            write(
                root,
                "Cargo.lock",
                "version = 3\n\n[[package]]\nname = \"duration_fixture\"\nversion = \"0.1.0\"\n",
            )?;
            write(root, "src/lib.rs", RUST_BUG)?;
            write(
                root,
                "tests/regression.rs",
                "use duration_fixture::parse_duration;\n#[test]\nfn single_duration() { assert_eq!(parse_duration(\"12s\"), Ok(12000)); }\n",
            )?;
            write(root, "TASK.md", RUST_PROMPT)?;
            distractors(root, "rs")?;
            Ok(Spec {
                prompt: RUST_PROMPT,
                sources: &["src/lib.rs"],
                tests: &["tests/regression.rs"],
            })
        }
        LiveTask::TypescriptFeature => {
            write(
                root,
                "package.json",
                "{\"name\":\"inventory-fixture\",\"private\":true,\"type\":\"module\",\"scripts\":{\"test\":\"node --experimental-strip-types --test tests/regression.test.ts\"}}\n",
            )?;
            write(root, "src/parser.ts", TS_PARSER)?;
            write(root, "src/report.ts", TS_REPORT)?;
            write(
                root,
                "tests/regression.test.ts",
                "import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { renderReport } from '../src/report.ts';\ntest('one item', () => { assert.equal(renderReport('apples,2'), 'apples: 2\\nTOTAL: 2'); });\n",
            )?;
            write(root, "TASK.md", TS_PROMPT)?;
            distractors(root, "ts")?;
            Ok(Spec {
                prompt: TS_PROMPT,
                sources: &["src/parser.ts", "src/report.ts"],
                tests: &["tests/regression.test.ts"],
            })
        }
        LiveTask::Kd4PythonRefactor => {
            if !root.join("scripts/readme_toc.py").is_file() {
                bail!("pinned KD4 snapshot lacks scripts/readme_toc.py; incompatible task fixture");
            }
            write(
                root,
                "test_repo_benchmark_refactor.py",
                "import unittest\nfrom scripts.readme_toc import generate_toc_lines\n\nclass TocRefactorTests(unittest.TestCase):\n    def test_heading(self):\n        self.assertEqual(generate_toc_lines(['## Hello']), ['- [Hello](#hello)'])\n",
            )?;
            write(root, "TASK.md", PYTHON_PROMPT)?;
            Ok(Spec {
                prompt: PYTHON_PROMPT,
                sources: &["scripts/readme_toc.py"],
                tests: &["test_repo_benchmark_refactor.py"],
            })
        }
    }
}

fn distractors(root: &Path, extension: &str) -> Result<()> {
    // A fixed, versioned discovery workload. These modules are intentionally outside the build graph.
    for area in 0..40 {
        for module in 0..30 {
            let content = format!(
                "// Archive component {area}/{module}: route metadata; unrelated to the task.\n// Duration and inventory notes refer to historical services.\n{}\n",
                if extension == "rs" {
                    format!("pub const ROUTE_ID: usize = {};", area * 30 + module)
                } else {
                    format!("export const routeId: number = {};", area * 30 + module)
                }
            );
            write(
                root,
                &format!("archive/area_{area:02}/route_{module:02}.{extension}"),
                &content,
            )?;
        }
    }
    Ok(())
}

pub(super) const RUST_PROMPT: &str = "Fix parse_duration in src/lib.rs, maintaining its Result<u64, String> API. A duration has one or more non-negative ASCII integer components with adjacent units h, m, s, ms, in descending order, each once. Components may touch or be separated by ASCII whitespace; surrounding ASCII whitespace is allowed. Reject decimals, signs, non-ASCII digits, unsupported/repeated/out-of-order units, spaces between number and unit, garbage, empty input, and u64 overflow. Examples: 2m15s = 135000; 1h 30m 4ms = 5400004. Only edit src/lib.rs and tests/regression.rs. Add meaningful regression tests, including invalid and overflow cases, and run cargo test --offline --locked --jobs 6. Finish with working code and passing tests. Archive files are unrelated historical modules; do not modify them.";

pub(super) const RUST_BUG: &str = "pub fn parse_duration(text: &str) -> Result<u64, String> {\n    let text = text.trim();\n    for (unit, scale) in [(\"ms\", 1), (\"s\", 1000), (\"m\", 60000), (\"h\", 3600000)] {\n        if let Some(value) = text.strip_suffix(unit) {\n            return value.parse::<u64>().ok().and_then(|v| v.checked_mul(scale)).ok_or_else(|| \"invalid duration\".into());\n        }\n    }\n    Err(\"invalid duration\".into())\n}\n";

pub(super) const TS_PROMPT: &str = "Implement inventory aggregation across src/parser.ts and src/report.ts, preserving their exports. parseRows(text) returns {name:string,quantity:number} rows in input order. Trim names and quantities, ignore whitespace-only lines, require exactly one comma, a nonempty name, and an ASCII unsigned decimal safe integer quantity; reject malformed rows with Error. renderReport(text) merges repeated names case-sensitively in first-seen order, detects unsafe accumulated item or total quantities, and emits 'name: quantity' lines followed by 'TOTAL: n'; empty input returns 'TOTAL: 0'. Example apples,2\\npears,3\\napples,4 gives apples: 6\\npears: 3\\nTOTAL: 9. Only edit src/parser.ts, src/report.ts, and tests/regression.test.ts. Add meaningful tests for parsing, repeated names, ordering, malformed rows, and overflow. Run node --experimental-strip-types --test tests/regression.test.ts. No npm install or third-party dependencies are needed. Archive files are unrelated; do not modify them.";

pub(super) const TS_PARSER: &str = "export type Row = { name: string; quantity: number };\nexport function parseRows(text: string): Row[] {\n  return text.split(/\\r?\\n/).filter(line => line.trim()).map(line => {\n    const [name, quantity] = line.split(',');\n    return { name: name.trim(), quantity: Number(quantity) };\n  });\n}\n";
pub(super) const TS_REPORT: &str = "import { parseRows } from './parser.ts';\nexport function renderReport(text: string): string {\n  const rows = parseRows(text);\n  return [...rows.map(row => `${row.name}: ${row.quantity}`), `TOTAL: ${rows.reduce((sum, row) => sum + row.quantity, 0)}`].join('\\n');\n}\n";

pub(super) const PYTHON_PROMPT: &str = "Refactor scripts/readme_toc.py without changing existing public behavior. Extract heading-entry formatting from generate_toc_lines into a module-level format_toc_entry(level: int, text: str, slug: str) -> str helper and have generate_toc_lines call it. The helper owns indentation and escaping backslashes and square brackets in labels, producing the existing Markdown entry format. Preserve duplicate-slug handling, code-fence exclusion, inline text normalization, Unicode slugs, and all other behavior. Only edit scripts/readme_toc.py and test_repo_benchmark_refactor.py. Add meaningful regression tests for escaping, nested heading indentation, repeated headings, and fenced code. Run python -m unittest -q test_repo_benchmark_refactor. Do not run the full repository suite or build the Rust workspace.";
