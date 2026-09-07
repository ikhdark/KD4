//! Parse test declarations without executing their source. This is a completeness
//! check for review, not a substitute for the independent behavioral assessment.

use super::*;
use syn::spanned::Spanned;
use syn::visit::Visit;
use tokio::io::AsyncWriteExt;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TestDeclaration {
    pub(super) name: String,
    pub(super) body: String,
    #[serde(default)]
    pub(super) context: String,
}

impl TestDeclaration {
    // Baseline discovery ignores a checkout's newline convention. The body
    // itself stays exact for reviewer output and failed/passing comparisons.
    pub(super) fn matches_baseline(&self, old: &Self) -> bool {
        self.name == old.name
            && self.body.replace("\r\n", "\n") == old.body.replace("\r\n", "\n")
            && self.context == old.context
    }
}

pub(super) async fn test_declarations(
    root: &Path,
    path: &str,
    source: &str,
    observed_pytest_module: bool,
) -> Result<Vec<TestDeclaration>, String> {
    if source.is_empty() {
        return Ok(Vec::new());
    }
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("rs") => rust_declarations(source),
        Some("py") => {
            let collect_functions =
                super::test_quality::dedicated_test_path(path) || observed_pytest_module;
            parse_with_process(
                root,
                "python",
                &[
                    "-X",
                    "utf8",
                    "-I",
                    "-S",
                    "-c",
                    PYTHON_DECLARATIONS,
                    if collect_functions {
                        "functions"
                    } else {
                        "methods"
                    },
                ],
                source,
            )
            .await
        }
        Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs") => {
            let input =
                serde_json::to_string(&serde_json::json!({"path":root.join(path),"source":source}))
                    .map_err(|e| e.to_string())?;
            parse_with_process(root, "node", &["-e", JAVASCRIPT_DECLARATIONS], &input).await
        }
        _ => Err(format!(
            "changed test source {path} has no supported declaration parser; its quality obligation is unsatisfied"
        )),
    }
}

struct RustDeclarations<'a> {
    lines: Vec<&'a str>,
    declarations: Vec<TestDeclaration>,
    context: String,
}

impl<'ast> Visit<'ast> for RustDeclarations<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let previous = self.context.clone();
        let test_module = item.ident == "tests"
            || item.attrs.iter().any(|attribute| {
                attribute.path().is_ident("cfg")
                    && matches!(&attribute.meta,
                syn::Meta::List(value) if value.tokens.to_string() == "test")
            });
        if test_module && let Some((_, items)) = &item.content {
            let mut context = previous.clone();
            for child in items {
                let is_test = matches!(child, syn::Item::Fn(function) if function.attrs.iter().any(|attribute|
                    attribute.path().segments.last().is_some_and(|segment| matches!(segment.ident.to_string().as_str(), "test" | "rstest" | "test_case"))));
                if !is_test {
                    let span: proc_macro2::Span = child.span();
                    let (start, end) = (span.start().line, span.end().line);
                    if start > 0 && end <= self.lines.len() {
                        context.push_str(&self.lines[start - 1..end].concat());
                    }
                }
            }
            self.context = format!(
                "{:x}",
                Sha256::digest(context.replace("\r\n", "\n").as_bytes())
            );
        }
        syn::visit::visit_item_mod(self, item);
        self.context = previous;
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if item.attrs.iter().any(|a| {
            a.path().segments.last().is_some_and(|s| {
                let name = s.ident.to_string();
                name == "test" || name == "rstest" || name == "test_case"
            })
        }) {
            let start = item
                .attrs
                .iter()
                .map(|a| a.span().start().line)
                .min()
                .unwrap_or(item.span().start().line);
            let end = item.span().end().line;
            if start > 0 && end <= self.lines.len() {
                self.declarations.push(TestDeclaration {
                    name: item.sig.ident.to_string(),
                    body: self.lines[start - 1..end].concat(),
                    context: self.context.clone(),
                });
            }
        }
        syn::visit::visit_item_fn(self, item);
    }
}

fn rust_declarations(source: &str) -> Result<Vec<TestDeclaration>, String> {
    let file =
        syn::parse_file(source).map_err(|e| format!("cannot parse changed Rust tests: {e}"))?;
    let mut parser = RustDeclarations {
        lines: source.split_inclusive('\n').collect(),
        declarations: Vec::new(),
        context: String::new(),
    };
    parser.visit_file(&file);
    Ok(parser.declarations)
}

async fn parse_with_process(
    root: &Path,
    executable: &str,
    args: &[&str],
    input: &str,
) -> Result<Vec<TestDeclaration>, String> {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = codex_utils_pty::with_windows_child_creation(|_| command.spawn())
        .map_err(|e| format!("cannot start read-only test declaration parser: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("declaration parser has no input channel")?;
    stdin
        .write_all(input.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    drop(stdin);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .map_err(|_| "test declaration parser timed out")?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "test declaration parsing is unsatisfied: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("invalid test declaration parser result: {e}"))
}

const PYTHON_DECLARATIONS: &str = r#"import ast, json, sys
source = sys.stdin.buffer.read().decode("utf-8")
lines = source.splitlines(keepends=True)
tree = ast.parse(source)
parents = {child: parent for parent in ast.walk(tree) for child in ast.iter_child_nodes(parent)}
collect_functions = sys.argv[1] == 'functions'
result = []
for node in ast.walk(tree):
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith('test'):
        # Ordinary module helpers are not unittest cases. Dedicated test modules
        # and exact native pytest observations also admit free test functions.
        # Keep class methods conservative for imported/custom TestCase bases.
        marked_pytest = any(ast.unparse(d).startswith('pytest.mark.') for d in node.decorator_list)
        if not collect_functions and not isinstance(parents.get(node), ast.ClassDef) and not marked_pytest:
            continue
        start = min([node.lineno] + [d.lineno for d in node.decorator_list])
        result.append({'name': node.name, 'body': ''.join(lines[start - 1:node.end_lineno])})
print(json.dumps(result))
"#;

const JAVASCRIPT_DECLARATIONS: &str = r#"const fs = require('node:fs');
const {createRequire} = require('node:module');
const input = JSON.parse(fs.readFileSync(0, 'utf8'));
const ts = createRequire(input.path)('typescript');
const file = ts.createSourceFile(input.path, input.source, ts.ScriptTarget.Latest, true);
if (file.parseDiagnostics.length) throw new Error('Test source has parse errors');
const result = [];
function visit(node) {
  if (ts.isCallExpression(node)) {
    const expression = node.expression.getText(file);
    if (/^(test|it)(\.(only|skip|todo|concurrent))?$/.test(expression)) {
      const title = node.arguments[0];
      if (!title || !ts.isStringLiteralLike(title)) throw new Error('Dynamic test titles need a native declaration adapter');
      result.push({name:title.text, body:node.getText(file)});
    } else if (/^(test|it)\./.test(expression)) {
      throw new Error('Parameterized test declarations need a native case adapter');
    }
  }
  ts.forEachChild(node, visit);
}
visit(file);
process.stdout.write(JSON.stringify(result));
"#;
