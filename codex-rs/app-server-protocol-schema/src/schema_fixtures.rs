use anyhow::Context;
use anyhow::Result;
use std::path::Path;

#[derive(Clone, Copy, Debug, Default)]
pub struct SchemaFixtureOptions {
    pub experimental_api: bool,
}

/// Regenerates schema fixtures with configurable options.
pub fn write_schema_fixtures_with_options(
    schema_root: &Path,
    prettier: Option<&Path>,
    options: SchemaFixtureOptions,
) -> Result<()> {
    std::fs::create_dir_all(schema_root)?;
    // One lock per root, held through generation, publication and rollback.
    // The file remains in place so all writers synchronize on the same inode.
    let publication_lock = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).truncate(false)
        .open(schema_root.join(".schema-publication.lock"))?;
    publication_lock.lock().context("failed to lock schema publication root")?;
    let staging = tempfile::Builder::new()
        .prefix(".schema-staging-")
        .tempdir_in(schema_root)?;
    let typescript_out_dir = staging.path().join("typescript");
    let json_out_dir = staging.path().join("json");

    crate::generate_ts_with_options(
        &typescript_out_dir,
        prettier,
        crate::GenerateTsOptions {
            experimental_api: options.experimental_api,
            ..crate::GenerateTsOptions::default()
        },
    )?;
    crate::generate_json_with_experimental(&json_out_dir, options.experimental_api)?;

    // Publish only after both generators (including the formatter) succeed.
    // Readers can observe the rename boundary, but successful writers cannot
    // interleave generations and failures restore already-published siblings.
    let mut published = Vec::new();
    for label in ["typescript", "json"] {
        let destination = schema_root.join(label);
        let previous = staging.path().join(format!("previous-{label}"));
        if let Err(error) = publish_fixture_subtree(
            &staging.path().join(label),
            &destination,
            &previous,
            |from, to| std::fs::rename(from, to),
        ) {
            let mut rollback_errors = Vec::new();
            for previous_label in published.iter().rev() {
                let destination = schema_root.join(previous_label);
                let replacement = staging.path().join(previous_label);
                let previous = staging.path().join(format!("previous-{previous_label}"));
                if let Err(rollback_error) = std::fs::rename(&destination, &replacement)
                    .and_then(|()| if previous.exists() { std::fs::rename(&previous, &destination) } else { Ok(()) })
                {
                    rollback_errors.push(format!("{previous_label}: {rollback_error}"));
                }
            }
            // A failed Windows rename must not delete either the replacement
            // or an old subtree whose rollback was also blocked.
            let recovery = staging.keep();
            return Err(error).with_context(|| {
                format!(
                    "schema publication failed; recovery data retained at {}; sibling rollback errors: {:?}",
                    recovery.display(), rollback_errors
                )
            });
        }
        published.push(label);
    }
    Ok(())
}

fn publish_fixture_subtree(
    source: &Path,
    destination: &Path,
    previous: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let had_previous = destination.exists();
    if had_previous {
        rename(destination, previous)
            .with_context(|| format!("failed to preserve {}", destination.display()))?;
    }
    if let Err(error) = rename(source, destination) {
        if had_previous {
            rename(previous, destination).with_context(|| {
                format!(
                    "failed to restore {}; previous data remains at {}; publish error: {error}",
                    destination.display(),
                    previous.display()
                )
            })?;
        }
        return Err(error).with_context(|| format!("failed to publish {}", destination.display()));
    }
    Ok(())
}
