use std::fs::File;
use std::io;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::models::BASE_INSTRUCTIONS_DEFAULT;

const DEFAULT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/prompts/base_instructions/default.md"
);
const MAX_BYTES: u64 = 1024 * 1024;

struct Snapshot {
    path: PathBuf,
    text: String,
    last_error: Option<String>,
}

/// Read the trusted default policy afresh, retaining the last valid text on failure.
/// Never discover policy relative to a task's cwd. Packaged binaries without the
/// build checkout retain the embedded fallback; relocated builds may select an
/// absolute source with CODEX_DEFAULT_INSTRUCTIONS_FILE before process startup.
pub fn default_base_instructions() -> String {
    static SNAPSHOT: OnceLock<Mutex<Snapshot>> = OnceLock::new();
    let snapshot = SNAPSHOT.get_or_init(|| {
        let path = std::env::var_os("CODEX_DEFAULT_INSTRUCTIONS_FILE")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PATH));
        Mutex::new(Snapshot {
            path,
            text: BASE_INSTRUCTIONS_DEFAULT.trim().to_string(),
            last_error: None,
        })
    });
    // Serialize bounded reads so an older concurrent acquisition cannot replace
    // a newer snapshot. This lock never spans an await or calls another owner.
    let mut snapshot = snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let result = (|| {
        let mut text = String::new();
        File::open(&snapshot.path)?
            .take(MAX_BYTES + 1)
            .read_to_string(&mut text)?;
        if text.len() as u64 > MAX_BYTES || text.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "default instructions must be nonempty UTF-8, at most 1 MiB",
            ));
        }
        Ok(text.trim().to_string())
    })();
    match result {
        Ok(text) => {
            snapshot.text = text;
            snapshot.last_error = None;
        }
        Err(error) => {
            let message = error.to_string();
            if snapshot.last_error.as_ref() != Some(&message) {
                tracing::debug!(path = %snapshot.path.display(), %error, "retaining last valid default instructions");
                snapshot.last_error = Some(message);
            }
        }
    }
    snapshot.text.clone()
}
