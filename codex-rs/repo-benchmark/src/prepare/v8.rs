//! Source-pinned OpenAI V8 release assets, following setup-rusty-v8's trust chain.
use super::environment::Environment;
use super::provenance::FileIdentity;
use super::provenance::command_output;
use super::provenance::git_path;
use super::provenance::hash_bytes;
use super::provenance::hash_file;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseArtifact {
    pub url: String,
    pub file: FileIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V8Artifacts {
    pub version: String,
    pub target: String,
    pub features: BTreeSet<String>,
    pub profile: String,
    pub trust_source: FileIdentity,
    pub trust_commit_blob_sha256: String,
    pub manifest: ReleaseArtifact,
    pub archive: ReleaseArtifact,
    pub binding: ReleaseArtifact,
}

impl V8Artifacts {
    pub fn verify(&self) -> Result<()> {
        self.trust_source.verify()?;
        self.manifest.file.verify()?;
        self.archive.file.verify()?;
        self.binding.file.verify()?;
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<String> {
        Ok(hash_bytes(&serde_json::to_vec(&(
            &self.version,
            &self.target,
            &self.features,
            &self.profile,
            &self.trust_commit_blob_sha256,
            (&self.manifest.url, &self.manifest.file.sha256),
            (&self.archive.url, &self.archive.file.sha256),
            (&self.binding.url, &self.binding.file.sha256),
        ))?))
    }

    pub fn settings(&self) -> [(String, String); 2] {
        [
            (
                "RUSTY_V8_ARCHIVE".into(),
                git_path(&self.archive.file.path)
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "RUSTY_V8_SRC_BINDING_PATH".into(),
                git_path(&self.binding.file.path)
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]
    }
}

// Remove external switches even for revisions using the ordinary upstream V8
// downloader. Only this preparation may supply archive/binding overrides.
pub fn clear_inherited_overrides(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy().to_ascii_uppercase();
        if text.starts_with("RUSTY_V8_")
            || text.starts_with("V8_")
            || [
                "GN_ARGS",
                "EXTRA_GN_ARGS",
                "PRINT_GN_ARGS",
                "DOCS_RS",
                "DENO_TRYBUILD",
            ]
            .contains(&text.as_str())
        {
            command.env_remove(name);
        }
    }
}

fn pinned_version(lock: &str) -> Result<Option<String>> {
    let lock: toml::Value = toml::from_str(lock)?;
    let versions: BTreeSet<_> = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|package| package.get("name").and_then(toml::Value::as_str) == Some("v8"))
        .map(|package| {
            package
                .get("version")
                .and_then(toml::Value::as_str)
                .context("locked V8 version")
        })
        .collect::<Result<_>>()?;
    ensure!(
        versions.len() <= 1,
        "multiple locked V8 versions require distinct native artifact selections"
    );
    let Some(version) = versions.into_iter().next() else {
        return Ok(None);
    };
    ensure!(
        version.split('.').count() == 3
            && version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())),
        "unsupported locked V8 version {version}"
    );
    Ok(Some(version.into()))
}

fn target_for_build(source: &Path, environment: &Environment) -> Result<String> {
    let config = source.join("codex-rs/.cargo/config.toml");
    if config.is_file() {
        let config: toml::Value = toml::from_str(&fs::read_to_string(config)?)?;
        if let Some(target) = config.get("build").and_then(|build| build.get("target")) {
            return target
                .as_str()
                .map(str::to_owned)
                .context("V8 preparation requires one source-selected build target");
        }
    }
    let rustc = &environment
        .tools
        .get("rustc")
        .context("pinned rustc identity")?
        .executable
        .path;
    let output = command_output(
        Command::new(rustc)
            .args(["--version", "--verbose"])
            .current_dir(source.join("codex-rs"))
            .env("RUSTUP_TOOLCHAIN", &environment.rust_toolchain),
    )?;
    let output = std::str::from_utf8(&output)?;
    output
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .context("pinned rustc did not report a host target")
}

fn selected_features(tree: &str, version: &str) -> Result<BTreeSet<String>> {
    let mut selections = BTreeSet::new();
    for line in tree.lines() {
        let Some((package, features)) = line.split_once('|') else {
            continue;
        };
        let mut package = package.split_whitespace();
        if package.next() != Some("v8") {
            continue;
        }
        ensure!(
            package.next() == Some(format!("v{version}").as_str()),
            "Cargo resolved V8 outside the pinned version"
        );
        let features = features
            .trim()
            .strip_suffix(" (*)")
            .unwrap_or(features.trim());
        selections.insert(
            features
                .split(',')
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<BTreeSet<_>>(),
        );
    }
    ensure!(
        selections.len() == 1,
        "expected one V8 feature selection for the requested native binaries"
    );
    selections
        .into_iter()
        .next()
        .context("expected one V8 feature selection for the requested native binaries")
}

fn artifact_names(target: &str, features: &BTreeSet<String>) -> Result<(String, String, String)> {
    ensure!(
        !target.is_empty()
            && target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "unsupported V8 target {target}"
    );
    ensure!(
        features.contains("v8_enable_sandbox")
            && features.contains("v8_enable_pointer_compression"),
        "source's trusted release assets require sandbox and pointer compression features"
    );
    ensure!(
        !features.contains("simdutf") && !features.contains("v8_enable_v8_checks"),
        "source selects V8 features absent from the trusted release profile"
    );
    let profile = "ptrcomp_sandbox_release";
    let archive = if target.ends_with("-pc-windows-msvc") {
        format!("rusty_v8_{profile}_{target}.lib.gz")
    } else {
        format!("librusty_v8_{profile}_{target}.a.gz")
    };
    Ok((
        archive,
        format!("src_binding_{profile}_{target}.rs"),
        format!("rusty_v8_{profile}_{target}.sha256"),
    ))
}

fn checksums(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for line in std::str::from_utf8(bytes)?.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(
            fields.len() == 2
                && fields[0].len() == 64
                && fields[0].bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid V8 checksum line"
        );
        let name = fields[1].strip_prefix('*').unwrap_or(fields[1]);
        ensure!(
            !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != "..",
            "invalid V8 checksum filename"
        );
        ensure!(
            result
                .insert(name.into(), fields[0].to_ascii_lowercase())
                .is_none(),
            "duplicate V8 checksum entry {name}"
        );
    }
    Ok(result)
}

fn verified_artifact(url: String, path: PathBuf, expected: &str) -> Result<ReleaseArtifact> {
    if !path.is_file() {
        let partial = path.with_extension(format!(
            "{}.partial",
            path.extension()
                .and_then(|value| value.to_str())
                .unwrap_or("download")
        ));
        command_output(
            Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--proto",
                    "=https",
                    "--proto-redir",
                    "=https",
                    "--connect-timeout",
                    "30",
                    "--max-time",
                    "600",
                    "--retry",
                    "2",
                    "--output",
                ])
                .arg(git_path(&partial))
                .arg(&url),
        )
        .with_context(|| format!("download source-pinned V8 artifact {url}"))?;
        ensure!(
            hash_file(&partial)? == expected,
            "downloaded V8 checksum mismatch: {url}; evidence retained at {}",
            partial.display()
        );
        fs::rename(&partial, &path)?;
    }
    let file = FileIdentity::record(&path)?;
    ensure!(
        file.sha256 == expected,
        "cached V8 checksum mismatch: {}",
        path.display()
    );
    Ok(ReleaseArtifact { url, file })
}

pub fn prepare(
    source: &Path,
    revision: &str,
    target_root: &Path,
    environment: &Environment,
    requested: &[(&str, &str)],
) -> Result<Option<V8Artifacts>> {
    if !source.join("third_party/v8").is_dir() {
        return Ok(None);
    }
    let workspace = source.join("codex-rs");
    let Some(version) = pinned_version(&fs::read_to_string(workspace.join("Cargo.lock"))?)? else {
        return Ok(None);
    };
    let trust_relative = format!(
        "third_party/v8/rusty_v8_{}_release_manifests.sha256",
        version.replace('.', "_")
    );
    let trust_path = source.join(&trust_relative);
    // Older fork revisions use the ordinary V8 release and retain their cache key.
    if !trust_path.is_file() {
        return Ok(None);
    }
    let trust_source = FileIdentity::record(&trust_path)?;
    let trusted_bytes = command_output(
        Command::new("git")
            .arg("-C")
            .arg(git_path(source))
            .args(["show", &format!("{revision}:{trust_relative}")]),
    )?;
    let trusted = checksums(&trusted_bytes)?;
    ensure!(
        checksums(&fs::read(&trust_path)?)? == trusted,
        "V8 trust file differs from pinned committed content"
    );
    let target = target_for_build(source, environment)?;
    let cargo = &environment
        .tools
        .get("cargo")
        .context("pinned Cargo identity")?
        .executable
        .path;
    let mut tree = Command::new(cargo);
    tree.current_dir(&workspace)
        .args([
            "tree",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}|{f}",
            "--target",
            &target,
        ])
        .env("RUSTUP_TOOLCHAIN", &environment.rust_toolchain)
        .env_remove("CARGO_TARGET_DIR");
    // Several requested binaries can share one package; pass each package once.
    #[expect(
        clippy::needless_collect,
        reason = "the set deduplicates packages shared by multiple requested binaries"
    )]
    for package in requested
        .iter()
        .map(|(package, _)| *package)
        .collect::<BTreeSet<_>>()
    {
        tree.args(["-p", package]);
    }
    clear_inherited_overrides(&mut tree);
    let output = command_output(&mut tree)
        .context("resolve native V8 feature selection without building")?;
    let features = selected_features(std::str::from_utf8(&output)?, &version)?;
    let (archive_name, binding_name, manifest_name) = artifact_names(&target, &features)?;
    let expected_manifest = trusted
        .get(&manifest_name)
        .context("source has no trusted V8 release manifest for selected target/profile")?;
    let directory = target_root
        .join("v8")
        .join(format!("{version}-{target}-{}", &expected_manifest[..16]));
    fs::create_dir_all(&directory)?;
    let base = format!("https://github.com/openai/codex/releases/download/rusty-v8-v{version}");
    let manifest = verified_artifact(
        format!("{base}/{manifest_name}"),
        directory.join(&manifest_name),
        expected_manifest,
    )?;
    let manifest_entries = checksums(&fs::read(&manifest.file.path)?)?;
    ensure!(
        manifest_entries.len() == 2,
        "V8 release manifest must contain exactly archive and source-binding checksums"
    );
    let archive_hash = manifest_entries
        .get(&archive_name)
        .context("V8 release manifest lacks exact archive checksum")?;
    let binding_hash = manifest_entries
        .get(&binding_name)
        .context("V8 release manifest lacks exact source-binding checksum")?;
    let archive = verified_artifact(
        format!("{base}/{archive_name}"),
        directory.join(&archive_name),
        archive_hash,
    )?;
    let binding = verified_artifact(
        format!("{base}/{binding_name}"),
        directory.join(&binding_name),
        binding_hash,
    )?;
    Ok(Some(V8Artifacts {
        version,
        target,
        features,
        profile: "ptrcomp_sandbox_release".into(),
        trust_source,
        trust_commit_blob_sha256: hash_bytes(&trusted_bytes),
        manifest,
        archive,
        binding,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v8_selection_uses_locked_version_and_actual_native_features() -> Result<()> {
        assert_eq!(
            pinned_version("[[package]]\nname='v8'\nversion='150.4.0'\n")?,
            Some("150.4.0".into())
        );
        assert!(pinned_version("[[package]]\nname='v8'\nversion='150.4.0'\n[[package]]\nname='v8'\nversion='149.2.0'\n").is_err());
        let features = selected_features(
            "codex-code-mode-runtime v0.0.0|\nv8 v150.4.0|default,use_custom_libcxx,v8_enable_pointer_compression,v8_enable_sandbox\nv8 v150.4.0|default,use_custom_libcxx,v8_enable_pointer_compression,v8_enable_sandbox (*)\n",
            "150.4.0",
        )?;
        assert_eq!(
            artifact_names("x86_64-pc-windows-msvc", &features)?,
            (
                "rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.lib.gz".into(),
                "src_binding_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.rs".into(),
                "rusty_v8_ptrcomp_sandbox_release_x86_64-pc-windows-msvc.sha256".into(),
            )
        );
        assert!(selected_features("v8 v149.2.0|default", "150.4.0").is_err());
        assert!(
            artifact_names(
                "x86_64-pc-windows-msvc",
                &BTreeSet::from(["default".into()])
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn v8_checksum_parser_accepts_crlf_and_rejects_ambiguous_or_escaping_entries() -> Result<()> {
        let hash = "a".repeat(64);
        assert_eq!(
            checksums(format!("{hash}  archive.lib.gz\r\n").as_bytes())?.get("archive.lib.gz"),
            Some(&hash)
        );
        for text in [
            format!("{hash}  ../archive.lib.gz\n"),
            format!("{hash}  archive.lib.gz\n{hash}  archive.lib.gz\n"),
            "bad  archive.lib.gz\n".into(),
        ] {
            assert!(checksums(text.as_bytes()).is_err());
        }
        Ok(())
    }

    #[test]
    fn v8_cached_files_are_verified_without_redownloading_or_hiding_tampering() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("native.lib.gz");
        fs::write(&path, b"pinned archive")?;
        let expected = hash_bytes(b"pinned archive");
        let artifact = verified_artifact(
            "https://github.com/openai/codex/releases/download/test/native.lib.gz".into(),
            path.clone(),
            &expected,
        )?;
        assert_eq!(artifact.file.sha256, expected);
        fs::write(&path, b"modified archive")?;
        assert!(
            verified_artifact(artifact.url, path.clone(), &expected)
                .unwrap_err()
                .to_string()
                .contains("cached V8 checksum mismatch")
        );
        assert_eq!(fs::read(&path)?, b"modified archive");
        Ok(())
    }

    #[test]
    fn v8_provenance_validates_both_assets_and_keys_the_trusted_release() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let archive_path = temp.path().join("native.lib.gz");
        let binding_path = temp.path().join("binding.rs");
        let manifest_path = temp.path().join("manifest.sha256");
        let trust_path = temp.path().join("trusted.sha256");
        fs::write(&archive_path, b"archive")?;
        fs::write(&binding_path, b"binding")?;
        let manifest_bytes = format!(
            "{}  native.lib.gz\n{}  binding.rs\n",
            hash_bytes(b"archive"),
            hash_bytes(b"binding")
        );
        fs::write(&manifest_path, &manifest_bytes)?;
        let trust_bytes = format!(
            "{}  manifest.sha256\n",
            hash_bytes(manifest_bytes.as_bytes())
        );
        fs::write(&trust_path, &trust_bytes)?;
        let trusted = checksums(trust_bytes.as_bytes())?;
        let manifest = verified_artifact(
            "https://example.invalid/manifest.sha256".into(),
            manifest_path.clone(),
            &trusted["manifest.sha256"],
        )?;
        let asset_hashes = checksums(&fs::read(&manifest.file.path)?)?;
        let mut artifacts = V8Artifacts {
            version: "150.4.0".into(),
            target: "x86_64-pc-windows-msvc".into(),
            features: BTreeSet::from([
                "v8_enable_sandbox".into(),
                "v8_enable_pointer_compression".into(),
            ]),
            profile: "ptrcomp_sandbox_release".into(),
            trust_source: FileIdentity::record(&trust_path)?,
            trust_commit_blob_sha256: hash_bytes(trust_bytes.as_bytes()),
            manifest,
            archive: verified_artifact(
                "https://example.invalid/native.lib.gz".into(),
                archive_path.clone(),
                &asset_hashes["native.lib.gz"],
            )?,
            binding: verified_artifact(
                "https://example.invalid/binding.rs".into(),
                binding_path.clone(),
                &asset_hashes["binding.rs"],
            )?,
        };
        artifacts.verify()?;
        let fingerprint = artifacts.fingerprint()?;
        let settings: BTreeMap<_, _> = artifacts.settings().into_iter().collect();
        assert_eq!(
            settings["RUSTY_V8_ARCHIVE"],
            git_path(&artifacts.archive.file.path).to_string_lossy()
        );
        assert_eq!(
            settings["RUSTY_V8_SRC_BINDING_PATH"],
            git_path(&artifacts.binding.file.path).to_string_lossy()
        );
        artifacts.binding.url.push_str("-changed");
        assert_ne!(
            artifacts.fingerprint()?,
            fingerprint,
            "binding source belongs to build provenance"
        );
        for (path, original) in [
            (&archive_path, b"archive".as_slice()),
            (&binding_path, b"binding".as_slice()),
            (&manifest_path, manifest_bytes.as_bytes()),
            (&trust_path, trust_bytes.as_bytes()),
        ] {
            fs::write(path, b"tampered")?;
            assert!(
                artifacts.verify().is_err(),
                "every link of the native artifact trust chain must be checked on reuse"
            );
            fs::write(path, original)?;
            artifacts.verify()?;
        }
        fs::write(&manifest_path, b"untrusted manifest")?;
        assert!(
            verified_artifact(
                artifacts.manifest.url,
                manifest_path,
                &trusted["manifest.sha256"]
            )
            .unwrap_err()
            .to_string()
            .contains("cached V8 checksum mismatch")
        );
        Ok(())
    }
}
