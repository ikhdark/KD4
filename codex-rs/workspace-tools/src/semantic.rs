use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use std::collections::BTreeSet;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

const LIMIT: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub repository: PathBuf,
    pub queries: Vec<Query>,
    #[serde(default)]
    pub configuration: Configuration,
    #[serde(default)]
    pub compiler_check: bool,
    pub state_directory: Option<PathBuf>,
    pub migration_id: Option<String>,
    #[serde(default)]
    pub reviewed_consumers: BTreeSet<String>,
}

#[derive(Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub no_default_features: bool,
    pub target: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub build_scripts: bool,
    #[serde(default)]
    pub procedural_macros: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub path: PathBuf,
    /// One-based source line and UTF-16 column (the LSP wire uses zero-based).
    pub line: u32,
    pub column: u32,
}

struct Server {
    child: Child,
    input: ChildStdin,
    messages: mpsc::Receiver<anyhow::Result<Value>>,
    id: u64,
    deadline: Instant,
    ready: bool,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_message(reader: &mut impl BufRead) -> anyhow::Result<Value> {
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        anyhow::ensure!(
            reader.read_line(&mut line)? != 0,
            "language server closed its output"
        );
        header_bytes += line.len();
        anyhow::ensure!(header_bytes <= 8192, "oversized LSP header");
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            anyhow::ensure!(length.is_none(), "duplicate LSP Content-Length");
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("missing LSP Content-Length")?;
    anyhow::ensure!(length <= LIMIT, "LSP response exceeds 8 MiB");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

impl Server {
    fn start(root: &Path, config: &Configuration) -> anyhow::Result<Self> {
        let mut child = crate::command("rust-analyzer")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context(
                "rust-analyzer must be installed on PATH in the selected execution environment",
            )?;
        let input = child.stdin.take().context("language server stdin")?;
        let output = child.stdout.take().context("language server stdout")?;
        let (sender, messages) = mpsc::sync_channel(32);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let message = read_message(&mut reader);
                let failed = message.is_err();
                if sender.send(message).is_err() || failed {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            input,
            messages,
            id: 0,
            deadline: Instant::now() + Duration::from_secs(120),
            ready: false,
        };
        server.request("initialize", json!({
            "processId":std::process::id(),"rootUri":uri(root)?,
            "capabilities":{"general":{"positionEncodings":["utf-16"]},"experimental":{"serverStatusNotification":true},
                "textDocument":{"hover":{"contentFormat":["markdown","plaintext"]}}},
            "initializationOptions":{"checkOnSave":false,"cargo":{"buildScripts":{"enable":config.build_scripts},
                "features":config.features,"noDefaultFeatures":config.no_default_features,"target":config.target},
                "procMacro":{"enable":config.procedural_macros}}
        }))?;
        server.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}))?;
        while !server.ready {
            let message = server.receive()?;
            server.handle_notification(&message)?;
        }
        Ok(server)
    }

    fn send(&mut self, value: Value) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(&value)?;
        write!(self.input, "Content-Length: {}\r\n\r\n", bytes.len())?;
        self.input.write_all(&bytes)?;
        self.input.flush()?;
        Ok(())
    }

    fn receive(&self) -> anyhow::Result<Value> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .context("semantic query exceeded its 120 second deadline; no complete result")?;
        self.messages
            .recv_timeout(remaining)
            .context("language server did not finish before the deadline")?
    }

    fn handle_notification(&mut self, message: &Value) -> anyhow::Result<()> {
        if message["method"] == "experimental/serverStatus" {
            self.ready = message["params"]["quiescent"] == true;
            if message["params"]["health"] == "error" {
                anyhow::bail!(
                    "language server workspace load failed: {}",
                    message["params"]
                );
            }
        }
        if !message["method"].is_null() && !message["id"].is_null() {
            let result = match message["method"].as_str() {
                Some("workspace/configuration") => json!(
                    message["params"]["items"]
                        .as_array()
                        .map(|items| vec![Value::Null; items.len()])
                        .unwrap_or_default()
                ),
                Some("workspace/applyEdit") => {
                    json!({"applied":false,"failureReason":"semantic_context never applies language-server edits"})
                }
                _ => Value::Null,
            };
            self.send(json!({"jsonrpc":"2.0","id":message["id"],"result":result}))?;
        }
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.id += 1;
        let id = self.id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        loop {
            let message = self.receive()?;
            if message["id"] == id && message["method"].is_null() {
                anyhow::ensure!(message["error"].is_null(), "{method}: {}", message["error"]);
                return Ok(message["result"].clone());
            }
            self.handle_notification(&message)?;
        }
    }
}

fn uri(path: &Path) -> anyhow::Result<String> {
    url::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|()| anyhow::anyhow!("cannot form file URI for {}", path.display()))
}

fn locations(value: Value) -> Vec<Value> {
    match value {
        Value::Array(values) => values,
        Value::Null => vec![],
        value => vec![value],
    }
}

fn excerpt(location: &Value) -> anyhow::Result<Value> {
    let location = location.get("from").unwrap_or(location);
    let file_uri = location
        .get("uri")
        .or_else(|| location.get("targetUri"))
        .and_then(Value::as_str)
        .context("definition has no file URI")?;
    let path = url::Url::parse(file_uri)?
        .to_file_path()
        .map_err(|()| anyhow::anyhow!("non-file language-server location"))?;
    let path = fs::canonicalize(path)?;
    anyhow::ensure!(
        fs::metadata(&path)?.len() <= LIMIT as u64,
        "source file exceeds 8 MiB"
    );
    let bytes = fs::read(&path)?;
    anyhow::ensure!(bytes.len() <= LIMIT, "source changed beyond read limit");
    let source = std::str::from_utf8(&bytes)?;
    let range = location
        .get("targetRange")
        .or_else(|| location.get("range"))
        .context("location has no range")?;
    let start = range["start"]["line"].as_u64().context("location start")? as usize;
    let end = range["end"]["line"].as_u64().context("location end")? as usize;
    if let Ok(mut unit) = crate::source_units::enclosing(source, start + 1) {
        unit["path"] = json!(path);
        unit["range"] = range.clone();
        unit["sha256"] = json!(format!("{:x}", sha2::Sha256::digest(&bytes)));
        unit["excerpt"] = json!(false);
        unit["range_truncated"] = json!(false);
        return Ok(unit);
    }
    let first = start.saturating_sub(4);
    let wanted_end = end.saturating_add(13);
    let last = wanted_end.min(first + 100);
    let text: String = source
        .lines()
        .enumerate()
        .skip(first)
        .take(last.saturating_sub(first))
        .map(|(i, line)| format!("{}: {line}\n", i + 1))
        .collect();
    Ok(
        json!({"path":path,"sha256":format!("{:x}",sha2::Sha256::digest(&bytes)),"range":range,
        "start_line":first+1,"text":text,"excerpt":true,"range_truncated":last<=end}),
    )
}

pub fn run(request: Request) -> anyhow::Result<Value> {
    anyhow::ensure!(
        !request.queries.is_empty() && request.queries.len() <= 16,
        "provide 1-16 source positions"
    );
    let root = fs::canonicalize(&request.repository)?;
    let lock = root.join("Cargo.lock");
    let lock_before = fs::read(&lock).ok();
    let mut inputs = Vec::new();
    for query in &request.queries {
        anyhow::ensure!(
            query.line > 0 && query.column > 0,
            "line and UTF-16 column are one-based"
        );
        let path = fs::canonicalize(root.join(&query.path))?;
        anyhow::ensure!(
            path.starts_with(&root) && path.extension().is_some_and(|e| e == "rs"),
            "query must select a Rust source file inside the repository"
        );
        let bytes = fs::read(&path)?;
        anyhow::ensure!(bytes.len() <= LIMIT, "query source exceeds 8 MiB");
        let source = std::str::from_utf8(&bytes)?;
        let line = source
            .lines()
            .nth((query.line - 1) as usize)
            .context("query line out of range")?;
        anyhow::ensure!(
            (query.column - 1) as usize <= line.encode_utf16().count(),
            "query column out of range"
        );
        inputs.push((path, bytes));
    }
    let mut server = Server::start(&root, &request.configuration)?;
    let mut results = Vec::new();
    let mut consumers = Vec::new();
    let mut opened = BTreeSet::new();
    for (query, (path, bytes)) in request.queries.iter().zip(&inputs) {
        let file_uri = uri(path)?;
        if opened.insert(file_uri.clone()) {
            server.send(json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{
                "uri":file_uri,"languageId":"rust","version":1,"text":std::str::from_utf8(bytes)?}}}))?;
        }
        let position = json!({"textDocument":{"uri":file_uri},"position":{"line":query.line-1,"character":query.column-1}});
        let hover = server.request("textDocument/hover", position.clone())?;
        let definitions = locations(server.request("textDocument/definition", position.clone())?);
        let types = locations(server.request("textDocument/typeDefinition", position.clone())?);
        let mut reference_params = position.clone();
        reference_params["context"] = json!({"includeDeclaration":false});
        let references = locations(server.request("textDocument/references", reference_params)?);
        let hierarchy =
            locations(server.request("textDocument/prepareCallHierarchy", position.clone())?);
        let mut callers = Vec::new();
        for item in hierarchy {
            callers.extend(locations(
                server.request("callHierarchy/incomingCalls", json!({"item":item}))?,
            ));
        }
        let mut seen = BTreeSet::new();
        let mut seen_units = BTreeSet::new();
        let mut bundles = Vec::new();
        let own = json!({"uri":file_uri,"range":{"start":{"line":query.line-1,"character":query.column-1},"end":{"line":query.line-1,"character":query.column-1}}});
        for location in std::iter::once(&own)
            .chain(definitions.iter())
            .chain(&types)
            .chain(callers.iter())
            .chain(references.iter())
        {
            if !seen.insert(location.to_string()) {
                continue;
            }
            let source = match excerpt(location) {
                Ok(value) => value,
                Err(error) => json!({"location":location,"unavailable":error.to_string()}),
            };
            let identity = json!([
                source["path"],
                source["sha256"],
                source["start_line"],
                source["end_line"]
            ])
            .to_string();
            if source["complete"] != true || seen_units.insert(identity) {
                bundles.push(source);
            }
        }
        for reference in &references {
            let id = format!("{:x}", sha2::Sha256::digest(reference.to_string()));
            let mut source =
                excerpt(reference).unwrap_or_else(|error| json!({"unavailable":error.to_string()}));
            // Each full unit is already in source_bundle. Worklists keep every
            // reference and its source identity without repeating source text.
            if let Some(object) = source.as_object_mut() {
                object.remove("text");
            }
            consumers.push(json!({"id":id,"location":reference,"source":source}));
        }
        results.push(json!({"path":path,"line":query.line,"column":query.column,
            "resolved":!definitions.is_empty(),"hover":hover,"definitions":definitions,"types":types,
            "reference_count":references.len(),"references":references,
            "caller_count":callers.len(),"callers":callers,
            "references_truncated":false,"callers_truncated":false,"source_bundle":bundles}));
    }
    for (path, bytes) in &inputs {
        anyhow::ensure!(
            fs::read(path)? == *bytes,
            "query source changed during semantic resolution; repeat against current source"
        );
    }
    anyhow::ensure!(
        fs::read(&lock).ok() == lock_before,
        "Cargo.lock changed during semantic resolution"
    );
    let compiler = if request.compiler_check {
        let state = request
            .state_directory
            .as_ref()
            .context("compiler_check requires state_directory")?;
        let mut args = vec!["check".into(), "--locked".into(), "--all-targets".into()];
        if request.configuration.packages.is_empty() {
            args.push("--workspace".into());
        }
        for package in &request.configuration.packages {
            args.extend(["-p".into(), package.clone()]);
        }
        if !request.configuration.features.is_empty() {
            args.extend([
                "--features".into(),
                request.configuration.features.join(","),
            ]);
        }
        if request.configuration.no_default_features {
            args.push("--no-default-features".into());
        }
        if let Some(target) = &request.configuration.target {
            args.extend(["--target".into(), target.clone()]);
        }
        Some(crate::validation::run(crate::validation::Request {
            repository: root.clone(),
            cache_directory: state.join("compiler"),
            cache_identity: None,
            action: crate::validation::Action::Run,
            checks: vec![crate::validation::Check {
                id: "semantic-compiler-check".into(),
                args,
            }],
            allow_full_suite: false,
            force_fresh: false,
        })?)
    } else {
        None
    };
    // Compilation can outlive resolution. Never attach verification or reviews to
    // source that changed while either phase was running.
    for (path, bytes) in &inputs {
        anyhow::ensure!(
            fs::read(path)? == *bytes,
            "query source changed during compiler verification"
        );
    }
    anyhow::ensure!(
        fs::read(&lock).ok() == lock_before,
        "Cargo.lock changed during compiler verification"
    );
    for source in results
        .iter()
        .flat_map(|result| result["source_bundle"].as_array().into_iter().flatten())
        .chain(consumers.iter().map(|consumer| &consumer["source"]))
    {
        if let (Some(path), Some(expected)) = (source["path"].as_str(), source["sha256"].as_str()) {
            let actual = format!("{:x}", sha2::Sha256::digest(fs::read(path)?));
            anyhow::ensure!(
                actual == expected,
                "resolved source changed during semantic verification: {path}"
            );
        }
    }
    let migration = if let Some(id) = &request.migration_id {
        Some(crate::migration::update(
            &request
                .state_directory
                .as_ref()
                .context("migration requires state_directory")?
                .join("migrations"),
            id,
            &root,
            consumers,
            &request.reviewed_consumers,
            request.configuration.packages.is_empty()
                && compiler.as_ref().is_some_and(|c| c["success"] == true),
        )?)
    } else {
        None
    };
    Ok(
        json!({"success":compiler.as_ref().is_none_or(|c| c["success"] == true),"engine":"rust-analyzer","repository":root,"configuration":request.configuration,
        "compiler":compiler,"migration":migration,
        "cargo_lock_sha256":lock_before.map(|bytes|format!("{:x}",sha2::Sha256::digest(bytes))),
        "limitations":["build scripts and procedural macros follow configuration; unresolved symbols do not establish absence",
            "parsed Rust units are complete; fallback excerpts mark incomplete coverage; generated or macro consumers may require additional checks",
            "migration completion requires compiler_check without a packages filter, so every workspace consumer is checked"],"results":results}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_compiler_and_persistent_migration_are_wired_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='configured_fixture'\nversion='0.1.0'\nedition='2021'\n[features]\nselected=[]\n").unwrap();
        let source = "#[cfg(feature=\"selected\")]\npub fn answer() -> u32 { 42 }\n#[cfg(feature=\"selected\")]\npub fn caller() -> u32 { answer() }\n";
        fs::write(root.join("src/lib.rs"), source).unwrap();
        let status = crate::command("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let query = |reviewed: BTreeSet<String>| {
            run(Request {
                repository: root.clone(),
                queries: vec![Query {
                    path: "src/lib.rs".into(),
                    line: 2,
                    column: 9,
                }],
                configuration: Configuration {
                    features: vec!["selected".into()],
                    ..Default::default()
                },
                compiler_check: true,
                state_directory: Some(dir.path().join("state")),
                migration_id: Some("representation".into()),
                reviewed_consumers: reviewed,
            })
            .unwrap()
        };
        let initial = query(BTreeSet::new());
        assert_eq!(initial["compiler"]["success"], true, "{initial}");
        assert_eq!(initial["migration"]["complete"], false);
        let consumers = initial["migration"]["consumers"].as_array().unwrap();
        assert!(
            !consumers.is_empty(),
            "configured reference must be discovered: {initial}"
        );
        assert!(
            initial["results"][0]["source_bundle"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["complete"] == true && s["edit_handle"].is_string())
        );
        let handles = initial["results"][0]["source_bundle"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["edit_handle"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            handles.len(),
            handles.iter().collect::<BTreeSet<_>>().len(),
            "return each complete unit once"
        );
        assert!(consumers.iter().all(|c| c["source"].get("text").is_none()));
        let reviewed = consumers
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect();
        let complete = query(reviewed);
        assert_eq!(complete["migration"]["complete"], true, "{complete}");
        fs::write(
            root.join("src/lib.rs"),
            source.replace("{ 42 }", "{ false }"),
        )
        .unwrap();
        let failed = query(BTreeSet::new());
        assert_eq!(failed["success"], false, "{failed}");
        assert_eq!(failed["migration"]["complete"], false);
        assert!(
            !failed["compiler"]["checks"][0]["inventory"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn lsp_frames_respect_byte_lengths_and_reject_oversize() {
        let value = json!({"result":"λ"});
        let bytes = serde_json::to_vec(&value).unwrap();
        let mut framed = format!("Content-Length: {}\r\n\r\n", bytes.len()).into_bytes();
        framed.extend(bytes);
        assert_eq!(
            read_message(&mut std::io::Cursor::new(framed)).unwrap(),
            value
        );
        assert!(
            read_message(&mut std::io::Cursor::new(
                b"Content-Length: 999999999\r\n\r\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn real_compiler_resolves_exact_path_dependency_and_callers() {
        // Deliberately mandatory: this gate proves actual semantic resolution.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("dep/src")).unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname='semantic_fixture'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nfixture_dep={path='dep'}\n").unwrap();
        fs::write(
            dir.path().join("dep/Cargo.toml"),
            "[package]\nname='fixture_dep'\nversion='0.7.3'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("dep/src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn caller() -> u32 { fixture_dep::answer() }\n",
        )
        .unwrap();
        let output = crate::command("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = run(Request {
            repository: dir.path().into(),
            queries: vec![Query {
                path: "src/lib.rs".into(),
                line: 1,
                column: 40,
            }],
            configuration: Configuration::default(),
            compiler_check: false,
            state_directory: None,
            migration_id: None,
            reviewed_consumers: BTreeSet::new(),
        })
        .unwrap();
        assert_eq!(result["results"][0]["resolved"], true, "{result}");
        assert!(
            result["results"][0]["definitions"]
                .to_string()
                .contains("dep/src/lib.rs"),
            "{result}"
        );
        assert!(
            result["results"][0]["hover"].to_string().contains("u32"),
            "{result}"
        );
        assert!(
            result["results"][0]["caller_count"].as_u64().unwrap() >= 1,
            "{result}"
        );
        assert!(
            result["results"][0]["source_bundle"]
                .to_string()
                .contains("42"),
            "{result}"
        );
    }
}
