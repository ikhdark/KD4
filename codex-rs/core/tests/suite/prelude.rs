// Shared Windows sandbox helpers for every `suite` integration target.
//
// This file is `include!`d (not declared as a module) so the test IDs stay
// `suite::<module>::<test>`. Arg0 helper dispatch comes from the ctor in
// `core_test_support`, which every shard links. Keep it free of `mod`
// declarations: the including target owns the module list it compiles.
#[cfg(target_os = "windows")]
#[allow(dead_code)]
pub(crate) fn lock_windows_sandbox_tests() -> anyhow::Result<std::fs::File> {
    use anyhow::Context;
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let test_exe = std::env::current_exe().context("resolve current Windows test executable")?;
    let test_exe_dir = test_exe
        .parent()
        .context("Windows test executable should have a parent directory")?;
    let resources_dir = test_exe_dir.join("codex-resources");
    std::fs::create_dir_all(&resources_dir)
        .with_context(|| format!("create resources dir {}", resources_dir.display()))?;
    let lock_path = resources_dir.join(".windows-sandbox-tests.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open Windows sandbox test lock {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("lock Windows sandbox tests at {}", lock_path.display()))?;
    Ok(lock)
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
pub(crate) fn stage_windows_sandbox_helpers() -> anyhow::Result<()> {
    use anyhow::Context;

    let test_exe = std::env::current_exe().context("resolve current Windows test executable")?;
    let test_exe_dir = test_exe
        .parent()
        .context("Windows test executable should have a parent directory")?;
    stage_windows_sandbox_helpers_in(&test_exe_dir.join("codex-resources"))
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
pub(crate) fn stage_windows_sandbox_helpers_in(
    resources_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use fs2::FileExt;
    use sha2::Digest;
    use sha2::Sha256;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::path::Path;

    fn digest(path: &Path) -> anyhow::Result<[u8; 32]> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("open staged helper candidate {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("read staged helper candidate {}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hasher.finalize().into())
    }

    fn already_staged(source: &Path, destination: &Path) -> anyhow::Result<bool> {
        let Ok(destination_metadata) = destination.metadata() else {
            return Ok(false);
        };
        if source.metadata()?.len() != destination_metadata.len() {
            return Ok(false);
        }
        Ok(digest(source)? == digest(destination)?)
    }

    match std::fs::create_dir_all(resources_dir) {
        Ok(()) => {}
        Err(err)
            if err.kind() == std::io::ErrorKind::PermissionDenied && resources_dir.is_dir() => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("create resources dir {}", resources_dir.display()));
        }
    }
    let staging_lock_path = resources_dir.join(".stage-windows-sandbox-helpers.lock");
    let staging_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&staging_lock_path)
        .with_context(|| format!("open helper staging lock {}", staging_lock_path.display()))?;
    staging_lock
        .lock_exclusive()
        .with_context(|| format!("lock helper staging at {}", staging_lock_path.display()))?;
    for helper_name in ["codex-windows-sandbox-setup", "codex-command-runner"] {
        let helper = codex_utils_cargo_bin::cargo_bin(helper_name)?;
        let file_name = Path::new(helper_name).with_extension("exe");
        let destination = resources_dir.join(file_name);
        // Windows prevents overwriting an executable while a sandbox process
        // still has it mapped. Avoid opening the destination for write when
        // the runner already staged this exact artifact. The digest check also
        // prevents a stale helper from being silently reused after a rebuild.
        if already_staged(&helper, &destination)? {
            continue;
        }
        if let Err(err) = std::fs::copy(&helper, &destination) {
            return Err(err).with_context(|| {
                format!(
                    "stage Windows sandbox helper {} at {}",
                    helper.display(),
                    destination.display()
                )
            });
        }
    }
    Ok(())
}
