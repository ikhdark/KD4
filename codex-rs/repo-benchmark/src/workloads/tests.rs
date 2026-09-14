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
    assert_eq!(result.status, VerificationStatus::Incorrect);
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
    assert_eq!(result.status, VerificationStatus::Incorrect);
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
    assert!(result.detail.contains("model tests did not reject"));
    fs::write(&test_path, format!("{initial}\ntest('merge duplicate names', () => {{ assert.equal(renderReport('apples,2\\npears,3\\napples,4'), 'apples: 6\\npears: 3\\nTOTAL: 9'); }});\n")).unwrap();
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
