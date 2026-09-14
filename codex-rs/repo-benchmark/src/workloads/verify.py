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


class Incorrect(Exception):
    pass


START = time.monotonic()


def require(condition, message):
    if not condition:
        raise Incorrect(message)


def run(argv, cwd, expect_failure=False):
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
    if expect_failure:
        # Reject compilation/import/infrastructure failures: a working assertion must fail.
        combined = result.stdout + result.stderr
        require(result.returncode != 0 and any(marker in combined for marker in (
            "assertion `left == right` failed", "assertion failed:", "AssertionError", "ERR_ASSERTION")),
            "model tests did not reject a plausible wrong implementation through an assertion")
    else:
        require(result.returncode == 0, "task behavior or focused tests failed: " + " ".join(argv))
    return result


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
    run([str(executable)], protected)
    run(['cargo', 'test', '--offline', '--locked', '--jobs', '6', '--test', 'regression'], root)
    # Use the unchanged original defective implementation to test the submitted regression tests.
    scratch = Path(tempfile.mkdtemp(prefix='mutation-', dir=protected))
    for name in ('Cargo.toml', 'Cargo.lock'):
        shutil.copy2(root / name, scratch / name)
    shutil.copytree(root / 'tests', scratch / 'tests')
    (scratch / 'src').mkdir()
    (scratch / 'src/lib.rs').write_text('pub fn parse_duration(text: &str) -> Result<u64,String> {\n'
        'for (unit,scale) in [("ms",1),("s",1000),("m",60000),("h",3600000)] { '
        'if let Some(v)=text.trim().strip_suffix(unit) { return v.parse::<u64>().ok().and_then(|v| v.checked_mul(scale)).ok_or("invalid".into()); }} Err("invalid".into()) }', encoding='utf-8')
    run(['cargo', 'test', '--offline', '--locked', '--jobs', '6', '--test', 'regression'], scratch, expect_failure=True)


def typescript_verify(root, protected):
    oracle = protected / 'oracle.mjs'
    oracle.write_text(TS_ORACLE.replace('__PARSER__', json.dumps((root / 'src/parser.ts').as_uri()))
                      .replace('__REPORT__', json.dumps((root / 'src/report.ts').as_uri())), encoding='utf-8')
    run(['node', '--experimental-strip-types', str(oracle)], protected)
    run(['node', '--experimental-strip-types', '--test', 'tests/regression.test.ts'], root)
    scratch = Path(tempfile.mkdtemp(prefix='mutation-', dir=protected))
    shutil.copy2(root / 'package.json', scratch / 'package.json')
    shutil.copytree(root / 'src', scratch / 'src')
    shutil.copytree(root / 'tests', scratch / 'tests')
    (scratch / 'src/report.ts').write_text("import { parseRows } from './parser.ts';\n"
        "export function renderReport(text: string): string { const rows=parseRows(text); return [...rows.map(r=>`${r.name}: ${r.quantity}`), `TOTAL: ${rows.reduce((s,r)=>s+r.quantity,0)}`].join('\\n'); }\n", encoding='utf-8')
    run(['node', '--experimental-strip-types', '--test', 'tests/regression.test.ts'], scratch, expect_failure=True)


def python_verify(root, protected):
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
    run(['python', '-m', 'unittest', '-q', 'test_repo_benchmark_refactor'], root)
    scratch = Path(tempfile.mkdtemp(prefix='mutation-', dir=protected))
    (scratch / 'scripts').mkdir(parents=True, exist_ok=False)
    shutil.copy2(source, scratch / 'scripts/readme_toc.py')
    shutil.copy2(root / 'test_repo_benchmark_refactor.py', scratch / 'test_repo_benchmark_refactor.py')
    with (scratch / 'scripts/readme_toc.py').open('a', encoding='utf-8') as stream:
        stream.write('\ndef format_toc_entry(level, text, slug):\n    return f"- [{text}](#{slug})"\n')
    run(['python', '-m', 'unittest', '-q', 'test_repo_benchmark_refactor'], scratch, expect_failure=True)


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
        run(['cargo', 'test', '--offline', '--locked', '--jobs', '6', '--test', 'regression'], scratch)
    elif task == 'typescript_feature':
        shutil.copy2(root / 'package.json', scratch / 'package.json')
        run(['node', '--experimental-strip-types', '--test', 'tests/regression.test.ts'], scratch)
    else:
        run(['python', '-m', 'unittest', '-q', 'test_repo_benchmark_refactor'], scratch)


def main():
    fixture = json.loads(Path(sys.argv[1]).read_text(encoding='utf-8'))
    root = Path(fixture['workspace'])
    protected = Path(fixture['protected_dir'])
    allowed = set(fixture['initial_source_hashes']) | set(fixture['initial_test_hashes'])
    initial_workspace = fixture['initial_workspace_hashes']
    current_workspace = workspace_hashes(root)
    unexpected = sorted(name for name in set(initial_workspace) | set(current_workspace)
                        if name not in allowed and initial_workspace.get(name) != current_workspace.get(name))
    require(not unexpected, f'files changed outside permitted task scope: {unexpected}')
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
