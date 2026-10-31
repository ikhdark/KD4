use crate::ExecServerRuntimePaths;
use crate::protocol::CapabilityManifest;
use crate::protocol::EnvironmentInfo;
use sha2::Digest;

/// Shared fingerprint owner for native and RPC-backed environments.
pub(crate) fn manifest(
    runtime_paths: &ExecServerRuntimePaths,
    backend: &str,
    mut methods: Vec<String>,
    methods_complete: bool,
) -> Result<CapabilityManifest, serde_json::Error> {
    methods.sort();
    methods.dedup();
    let configuration = serde_json::json!({
        "operating_system": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "cwd": std::env::current_dir().map_err(serde_json::Error::io)?,
        "helper_executable": runtime_paths.codex_self_exe.as_path(),
    });
    let mut manifest = CapabilityManifest {
        schema_version: crate::protocol::CAPABILITY_MANIFEST_VERSION,
        backend: backend.into(),
        backend_id: EnvironmentInfo::local_resource_id(),
        backend_version: env!("CARGO_PKG_VERSION").into(),
        configuration_revision: revision(&configuration)?,
        revision: String::new(),
        methods,
        methods_complete,
    };
    manifest.revision = revision(&manifest)?;
    Ok(manifest)
}

fn revision(value: &impl serde::Serialize) -> Result<String, serde_json::Error> {
    let digest = sha2::Sha256::digest(serde_json::to_vec(value)?);
    Ok(format!(
        "v1:{}",
        digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
    ))
}
