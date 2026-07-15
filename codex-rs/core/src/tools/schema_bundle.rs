use std::sync::Arc;

use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use codex_tools::create_tools_json_for_responses_api;
use once_cell::sync::OnceCell;
use sha2::Digest;
use sha2::Sha256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolWireMode {
    Responses,
    Compact,
    ResponsesLite,
}

#[derive(Debug, Clone)]
pub(crate) enum ToolWireValue {
    TopLevel(Arc<[serde_json::Value]>),
    AdditionalTools(Arc<ResponseItem>),
}

#[derive(Debug)]
pub(crate) struct ToolWireProduct {
    value: ToolWireValue,
    fingerprint: [u8; 32],
}

impl ToolWireProduct {
    pub(crate) fn value(&self) -> ToolWireValue {
        self.value.clone()
    }

    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

#[derive(Debug)]
pub(crate) struct ToolSchemaBundle {
    canonical: Arc<[ToolSpec]>,
    planning_digest: OnceCell<Vec<u8>>,
    responses: OnceCell<ToolWireProduct>,
    compact: OnceCell<ToolWireProduct>,
    responses_lite: OnceCell<ToolWireProduct>,
}

impl ToolSchemaBundle {
    pub(crate) fn new(specs: Vec<ToolSpec>) -> Self {
        Self {
            canonical: specs.into(),
            planning_digest: OnceCell::new(),
            responses: OnceCell::new(),
            compact: OnceCell::new(),
            responses_lite: OnceCell::new(),
        }
    }

    pub(crate) fn empty() -> Arc<Self> {
        Arc::new(Self::new(Vec::new()))
    }

    #[allow(dead_code)]
    pub(crate) fn canonical(&self) -> &[ToolSpec] {
        &self.canonical
    }

    pub(crate) fn planning_digest_bytes(&self) -> serde_json::Result<&[u8]> {
        self.planning_digest
            .get_or_try_init(|| serde_json::to_vec(self.canonical.as_ref()))
            .map(Vec::as_slice)
    }

    pub(crate) fn wire_product(&self, mode: ToolWireMode) -> serde_json::Result<&ToolWireProduct> {
        let slot = match mode {
            ToolWireMode::Responses => &self.responses,
            ToolWireMode::Compact => &self.compact,
            ToolWireMode::ResponsesLite => &self.responses_lite,
        };
        slot.get_or_try_init(|| self.serialize_wire_product(mode))
    }

    fn serialize_wire_product(&self, mode: ToolWireMode) -> serde_json::Result<ToolWireProduct> {
        let tools = create_tools_json_for_responses_api(self.canonical.as_ref())?;
        let value = match mode {
            ToolWireMode::Responses | ToolWireMode::Compact => {
                ToolWireValue::TopLevel(tools.into())
            }
            ToolWireMode::ResponsesLite => {
                ToolWireValue::AdditionalTools(Arc::new(ResponseItem::AdditionalTools {
                    id: None,
                    role: "developer".to_string(),
                    tools,
                }))
            }
        };
        let serialized = match &value {
            ToolWireValue::TopLevel(tools) => serde_json::to_vec(tools.as_ref())?,
            ToolWireValue::AdditionalTools(item) => serde_json::to_vec(item.as_ref())?,
        };
        let mut hasher = Sha256::new();
        hasher.update(b"codex.tool-wire-product.v1");
        hasher.update((serialized.len() as u64).to_be_bytes());
        hasher.update(serialized);
        Ok(ToolWireProduct {
            value,
            fingerprint: hasher.finalize().into(),
        })
    }
}
