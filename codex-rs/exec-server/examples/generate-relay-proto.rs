use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

const PROTO_FILE: &str = "codex.exec_server.relay.v1.proto";
const GENERATED_FILE: &str = "codex.exec_server.relay.v1.rs";

fn main() -> Result<()> {
    let check = parse_args()?;
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/proto");
    let generated = generate_binding(&proto_dir)?;
    sync_generated(&proto_dir.join(GENERATED_FILE), &generated, check)
}

fn parse_args() -> Result<bool> {
    let mut check = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--check" if !check => check = true,
            _ => bail!("usage: generate-relay-proto [--check]"),
        }
    }
    Ok(check)
}

fn generate_binding(proto_dir: &Path) -> Result<Vec<u8>> {
    let output_dir = tempfile::tempdir().context("create protobuf generation directory")?;
    let protoc = protoc_bin_vendored::protoc_bin_path().context("locate vendored protoc")?;
    let mut config = prost_build::Config::new();
    config.out_dir(output_dir.path()).protoc_executable(protoc);
    config
        .compile_protos(&[proto_dir.join(PROTO_FILE)], &[proto_dir])
        .context("generate relay protobuf binding")?;
    let generated = fs::read_to_string(output_dir.path().join(GENERATED_FILE))
        .context("read generated relay binding")?;
    Ok(normalize_newlines(&generated).into_bytes())
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn sync_generated(path: &Path, generated: &[u8], check: bool) -> Result<()> {
    let current = fs::read(path).ok();
    if current.as_deref() == Some(generated) {
        return Ok(());
    }
    if check {
        bail!(
            "{} is stale; run `just generate-exec-server-relay-proto`",
            path.display()
        );
    }

    let parent = path.parent().context("generated binding has no parent")?;
    fs::create_dir_all(parent).context("create generated binding directory")?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("create temporary generated binding")?;
    temporary
        .write_all(generated)
        .context("write temporary generated binding")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("install generated relay binding")?;
    Ok(())
}

