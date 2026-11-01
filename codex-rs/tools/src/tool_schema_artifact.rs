#![deny(clippy::print_stdout, clippy::print_stderr)]
use crate::ToolSpec;
use std::sync::Arc;
use sha2::{Digest, Sha256};

/// Immutable, model-visible tool schema data shared by prompt construction,
/// request serialization, accounting, and retries.
#[derive(Debug)]
pub struct ToolSchemaArtifact {
    specs: Arc<[ToolSpec]>,
    serialized: Arc<[u8]>,
    digest: [u8; 32],
    has_request_user_input: bool,
    has_wait: bool,
}

impl ToolSchemaArtifact {
    pub fn new(mut specs: Vec<ToolSpec>) -> Self {
        // Order only the effective surface, after permission/exposure gates and
        // collision resolution. Never reorder schema arrays or promote tools.
        // Stable sorts preserve the relative order of any duplicate identities.
        specs.sort_by(|left, right| left.name().cmp(right.name()));
        for spec in &mut specs {
            if let ToolSpec::Namespace(namespace) = spec {
                namespace.tools.sort_by(|left, right| match (left, right) {
                    (
                        crate::ResponsesApiNamespaceTool::Function(left),
                        crate::ResponsesApiNamespaceTool::Function(right),
                    ) => left.name.cmp(&right.name),
                });
            }
        }
        let specs: Arc<[ToolSpec]> = specs.into();
        let serialized: Arc<[u8]> = serde_json::to_vec(specs.as_ref())
            .unwrap_or_default()
            .into();
        Self {
            has_request_user_input: specs.iter().any(|spec| spec.name() == "request_user_input"),
            has_wait: specs.iter().any(|spec| spec.name() == "wait"),
            digest: Sha256::digest(serialized.as_ref()).into(),
            specs,
            serialized,
        }
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub fn serialized(&self) -> &[u8] {
        &self.serialized
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn has_request_user_input(&self) -> bool {
        self.has_request_user_input
    }

    pub fn has_wait(&self) -> bool {
        self.has_wait
    }
}

impl Default for ToolSchemaArtifact {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}
