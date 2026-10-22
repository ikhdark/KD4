use crate::approvals::NetworkApprovalProtocol;
use codex_network_types::NetworkDecisionSource;
use codex_network_types::NetworkPolicyDecision;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyDecisionPayload {
    pub decision: NetworkPolicyDecision,
    pub source: NetworkDecisionSource,
    #[serde(default)]
    pub protocol: Option<NetworkApprovalProtocol>,
    pub host: Option<String>,
    pub reason: Option<String>,
    pub port: Option<u16>,
}

impl NetworkPolicyDecisionPayload {
    pub fn is_ask_from_decider(&self) -> bool {
        self.decision == NetworkPolicyDecision::Ask && self.source == NetworkDecisionSource::Decider
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ask_from_decider_requests_approval() {
        for decision in ["deny", "ask"] {
            for source in ["baseline_policy", "mode_guard", "proxy_state", "decider"] {
                let payload: NetworkPolicyDecisionPayload = serde_json::from_value(
                    serde_json::json!({ "decision": decision, "source": source }),
                )
                .unwrap();
                assert_eq!(
                    payload.is_ask_from_decider(),
                    decision == "ask" && source == "decider"
                );
                assert!(payload.protocol.is_none());
                assert!(payload.host.is_none());
                assert!(payload.reason.is_none());
                assert!(payload.port.is_none());
            }
        }
    }
}
