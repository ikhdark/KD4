//! Shared network contracts without either HTTP or proxy runtime ancestry.

mod retry_after;

pub use retry_after::RetryAfter;

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicyDecision {
    Deny,
    Ask,
}

impl NetworkPolicyDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDecisionSource {
    BaselinePolicy,
    ModeGuard,
    ProxyState,
    Decider,
}

impl NetworkDecisionSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BaselinePolicy => "baseline_policy",
            Self::ModeGuard => "mode_guard",
            Self::ProxyState => "proxy_state",
            Self::Decider => "decider",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_wire_spellings_round_trip() {
        for (decision, decision_name) in [
            (NetworkPolicyDecision::Deny, "deny"),
            (NetworkPolicyDecision::Ask, "ask"),
        ] {
            for (source, source_name) in [
                (NetworkDecisionSource::BaselinePolicy, "baseline_policy"),
                (NetworkDecisionSource::ModeGuard, "mode_guard"),
                (NetworkDecisionSource::ProxyState, "proxy_state"),
                (NetworkDecisionSource::Decider, "decider"),
            ] {
                let wire = serde_json::json!([decision_name, source_name]);
                assert_eq!(serde_json::to_value((decision, source)).unwrap(), wire);
                assert_eq!(
                    serde_json::from_value::<(NetworkPolicyDecision, NetworkDecisionSource)>(wire)
                        .unwrap(),
                    (decision, source)
                );
                assert_eq!(decision.as_str(), decision_name);
                assert_eq!(source.as_str(), source_name);
            }
        }
    }
}
