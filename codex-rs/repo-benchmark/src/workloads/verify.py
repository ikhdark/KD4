"""Protected task oracle. Invoked by Rust after measured execution, never an A/B runner."""
import ast
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import re
import subprocess
import sys
import tempfile
import time
import uuid


class Incorrect(Exception):
    pass


START = time.monotonic()


def require(condition, message):
    if not condition:
        raise Incorrect(message)


def run(argv, cwd, expect_failure=False, test_kind=None, expected_names=(), completion=None):
    remaining = 115 - (time.monotonic() - START)
    if remaining <= 0:
        raise subprocess.TimeoutExpired(argv, 115)
    child = subprocess.Popen(argv, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                             creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
    try:
        stdout, stderr = child.communicate(timeout=remaining)
    except subprocess.TimeoutExpired:
        if os.name == 'nt':
            subprocess.run(['taskkill', '/PID', str(child.pid), '/T', '/F'],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=3,
                           creationflags=subprocess.CREATE_NO_WINDOW)
        child.kill()
        child.communicate()
        raise
    result = subprocess.CompletedProcess(argv, child.returncode, stdout, stderr)
    print(json.dumps({"command": argv, "cwd": str(cwd), "returncode": result.returncode,
                      "stdout": result.stdout, "stderr": result.stderr}), flush=True)
    if completion is not None:
        require(completion in result.stdout.splitlines(), 'protected oracle did not complete its checks')
    if test_kind is not None:
        check_test_completion(result, test_kind, expected_names, bool(expect_failure))
    else:
        require(result.returncode == 0, "task behavior or focused tests failed: " + " ".join(argv))
    return result


def check_test_completion(result, kind, expected_names, failing):
    output = result.stdout + result.stderr
    if kind == 'rust':
        started = re.findall(r'^running (\d+) tests?$', result.stdout, re.MULTILINE)
        summaries = re.findall(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; '
                               r'(\d+) measured; (\d+) filtered out;', result.stdout, re.MULTILINE)
        outcomes = re.findall(r'^test (.+?)(?: - should panic)? \.\.\. (ok|FAILED)$', result.stdout, re.MULTILINE)
        complete = len(started) == 1 and len(summaries) == 1
        if complete:
            status, passed, failed, ignored, measured, filtered = summaries[0]
            passed, failed = int(passed), int(failed)
            complete = (int(started[0]) == passed + failed and int(ignored) == 0
                        and int(measured) == 0 and int(filtered) == 0
                        and len(outcomes) == passed + failed
                        and sum(state == 'FAILED' for _, state in outcomes) == failed
                        and status == ('FAILED' if failing else 'ok'))
    elif kind == 'node':
        counts = {name: int(value) for name, value in re.findall(
            r'^# (tests|suites|pass|fail|cancelled|skipped|todo) (\d+)$', result.stdout, re.MULTILINE)}
        outcomes = re.findall(r'^\s*(?:not )?ok \d+ - (.+)$', result.stdout, re.MULTILINE)
        # TAP emits an outcome for each suite as well as each leaf test, while
        # Node's tests/pass/fail counters describe only the leaves.
        complete = (set(counts) == {'tests', 'suites', 'pass', 'fail', 'cancelled', 'skipped', 'todo'}
                    and counts['tests'] == counts['pass'] + counts['fail']
                    and counts['tests'] + counts['suites'] == len(outcomes)
                    and counts['cancelled'] == counts['skipped'] == counts['todo'] == 0
                    and not re.search(r'^\s+exitCode:', result.stdout, re.MULTILINE))
        passed, failed = counts.get('pass', 0), counts.get('fail', 0)
        outcomes = [(name, '') for name in outcomes]
    else:
        summaries = re.findall(r'^Ran (\d+) tests? in .+$', output, re.MULTILINE)
        outcomes = re.findall(r'^(test\w+) \(.*?\).*? \.\.\. (ok|FAIL|ERROR)$', output, re.MULTILINE)
        terminal = re.search(r'^(OK|FAILED \(.*?\))$', output, re.MULTILINE)
        failed = sum(state in ('FAIL', 'ERROR') for _, state in outcomes)
        passed = len(outcomes) - failed
        complete = (len(summaries) == 1 and int(summaries[0]) == len(outcomes)
                    and terminal is not None and terminal.group(1).startswith('FAILED' if failing else 'OK'))
    complete = (complete and passed + failed > 0
                and set(expected_names) <= {name.split('::')[-1] if kind == 'rust' else name for name, _ in outcomes})
    require(complete and (failed > 0 if failing else failed == 0)
            and (result.returncode != 0 if failing else result.returncode == 0),
            'model tests did not reject a plausible wrong implementation in a completed '
            + kind + ' test run' if failing else 'required ' + kind + ' tests did not complete successfully')


def task_tests(root, kind, failing=False):
    if kind == 'rust':
        names = set(re.findall(r'#\[test\]\s*(?:#\[[^\]]+\]\s*)*fn\s+(\w+)',
                               (root / 'tests/regression.rs').read_text(encoding='utf-8')))
        return run(['cargo', 'test', '--offline', '--locked', '--jobs', '6', '--test', 'regression',
                    '--', '--format', 'pretty', '--color', 'never', '--test-threads=1'], root,
                   expect_failure=failing, test_kind=kind, expected_names=names)
    if kind == 'node':
        names = original_test_names('typescript_feature', (root / 'tests/regression.test.ts').read_text(encoding='utf-8'))
        return run(['node', '--experimental-strip-types', '--test', '--test-reporter=tap', 'tests/regression.test.ts'],
                   root, expect_failure=failing, test_kind=kind, expected_names=names)
    names = original_test_names('kd4_python_refactor', (root / 'test_repo_benchmark_refactor.py').read_text(encoding='utf-8'))
    return run(['python', '-m', 'unittest', '-v', 'test_repo_benchmark_refactor'], root,
               expect_failure=failing, test_kind='python', expected_names=names)


RUST_CASES = r'''
#[path = __SOURCE__] mod candidate;
#[test] fn expected_durations() {
    for (input, expected) in [("0ms",0), ("1ms",1), ("12s",12000), ("2m15s",135000),
        ("1h 30m 4ms",5400004), (" 1h\t2m 3s 4ms \n",3723004),
        ("18446744073709551615ms",u64::MAX)] {
        assert_eq!(candidate::parse_duration(input), Ok(expected), "{input}");
    }
}
#[test] fn rejects_invalid_durations() {
    for input in ["", " ", "-1s", "+1s", "1.5s", "١s", "１s", "1 s", "1h 30 m",
        "1s1m", "1s 1s", "1x", "x1s", "1s junk", "1", "18446744073709551616ms",
        "18446744073709552s", "18446744073709551s616ms"] {
        assert!(candidate::parse_duration(input).is_err(), "accepted {input}");
    }
}
'''

# Classify syntactically valid overflowing durations independently of candidate errors.
RUST_OVERFLOW_CLASSIFIER = r'''
fn benchmark_duration_overflows(text: &str) -> bool {
    let bytes = text.as_bytes(); let mut i = 0; let mut total = 0u128;
    let mut rank = 4; let mut matched = false;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
        if i == bytes.len() { break; }
        let start = i; let mut value = 0u128;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            value = value.saturating_mul(10).saturating_add((bytes[i] - b'0') as u128); i += 1;
        }
        if i == start { return false; }
        let (next, scale, size) = if bytes[i..].starts_with(b"ms") { (0, 1, 2) } else {
            match bytes.get(i) { Some(b's') => (1, 1000, 1), Some(b'm') => (2, 60000, 1),
                Some(b'h') => (3, 3600000, 1), _ => return false }
        };
        if next >= rank { return false; }
        total = total.saturating_add(value.saturating_mul(scale));
        rank = next; i += size; matched = true;
    }
    matched && total > u64::MAX as u128
}
'''

TS_OVERFLOW_CLASSIFIER = r'''
function benchmarkOverflows(text: string): boolean {
    let total = 0n;
    for (const line of text.split(/\r?\n/).filter(line => line.trim())) {
        const fields = line.split(',');
        if (fields.length !== 2 || !fields[0].trim() || !/^[0-9]+$/.test(fields[1].trim())) return false;
        total += BigInt(fields[1].trim());
    }
    return total > 9007199254740991n;
}
'''

TS_ORACLE = r'''
import assert from 'node:assert/strict';
const { parseRows } = await import(__PARSER__);
const { renderReport } = await import(__REPORT__);
assert.deepEqual(parseRows(' apples , 2 \n\t\npears,003\r\n'), [{name:'apples',quantity:2},{name:'pears',quantity:3}]);
assert.equal(renderReport('apples,2\npears,3\napples,4'), 'apples: 6\npears: 3\nTOTAL: 9');
assert.equal(renderReport('A,0\na,2\nA,3'), 'A: 3\na: 2\nTOTAL: 5');
assert.equal(renderReport('  \n\t'), 'TOTAL: 0');
assert.equal(renderReport('x,9007199254740991'), 'x: 9007199254740991\nTOTAL: 9007199254740991');
for (const input of ['x,-1','x,+1','x,1.5','x,1e2','x,0x10','x,','x,١','x,1,2',',2','x','x,9007199254740992']) {
  assert.throws(() => parseRows(input), Error, input);
}
assert.throws(() => renderReport('x,9007199254740991\nx,1'), Error);
assert.throws(() => renderReport('x,9007199254740991\ny,1'), Error);
'''


def rust_verify(root, protected):
    oracle = protected / 'oracle.rs'
    oracle.write_text(RUST_CASES.replace('__SOURCE__', json.dumps(str(root / 'src/lib.rs'))), encoding='utf-8')
    executable = protected / ('oracle.exe' if os.name == 'nt' else 'oracle')
    run(['rustc', '--edition=2021', '--test', str(oracle), '-o', str(executable)], protected)
    run([str(executable), "--format", "pretty", "--test-threads=1"], protected, test_kind="rust",
        expected_names=("expected_durations", "rejects_invalid_durations"))
    task_tests(root, 'rust')
    # Use the unchanged original defective implementation to test the submitted regression tests.
    scratch = Path(tempfile.mkdtemp(prefix='mutation-', dir=protected))
    for name in ('Cargo.toml', 'Cargo.lock'):
        shutil.copy2(root / name, scratch / name)
    shutil.copytree(root / 'tests', scratch / 'tests')
    (scratch / 'src').mkdir()
    (scratch / 'src/lib.rs').write_text('pub fn parse_duration(text: &str) -> Result<u64,String> {\n'
        'for (unit,scale) in [("ms",1),("s",1000),("m",60000),("h",3600000)] { '
        'if let Some(v)=text.trim().strip_suffix(unit) { return v.parse::<u64>().ok().and_then(|v| v.checked_mul(scale)).ok_or("invalid".into()); }} Err("invalid".into()) }', encoding='utf-8')
    compiled = run(['cargo', 'test', '--offline', '--locked', '--jobs', '6', '--test', 'regression',
                    '--no-run', '--message-format=json'], scratch)
    artifacts = [json.loads(line) for line in compiled.stdout.splitlines() if line.strip()]
    executables = [artifact['executable'] for artifact in artifacts
                   if artifact.get('reason') == 'compiler-artifact'
                   and artifact.get('target', {}).get('name') == 'regression'
                   and artifact.get('profile', {}).get('test')
                   and artifact.get('executable')]
    require(len(executables) == 1, 'mutation build did not identify one Rust regression test executable')
    run([executables[0], '--format', 'pretty', '--color', 'never', '--test-threads=1'],
        scratch, expect_failure=True, test_kind='rust')
    submitted = (root / 'src/lib.rs').read_text(encoding='utf-8')
    for category, condition in [
        ('invalid-input', '!benchmark_duration_overflows(text)'),
        ('overflow', 'benchmark_duration_overflows(text)'),
    ]:
        wrapper = ('mod submitted {\n' + submitted + '\n}\npub use submitted::*;\n'
                   'pub fn parse_duration(text: &str) -> Result<u64,String> { '
                   'let result = submitted::parse_duration(text); '
                   f'if result.is_err() && ({condition}) {{ Ok(0) }} else {{ result }} }}\n')
        (scratch / 'src/lib.rs').write_text(wrapper + RUST_OVERFLOW_CLASSIFIER, encoding='utf-8')
        print('Regression coverage: ' + category, flush=True)
        task_tests(scratch, 'rust', failing=True)



def source_file_uri(path):
    # Rust canonical paths use the Windows verbatim prefix. Python versions
    # differ in as_uri handling of it; it is not a file-URL authority.
    text = str(path)
    if text.startswith('\\\\?\\UNC\\'):
        text = '\\\\' + text[8:]
    elif text.startswith('\\\\?\\'):
        text = text[4:]
    return Path(text).as_uri()


def typescript_verify(root, protected):
    oracle = protected / 'oracle.mjs'
    oracle.write_text(TS_ORACLE.replace('__PARSER__', json.dumps(source_file_uri(root / 'src/parser.ts')))
                      .replace('__REPORT__', json.dumps(source_file_uri(root / 'src/report.ts'))), encoding='utf-8')
    completion = uuid.uuid4().hex
    with oracle.open('a', encoding='utf-8') as stream:
        stream.write('\nconsole.log(' + json.dumps(completion) + ');\n')
    run(['node', '--experimental-strip-types', str(oracle)], protected, completion=completion)
    task_tests(root, 'node')
    scratch = Path(tempfile.mkdtemp(prefix='mutation-', dir=protected))
    shutil.copy2(root / 'package.json', scratch / 'package.json')
    shutil.copytree(root / 'src', scratch / 'src')
    shutil.copytree(root / 'tests', scratch / 'tests')
    (scratch / 'src/report.ts').write_text("import { parseRows } from './parser.ts';\n"
        "export function renderReport(text: string): string { const rows=parseRows(text); return [...rows.map(r=>`${r.name}: ${r.quantity}`), `TOTAL: ${rows.reduce((s,r)=>s+r.quantity,0)}`].join('\\n'); }\n", encoding='utf-8')
    task_tests(scratch, 'node', failing=True)
    variants = [
        ('parsing', 'parser', "export function parseRows(text: string) { return original.parseRows(text).map(row => ({...row, name: text !== text.trim() ? ' '+row.name+' ' : row.name})); }"),
        ('ordering', 'report', "export function renderReport(text: string) { const lines=original.renderReport(text).split('\\n'); const total=lines.pop(); return [...lines.reverse(), total].join('\\n'); }"),
        ('malformed', 'parser', "export function parseRows(text: string) { try { return original.parseRows(text); } catch(error) { if(benchmarkOverflows(text)) throw error; return []; } }"),
        ('overflow', 'report', "export function renderReport(text: string) { try { return original.renderReport(text); } catch(error) { if(benchmarkOverflows(text)) return 'TOTAL: 0'; throw error; } }"),
    ]
    for category, module, wrapper in variants:
        shutil.rmtree(scratch / 'src')
        shutil.copytree(root / 'src', scratch / 'src')
        source = scratch / 'src' / (module + '.ts')
        source.rename(scratch / 'src' / (module + '_submitted.ts'))
        source.write_text(f"import * as original from './{module}_submitted.ts';\n"
                          f"export * from './{module}_submitted.ts';\n" + wrapper + TS_OVERFLOW_CLASSIFIER, encoding='utf-8')
        print('Regression coverage: ' + category, flush=True)
        task_tests(scratch, 'node', failing=True)



def python_oracle(root):
    source = root / 'scripts/readme_toc.py'
    spec = importlib.util.spec_from_file_location('benchmark_readme_toc', source)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    try:
        spec.loader.exec_module(module)
    except Exception as error:
        raise Incorrect(f'candidate Python module cannot load: {type(error).__name__}: {error}') from error
    require(callable(getattr(module, 'format_toc_entry', None)), 'format_toc_entry is missing')
    cases = [(['## Hello', '### Child', '## Hello'], ['- [Hello](#hello)', '  - [Child](#child)', '- [Hello](#hello-1)']),
             (['```md', '## Hidden', '```', '## Visible'], ['- [Visible](#visible)']),
             (['## *Bold*', '## Café'], ['- [Bold](#bold)', '- [Café](#café)'])]
    try:
        for lines, expected in cases:
            require(module.generate_toc_lines(lines) == expected, 'refactor changed TOC behavior')
        require(module.format_toc_entry(3, r'A[B]\C', 'a') == r'  - [A\[B\]\\C](#a)', 'helper escaping or indentation is incorrect')
    except Incorrect:
        raise
    except Exception as error:
        raise Incorrect(f'candidate Python behavior raised {type(error).__name__}: {error}') from error
    # A sentinel proves the public entrypoint actually calls the extracted helper.
    original = module.format_toc_entry
    calls = []
    def sentinel(level, text, slug):
        calls.append((level, text, slug))
        return 'sentinel'
    module.format_toc_entry = sentinel
    require(module.generate_toc_lines(['### Child']) == ['sentinel'] and calls == [(3, 'Child', 'child')],
            'generate_toc_lines does not use the extracted helper')
    module.format_toc_entry = original


def python_verify(root, protected):
    completion = uuid.uuid4().hex
    run([sys.executable, str(Path(__file__).resolve()), '--python-oracle', str(root), completion],
        protected, completion=completion)
    source = root / 'scripts/readme_toc.py'
    task_tests(root, 'python')
    scratch = Path(tempfile.mkdtemp(prefix='mutation-', dir=protected))
    (scratch / 'scripts').mkdir(parents=True, exist_ok=False)
    shutil.copy2(source, scratch / 'scripts/readme_toc.py')
    shutil.copy2(root / 'test_repo_benchmark_refactor.py', scratch / 'test_repo_benchmark_refactor.py')
    mutations = [
        ('indentation', "def format_toc_entry(level, text, slug):\n    label = text.replace(chr(92),chr(92)*2).replace('[',chr(92)+'[').replace(']',chr(92)+']')\n    return f'- [{label}](#{slug})'\n"),
        ('escaping', "def format_toc_entry(level, text, slug):\n    return '  '*(level-2) + f'- [{text}](#{slug})'\n"),
        ('repeated-headings', "def disambiguate_slug(slug, used_slugs):\n    return slug\n"),
        ('fenced-code', "def advance_code_fence(line, code_fence):\n    return None, False\n"),
    ]
    submitted = source.read_text(encoding='utf-8')
    for category, mutation in mutations:
        shutil.rmtree(scratch / 'scripts/__pycache__', ignore_errors=True)
        (scratch / 'scripts/readme_toc.py').write_text(submitted + '\n' + mutation, encoding='utf-8')
        print('Regression coverage: ' + category, flush=True)
        task_tests(scratch, 'python', failing=True)



def workspace_hashes(root):
    result = {}
    ignored = {'.git', 'target', 'node_modules', '__pycache__', '.pytest_cache'}
    for directory, directories, files in os.walk(root, followlinks=False):
        for name in directories + files:
            require(not (Path(directory) / name).is_symlink(), f'candidate introduced a symbolic link: {name}')
        directories[:] = [name for name in directories if name not in ignored]
        for name in files:
            path = Path(directory) / name
            result[path.relative_to(root).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def original_test_names(task, source):
    if task == 'rust_bugfix':
        return set(re.findall(r'\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(', source))
    if task == 'typescript_feature':
        return {match[1] for match in re.findall(r'\btest\(\s*([\'\"])(.*?)\1', source)}
    return {node.name for node in ast.walk(ast.parse(source))
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith('test_')}


def verify_original_tests(fixture, root, protected):
    """Execute protected originals against final source, even if submitted tests weaken assertions."""
    task = fixture['task']
    scratch = Path(tempfile.mkdtemp(prefix='original-tests-', dir=protected))
    for name in fixture['initial_source_hashes']:
        destination = scratch / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(root / name, destination)
    for name, expected_hash in fixture['initial_test_hashes'].items():
        original = protected / 'baseline_tests' / name
        require(hashlib.sha256(original.read_bytes()).hexdigest() == expected_hash, 'protected original test changed')
        try:
            required_names = original_test_names(task, original.read_text(encoding='utf-8'))
            current_names = original_test_names(task, (root / name).read_text(encoding='utf-8'))
        except SyntaxError as error:
            raise Incorrect(f'candidate regression test does not parse: {error}') from error
        require(required_names <= current_names, f'original required tests removed or renamed: {sorted(required_names-current_names)}')
        destination = scratch / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(original, destination)
    if task == 'rust_bugfix':
        for name in ('Cargo.toml', 'Cargo.lock'):
            shutil.copy2(root / name, scratch / name)
        task_tests(scratch, 'rust')
    elif task == 'typescript_feature':
        shutil.copy2(root / 'package.json', scratch / 'package.json')
        task_tests(scratch, 'node')
    else:
        task_tests(scratch, 'python')


def main():
    fixture = json.loads(Path(sys.argv[1]).read_text(encoding='utf-8'))
    root = Path(fixture['workspace'])
    protected = Path(fixture['protected_dir'])
    allowed = set(fixture['initial_source_hashes']) | set(fixture['initial_test_hashes'])
    initial_workspace = fixture['initial_workspace_hashes']
    current_workspace = workspace_hashes(root)
    unexpected = sorted(name for name in set(initial_workspace) | set(current_workspace)
                        if name not in allowed and initial_workspace.get(name) != current_workspace.get(name))
    if unexpected:
        print(f'files changed outside permitted task scope: {unexpected}', flush=True)
        sys.exit(4)
    source_changed = []
    for name, initial in fixture['initial_source_hashes'].items():
        path = root / name
        require(path.is_file(), f'required source disappeared: {name}')
        source_changed.append(hashlib.sha256(path.read_bytes()).hexdigest() != initial)
    require(all(source_changed), 'requested source change was not made in every required file')
    for name, initial in fixture['initial_test_hashes'].items():
        path = root / name
        require(path.is_file() and hashlib.sha256(path.read_bytes()).hexdigest() != initial,
                f'regression tests were not added: {name}')
    verify_original_tests(fixture, root, protected)
    {'rust_bugfix': rust_verify, 'typescript_feature': typescript_verify,
     'kd4_python_refactor': python_verify}[fixture['task']](root, protected)
    require(workspace_hashes(root) == current_workspace,
            'focused tests changed final source or task inputs during independent verification')
    print('Independent behavior, requested change, and regression mutation verification passed.', flush=True)


if __name__ == '__main__':
    try:
        if len(sys.argv) > 1 and sys.argv[1] == '--python-oracle':
            python_oracle(Path(sys.argv[2]))
            print(sys.argv[3], flush=True)
        else:
            main()
    except Incorrect as error:
        print(str(error), flush=True)
        sys.exit(1)
    except subprocess.TimeoutExpired as error:
        print(f'Independent verification timed out: {error}', flush=True)
        sys.exit(3)
    except Exception as error:
        print(f'Independent verifier unavailable: {type(error).__name__}: {error}', flush=True)
        sys.exit(2)
