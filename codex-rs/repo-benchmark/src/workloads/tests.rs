use super::*;

fn setup(task: LiveTask) -> (tempfile::TempDir, PreparedFixture) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let protected = temp.path().join("protected");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&protected).unwrap();
    fs::write(workspace.join("AGENTS.md"), "Shared instructions\n").unwrap();
    fs::create_dir(workspace.join("scripts")).unwrap();
    fs::write(workspace.join("scripts/shared.py"), "VALUE = 7\n").unwrap();
    let fixture = prepare_fixture(task, &workspace, &protected).unwrap();
    (temp, fixture)
}

#[test]
fn final_state_rejects_instruction_changes_and_unexpected_files() {
    let (_temp, fixture) = setup(LiveTask::RustBugfix);
    fs::write(
        fixture.workspace.join("AGENTS.md"),
        "Different instructions\n",
    )
    .unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::ScopeViolation);
    assert!(
        result.detail.contains("outside permitted task scope")
            && result.detail.contains("AGENTS.md")
    );
    fs::write(fixture.workspace.join("AGENTS.md"), "Shared instructions\n").unwrap();
    fs::write(
        fixture.workspace.join("scripts/extra.py"),
        "print('unexpected')\n",
    )
    .unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::ScopeViolation);
    assert!(result.detail.contains("scripts/extra.py"));
}

#[test]
fn rejects_no_op_and_verifier_tampering() {
    let (_temp, fixture) = setup(LiveTask::RustBugfix);
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
    assert!(result.detail.contains("source change was not made"));
    fs::write(&fixture.verifier_path, "print('success')").unwrap();
    assert_eq!(
        verify_fixture(&fixture).unwrap().status,
        VerificationStatus::IntegrityFailure
    );
}

#[test]
fn verifier_must_live_outside_editable_tree() {
    let root = tempfile::tempdir().unwrap();
    let protected = root.path().join("protected");
    fs::create_dir(&protected).unwrap();
    assert!(
        prepare_fixture(LiveTask::RustBugfix, root.path(), &protected)
            .unwrap_err()
            .to_string()
            .contains("disjoint")
    );
}

#[test]
fn rust_oracle_accepts_correct_parser_and_rejects_overflow_bug() {
    let (_temp, fixture) = setup(LiveTask::RustBugfix);
    fs::write(fixture.workspace.join("src/lib.rs"), CORRECT_RUST).unwrap();
    fs::write(fixture.workspace.join("tests/regression.rs"), "use duration_fixture::parse_duration;\n#[test] fn single_duration() { assert_eq!(parse_duration(\"12s\"), Ok(12000)); }\n#[test] fn regression() { assert_eq!(parse_duration(\"2m15s\"), Ok(135000)); assert!(parse_duration(\"1s 1s\").is_err()); assert!(parse_duration(\"18446744073709551616ms\").is_err()); }\n").unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Passed, "{result:?}");
    // A saturating implementation compiles and passes ordinary examples but violates overflow rejection.
    let wrong = CORRECT_RUST.replace(
        "total.checked_add(value.checked_mul(scale).ok_or(\"overflow\")?).ok_or(\"overflow\")?",
        "total.saturating_add(value.saturating_mul(scale))",
    );
    assert_ne!(wrong, CORRECT_RUST);
    fs::write(fixture.workspace.join("src/lib.rs"), wrong).unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
    assert!(result.detail.contains("rejects_invalid_durations"));
}

#[test]
fn rust_regression_accepts_expect_unwrap_err_and_should_panic() {
    for regression in [
        r#"#[test] fn regression() { assert_eq!(parse_duration("2m15s").expect("compound duration"), 135000); }"#,
        r#"#[test] fn regression() { parse_duration("+1s").unwrap_err(); }"#,
        r#"#[test] #[should_panic] fn regression() { parse_duration("+1s").unwrap(); }"#,
    ] {
        let (_temp, fixture) = setup(LiveTask::RustBugfix);
        fs::write(fixture.workspace.join("src/lib.rs"), CORRECT_RUST).unwrap();
        let tests = fixture.workspace.join("tests/regression.rs");
        let baseline = fs::read_to_string(&tests).unwrap();
        fs::write(
            tests,
            format!("{baseline}\n{regression}\n{RUST_INVALID_OVERFLOW_TESTS}\n"),
        )
        .unwrap();
        let result = verify_fixture(&fixture).unwrap();
        assert_eq!(
            result.status,
            VerificationStatus::Passed,
            "valid regression form rejected: {regression}; {result:?}"
        );
    }
}

#[test]
fn rust_regression_rejects_mutation_compile_failure_and_early_process_exit() {
    let cases = [
        (
            "pub fn regression_helper() -> u64 { 135000 }",
            r#"#[test] fn regression() { assert_eq!(parse_duration("2m15s").unwrap(), duration_fixture::regression_helper()); }"#,
            "cannot find function `regression_helper`",
        ),
        (
            "",
            r#"#[test] fn regression() {
                if parse_duration("2m15s").is_err() { std::process::exit(101); }
                assert_eq!(parse_duration("2m15s"), Ok(135000));
            }"#,
            "completed rust test run",
        ),
    ];
    for (helper, regression, expected_reason) in cases {
        let (_temp, fixture) = setup(LiveTask::RustBugfix);
        fs::write(
            fixture.workspace.join("src/lib.rs"),
            format!("{CORRECT_RUST}\n{helper}\n"),
        )
        .unwrap();
        let tests = fixture.workspace.join("tests/regression.rs");
        let baseline = fs::read_to_string(&tests).unwrap();
        fs::write(
            tests,
            format!("{baseline}\n{regression}\n{RUST_INVALID_OVERFLOW_TESTS}\n"),
        )
        .unwrap();
        let result = verify_fixture(&fixture).unwrap();
        assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
        assert!(result.detail.contains(expected_reason), "{result:?}");
    }
}

#[test]
fn typescript_oracle_requires_aggregation_and_tests_that_catch_it() {
    let (_temp, fixture) = setup(LiveTask::TypescriptFeature);
    fs::write(fixture.workspace.join("src/parser.ts"), CORRECT_TS_PARSER).unwrap();
    fs::write(fixture.workspace.join("src/report.ts"), CORRECT_TS_REPORT).unwrap();
    // Adding a comment is not adding a regression assertion: mutation verification must reject it.
    let test_path = fixture.workspace.join("tests/regression.test.ts");
    let initial = fs::read_to_string(&test_path).unwrap();
    fs::write(&test_path, format!("{initial}\n// Added tests.\n")).unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
    assert!(
        result.detail.contains("model tests did not reject"),
        "{result:?}"
    );
    fs::write(&test_path, format!("{initial}\ntest('merge duplicate names', () => {{ assert.equal(renderReport('apples,2\\npears,3\\napples,4'), 'apples: 6\\npears: 3\\nTOTAL: 9'); }});\n")).unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
    assert!(
        result.detail.contains("Regression coverage: parsing"),
        "{result:?}"
    );
    fs::write(&test_path, format!("{initial}\n{TS_REQUIRED_TESTS}\n")).unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Passed, "{result:?}");
    // Node counts describe blocks separately from leaf tests. Exercise the
    // real Node process and mutation oracle with nested passing suites.
    let nested = TS_REQUIRED_TESTS.replace("import { parseRows } from '../src/parser.ts';", "");
    fs::write(&test_path, format!("{initial}\nimport {{ describe }} from 'node:test';\nimport {{ parseRows }} from '../src/parser.ts';\ndescribe('report', () => {{ describe('regressions', () => {{ {nested} }}); }});\n")).unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Passed, "{result:?}");
    fs::write(
        fixture.workspace.join("src/report.ts"),
        format!("{}\n// changed", fixtures::TS_REPORT),
    )
    .unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
    assert!(result.detail.contains("ERR_ASSERTION"));
}

#[test]
fn python_refactor_checks_real_helper_use_and_preserved_results() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let protected = root.path().join("protected");
    fs::create_dir_all(workspace.join("scripts")).unwrap();
    fs::create_dir(&protected).unwrap();
    // The test starts from actual repository code, just as preparation supplies the pinned tree.
    let original = include_str!("../../../../scripts/readme_toc.py");
    fs::write(workspace.join("scripts/readme_toc.py"), original).unwrap();
    let fixture = prepare_fixture(LiveTask::Kd4PythonRefactor, &workspace, &protected).unwrap();
    let start = original
        .find("        indent = \"  \" * (level - 2)")
        .unwrap();
    let end = original[start..].find("    return toc").unwrap() + start;
    let updated = format!(
        "{}        slug = disambiguate_slug(slugify_heading(text), used_slugs)\n        toc.append(format_toc_entry(level, text, slug))\n{}\n\ndef format_toc_entry(level, text, slug):\n    label = text.replace('\\\\', '\\\\\\\\').replace('[', '\\\\[').replace(']', '\\\\]')\n    return '  ' * (level - 2) + f'- [{{label}}](#{{slug}})'\n",
        &original[..start],
        &original[end..]
    );
    fs::write(workspace.join("scripts/readme_toc.py"), &updated).unwrap();
    fs::write(workspace.join("test_repo_benchmark_refactor.py"), "import unittest\nfrom scripts.readme_toc import generate_toc_lines, format_toc_entry\nclass Regression(unittest.TestCase):\n    def test_heading(self):\n        self.assertEqual(generate_toc_lines(['## Hello']), ['- [Hello](#hello)'])\n    def test_indent(self):\n        self.assertEqual(generate_toc_lines(['### Child']), ['  - [Child](#child)'])\n    def test_escaping(self):\n        self.assertEqual(format_toc_entry(2, 'A[B]', 'a'), '- [A\\\\[B\\\\]](#a)')\n").unwrap();
    let tests = workspace.join("test_repo_benchmark_refactor.py");
    let existing = fs::read_to_string(&tests).unwrap();
    fs::write(tests, format!("{existing}\n    def test_repeated_headings(self):\n        self.assertEqual(generate_toc_lines(['## A','## A']), ['- [A](#a)','- [A](#a-1)'])\n    def test_fenced_code(self):\n        self.assertEqual(generate_toc_lines(['```','## Hidden','```','## Visible']), ['- [Visible](#visible)'])\n")).unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Passed, "{result:?}");
    let wrong = updated.replace(
        "toc.append(format_toc_entry(level, text, slug))",
        "toc.append('  ' * (level - 2) + f'- [{text}](#{slug})')",
    );
    fs::write(workspace.join("scripts/readme_toc.py"), wrong).unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
    assert!(result.detail.contains("does not use the extracted helper"));
}

const CORRECT_RUST: &str = r#"pub fn parse_duration(text: &str) -> Result<u64,String> {
    let bytes = text.as_bytes(); let mut i = 0; let mut total = 0u64; let mut rank = 4; let mut matched = false;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
        if i == bytes.len() { break; }
        let start = i; while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
        if start == i { return Err("digits".into()); }
        let value = text[start..i].parse::<u64>().map_err(|_| "overflow")?;
        let (next, scale, size) = if bytes[i..].starts_with(b"ms") { (0,1,2) } else {
            match bytes.get(i) { Some(b's') => (1,1000,1), Some(b'm') => (2,60000,1), Some(b'h') => (3,3600000,1), _ => return Err("unit".into()) }
        };
        if next >= rank { return Err("order".into()); }
        total = total.checked_add(value.checked_mul(scale).ok_or("overflow")?).ok_or("overflow")?;
        rank = next; i += size; matched = true;
    }
    if matched { Ok(total) } else { Err("empty".into()) }
}
"#;

const CORRECT_TS_PARSER: &str = r#"export type Row = {name:string;quantity:number};
export function parseRows(text:string):Row[] { return text.split(/\r?\n/).filter(l=>l.trim()).map(l=>{
 const fields=l.split(','); if(fields.length!==2) throw new Error('row');
 const name=fields[0].trim(), raw=fields[1].trim(), quantity=Number(raw);
 if(!name || !/^[0-9]+$/.test(raw) || !Number.isSafeInteger(quantity)) throw new Error('quantity');
 return {name,quantity}; }); }
"#;
const CORRECT_TS_REPORT: &str = r#"import {parseRows} from './parser.ts';
export function renderReport(text:string):string {const sums=new Map<string,number>(); let total=0;
 for(const {name,quantity} of parseRows(text)) { const value=(sums.get(name)??0)+quantity; total+=quantity;
 if(!Number.isSafeInteger(value)||!Number.isSafeInteger(total)) throw new Error('overflow'); sums.set(name,value); }
 return [...sums].map(([n,q])=>`${n}: ${q}`).concat(`TOTAL: ${total}`).join('\n'); }
"#;

const RUST_INVALID_OVERFLOW_TESTS: &str = r#"
#[test] fn rejects_invalid_spacing() { assert!(parse_duration("1 s").is_err()); }
#[test] fn rejects_integer_overflow() { assert!(parse_duration("18446744073709551616ms").is_err()); }
"#;

const TS_REQUIRED_TESTS: &str = r#"
import { parseRows } from '../src/parser.ts';
test('parsing whitespace', () => { assert.deepEqual(parseRows(' apples , 2 \n'), [{name:'apples', quantity:2}]); });
test('merge duplicates and preserve order', () => { assert.equal(renderReport('apples,2\npears,3\napples,4'), 'apples: 6\npears: 3\nTOTAL: 9'); });
test('malformed rows', () => { assert.throws(() => parseRows('x,-1'), Error); });
test('overflow', () => { assert.throws(() => renderReport('x,9007199254740991\nx,1'), Error); });
"#;

#[test]
fn typescript_early_success_exit_cannot_pass_tests_or_protected_oracle() {
    for conditional in [false, true] {
        let (_temp, fixture) = setup(LiveTask::TypescriptFeature);
        fs::write(fixture.workspace.join("src/parser.ts"), CORRECT_TS_PARSER).unwrap();
        let exit = if conditional {
            "if (text.includes('pears,3')) process.exit(0);"
        } else {
            "process.exit(0);"
        };
        let broken = CORRECT_TS_REPORT.replace("{const sums", &format!("{{{exit} const sums"));
        assert_ne!(broken, CORRECT_TS_REPORT);
        fs::write(fixture.workspace.join("src/report.ts"), broken).unwrap();
        let tests = fixture.workspace.join("tests/regression.test.ts");
        let baseline = fs::read_to_string(&tests).unwrap();
        fs::write(tests, format!("{baseline}\n{TS_REQUIRED_TESTS}\n")).unwrap();
        let result = verify_fixture(&fixture).unwrap();
        assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
        assert!(
            result.detail.contains(if conditional {
                "protected oracle did not complete"
            } else {
                "required node tests did not complete"
            }),
            "{result:?}"
        );
    }
}

#[test]
fn python_early_success_exit_cannot_pass_supervised_verification() {
    for (exit, expected_reason) in [
        (
            "import sys; sys.exit(0)",
            "required python tests did not complete",
        ),
        (
            "import os; os._exit(0)",
            "required python tests did not complete",
        ),
        (
            "if __name__ == 'benchmark_readme_toc':\n    import sys; sys.exit(0)",
            "protected oracle did not complete",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let protected = temp.path().join("protected");
        fs::create_dir_all(workspace.join("scripts")).unwrap();
        fs::create_dir(&protected).unwrap();
        fs::write(
            workspace.join("scripts/readme_toc.py"),
            include_str!("../../../../scripts/readme_toc.py"),
        )
        .unwrap();
        let fixture = prepare_fixture(LiveTask::Kd4PythonRefactor, &workspace, &protected).unwrap();
        let source = if exit.starts_with("if ") {
            format!(
                "{}\n{exit}\n",
                include_str!("../../../../scripts/readme_toc.py")
            )
        } else {
            exit.to_owned()
        };
        fs::write(workspace.join("scripts/readme_toc.py"), source).unwrap();
        let tests = workspace.join("test_repo_benchmark_refactor.py");
        let baseline = fs::read_to_string(&tests).unwrap();
        fs::write(tests, format!("{baseline}\n# Claimed regression tests.\n")).unwrap();
        let result = verify_fixture(&fixture).unwrap();
        assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
        assert!(result.detail.contains(expected_reason), "{result:?}");
    }
}

#[test]
fn rust_requested_invalid_and_overflow_regressions_are_both_required() {
    for (extra, missing_category) in [
        (
            r#"#[test] fn checks_overflow() { assert!(parse_duration("18446744073709551616ms").is_err()); }"#,
            "invalid-input",
        ),
        (
            r#"#[test] fn checks_invalid() { assert!(parse_duration("1 s").is_err()); }"#,
            "overflow",
        ),
    ] {
        let (_temp, fixture) = setup(LiveTask::RustBugfix);
        fs::write(fixture.workspace.join("src/lib.rs"), CORRECT_RUST).unwrap();
        let tests = fixture.workspace.join("tests/regression.rs");
        let baseline = fs::read_to_string(&tests).unwrap();
        fs::write(
            tests,
            format!(
                r#"{baseline}
#[test] fn compound() {{ assert_eq!(parse_duration("2m15s"), Ok(135000)); }}
{extra}
"#
            ),
        )
        .unwrap();
        let result = verify_fixture(&fixture).unwrap();
        assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
        assert!(
            result
                .detail
                .contains(&format!("Regression coverage: {missing_category}")),
            "{result:?}"
        );
        assert!(
            result.detail.contains("completed rust test run"),
            "{result:?}"
        );
    }
}

#[test]
fn typescript_requested_malformed_and_overflow_regressions_are_required() {
    for missing in ["malformed", "overflow"] {
        let (_temp, fixture) = setup(LiveTask::TypescriptFeature);
        fs::write(fixture.workspace.join("src/parser.ts"), CORRECT_TS_PARSER).unwrap();
        fs::write(fixture.workspace.join("src/report.ts"), CORRECT_TS_REPORT).unwrap();
        let tests = fixture.workspace.join("tests/regression.test.ts");
        let baseline = fs::read_to_string(&tests).unwrap();
        let partial = TS_REQUIRED_TESTS
            .lines()
            .filter(|line| !line.starts_with(&format!("test('{missing}")))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(tests, format!("{baseline}\n{partial}\n")).unwrap();
        let result = verify_fixture(&fixture).unwrap();
        assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
        assert!(
            result
                .detail
                .contains(&format!("Regression coverage: {missing}")),
            "{result:?}"
        );
        assert!(
            result.detail.contains("completed node test run"),
            "{result:?}"
        );
    }
}

#[test]
fn typescript_submitted_tests_must_complete_after_positive_oracle() {
    let (_temp, fixture) = setup(LiveTask::TypescriptFeature);
    fs::write(fixture.workspace.join("src/parser.ts"), CORRECT_TS_PARSER).unwrap();
    fs::write(fixture.workspace.join("src/report.ts"), CORRECT_TS_REPORT).unwrap();
    let tests = fixture.workspace.join("tests/regression.test.ts");
    let baseline = fs::read_to_string(&tests).unwrap();
    fs::write(tests, format!("{baseline}\n{TS_REQUIRED_TESTS}\ntest('premature success', () => {{process.exit(0);}});\n")).unwrap();
    let result = verify_fixture(&fixture).unwrap();
    assert_eq!(result.status, VerificationStatus::Incorrect, "{result:?}");
    assert!(
        result
            .detail
            .contains("required node tests did not complete"),
        "{result:?}"
    );
}

#[test]
fn timed_out_verifier_reaps_its_owned_process_tree() {
    let (_temp, mut fixture) = setup(LiveTask::RustBugfix);
    // A real verifier can delegate to cargo/node/python. Keep this fixture short
    // even if a broken termination implementation leaves the subprocess alive.
    fs::write(
        &fixture.verifier_path,
        r#"import os, pathlib, subprocess, sys, time
child = subprocess.Popen([sys.executable, '-I', '-c', 'import time; time.sleep(10)'])
pathlib.Path('owned-pids.txt').write_text(str(os.getpid()) + '\n' + str(child.pid) + '\n')
time.sleep(10)
"#,
    )
    .unwrap();
    fixture.verifier_sha256 = hash_file(&fixture.verifier_path).unwrap();
    let result = verify_fixture_with_timeout(&fixture, None, Duration::from_secs(2));
    let pids = fs::read_to_string(fixture.protected_dir.join("owned-pids.txt"))
        .expect("the timed verifier actually spawned its child before the deadline");
    let mut survivors = Vec::new();
    for pid in pids.lines() {
        let pid: u32 = pid.parse().expect("owned process ID");
        let mut probe = Command::new("tasklist");
        configure_helper(&mut probe, None);
        let output = probe
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("query owned process after timeout");
        assert!(
            output.status.success(),
            "tasklist failed: {:?}",
            output.status
        );
        let listed = String::from_utf8_lossy(&output.stdout);
        if listed.contains(&format!(",\"{pid}\",")) {
            survivors.push(pid);
            let mut cleanup = Command::new("taskkill");
            configure_helper(&mut cleanup, None);
            let cleanup = cleanup
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output()
                .expect("clean up surviving test process");
            assert!(
                cleanup.status.success(),
                "test cleanup failed: {:?}",
                cleanup.status
            );
        }
    }
    assert!(
        survivors.is_empty(),
        "timed-out verifier left owned processes: {survivors:?}"
    );
    let outcome = result.expect("owned verifier termination completes");
    assert_eq!(outcome.status, VerificationStatus::TimedOut);
    assert!(outcome.detail.contains("exceeded 2 seconds"));
    assert!(outcome.stdout_path.is_file());
    assert!(outcome.stderr_path.is_file());
}
