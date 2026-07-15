use std::sync::Arc;

use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use codex_tools::create_tools_json_for_responses_api;
use once_cell::sync::OnceCell;
use sha2::Digest;
use sha2::Sha256;
#[cfg(test)]
use std::sync::atomic::AtomicU8;
#[cfg(test)]
use std::sync::atomic::Ordering;

#[cfg(test)]
const FAIL_PLANNING_DIGEST: u8 = 1 << 0;
#[cfg(test)]
const FAIL_RESPONSES: u8 = 1 << 1;
#[cfg(test)]
const FAIL_COMPACT: u8 = 1 << 2;
#[cfg(test)]
const FAIL_RESPONSES_LITE: u8 = 1 << 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolWireMode {
    Responses,
    Compact,
    ResponsesLite,
}

impl ToolWireMode {
    fn fingerprint_tag(self) -> &'static [u8] {
        match self {
            Self::Responses => b"responses",
            Self::Compact => b"compact",
            Self::ResponsesLite => b"responses-lite",
        }
    }

    #[cfg(test)]
    fn failure_bit(self) -> u8 {
        match self {
            Self::Responses => FAIL_RESPONSES,
            Self::Compact => FAIL_COMPACT,
            Self::ResponsesLite => FAIL_RESPONSES_LITE,
        }
    }
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
    #[cfg(test)]
    injected_failures: AtomicU8,
}

impl ToolSchemaBundle {
    pub(crate) fn new(specs: Vec<ToolSpec>) -> Self {
        Self {
            canonical: specs.into(),
            planning_digest: OnceCell::new(),
            responses: OnceCell::new(),
            compact: OnceCell::new(),
            responses_lite: OnceCell::new(),
            #[cfg(test)]
            injected_failures: AtomicU8::new(0),
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
            .get_or_try_init(|| {
                #[cfg(test)]
                self.maybe_fail_serialization(FAIL_PLANNING_DIGEST)?;
                serde_json::to_vec(self.canonical.as_ref())
            })
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
        #[cfg(test)]
        self.maybe_fail_serialization(mode.failure_bit())?;
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
        hasher.update(mode.fingerprint_tag());
        hasher.update((serialized.len() as u64).to_be_bytes());
        hasher.update(serialized);
        Ok(ToolWireProduct {
            value,
            fingerprint: hasher.finalize().into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_planning_serialization(&self) {
        self.injected_failures
            .fetch_or(FAIL_PLANNING_DIGEST, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_wire_serialization(&self, mode: ToolWireMode) {
        self.injected_failures
            .fetch_or(mode.failure_bit(), Ordering::SeqCst);
    }

    #[cfg(test)]
    fn maybe_fail_serialization(&self, failure_bit: u8) -> serde_json::Result<()> {
        let previous = self
            .injected_failures
            .fetch_and(!failure_bit, Ordering::SeqCst);
        if previous & failure_bit == 0 {
            return Ok(());
        }
        Err(serde_json::Error::io(std::io::Error::other(
            "injected tool-schema serialization failure",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn serialized_product(product: &ToolWireProduct) -> Vec<u8> {
        match &product.value {
            ToolWireValue::TopLevel(tools) => {
                serde_json::to_vec(tools.as_ref()).expect("top-level tools should serialize")
            }
            ToolWireValue::AdditionalTools(item) => {
                serde_json::to_vec(item.as_ref()).expect("additional tools should serialize")
            }
        }
    }

    #[test]
    fn lazy_mode_products_match_legacy_bytes_and_use_separate_fingerprints() {
        let bundle =
            ToolSchemaBundle::new(vec![ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: "calendar_lookup".to_string(),
                description: "Look up a calendar entry.".to_string(),
                strict: true,
                defer_loading: Some(false),
                parameters: codex_tools::JsonSchema::default(),
                output_schema: Some(serde_json::json!({"type": "object"})),
            })]);
        let legacy_tools = create_tools_json_for_responses_api(bundle.canonical())
            .expect("legacy tools should serialize");
        let legacy_top_level =
            serde_json::to_vec(&legacy_tools).expect("legacy top-level tools should serialize");
        let legacy_lite = serde_json::to_vec(&ResponseItem::AdditionalTools {
            id: None,
            role: "developer".to_string(),
            tools: legacy_tools,
        })
        .expect("legacy lite tools should serialize");

        assert_eq!(
            bundle
                .planning_digest_bytes()
                .expect("planning digest should serialize"),
            serde_json::to_vec(bundle.canonical())
                .expect("legacy planning digest should serialize")
        );
        let responses = bundle
            .wire_product(ToolWireMode::Responses)
            .expect("Responses tools should serialize");
        let responses_again = bundle
            .wire_product(ToolWireMode::Responses)
            .expect("Responses tools should be cached");
        let compact = bundle
            .wire_product(ToolWireMode::Compact)
            .expect("compact tools should serialize");
        let lite = bundle
            .wire_product(ToolWireMode::ResponsesLite)
            .expect("Lite tools should serialize");

        assert!(std::ptr::eq(responses, responses_again));
        assert_eq!(serialized_product(responses), legacy_top_level);
        assert_eq!(serialized_product(compact), legacy_top_level);
        assert_eq!(serialized_product(lite), legacy_lite);
        assert_ne!(responses.fingerprint(), compact.fingerprint());
        assert_ne!(responses.fingerprint(), lite.fingerprint());
        assert_ne!(compact.fingerprint(), lite.fingerprint());
    }

    #[test]
    fn failed_lazy_serializations_are_not_cached_and_retry() {
        let bundle = ToolSchemaBundle::new(Vec::new());
        bundle.fail_next_planning_serialization();
        assert!(bundle.planning_digest_bytes().is_err());
        assert!(bundle.planning_digest.get().is_none());
        assert!(bundle.planning_digest_bytes().is_ok());
        assert!(bundle.planning_digest.get().is_some());

        for mode in [
            ToolWireMode::Responses,
            ToolWireMode::Compact,
            ToolWireMode::ResponsesLite,
        ] {
            bundle.fail_next_wire_serialization(mode);
            assert!(bundle.wire_product(mode).is_err());
            let slot = match mode {
                ToolWireMode::Responses => &bundle.responses,
                ToolWireMode::Compact => &bundle.compact,
                ToolWireMode::ResponsesLite => &bundle.responses_lite,
            };
            assert!(slot.get().is_none());
            assert!(bundle.wire_product(mode).is_ok());
            assert!(slot.get().is_some());
        }
    }
}
