use crate::memory_extensions_root;
use std::io::Write as _;
use std::path::Path;

pub(super) const INSTRUCTIONS: &str =
    include_str!("../../templates/extensions/ad_hoc/instructions.md");

pub(super) async fn seed_instructions(memory_root: &Path) -> std::io::Result<()> {
    let extension_root = memory_extensions_root(memory_root).join("ad_hoc");
    let instructions_path = extension_root.join("instructions.md");

    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&extension_root)?;
        let mut file = tempfile::NamedTempFile::new_in(&extension_root)?;
        file.write_all(INSTRUCTIONS.as_bytes())?;
        file.flush()?;
        match file.persist_noclobber(&instructions_path) {
            Ok(_) => Ok(()),
            Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(err) => Err(err.error),
        }
    })
    .await
    .map_err(|err| std::io::Error::other(format!("instruction seeding task failed: {err}")))?
}

#[cfg(test)]
#[path = "ad_hoc_tests.rs"]
mod tests;
