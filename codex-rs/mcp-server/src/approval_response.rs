use codex_protocol::protocol::ReviewDecision;
use rmcp::model::ElicitationAction;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct ApprovalResponse {
    action: Option<ElicitationAction>,
    content: Option<Value>,
    decision: Option<ReviewDecision>,
}

pub(crate) fn review_decision_from_elicitation_response(
    value: Value,
) -> Result<ReviewDecision, serde_json::Error> {
    let ApprovalResponse {
        action,
        content,
        decision,
    } = serde_json::from_value(value)?;

    match action {
        Some(ElicitationAction::Accept) => {
            let content_decision = match content {
                Some(content) => {
                    let mut content =
                        serde_json::from_value::<serde_json::Map<String, Value>>(content)?;
                    content
                        .remove("decision")
                        .map(serde_json::from_value::<ReviewDecision>)
                        .transpose()?
                }
                None => None,
            };
            Ok(content_decision
                .or(decision)
                .unwrap_or(ReviewDecision::Approved))
        }
        Some(ElicitationAction::Decline | ElicitationAction::Cancel) => Ok(ReviewDecision::Denied),
        None => Ok(decision.unwrap_or(ReviewDecision::Denied)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn approval_response_obeys_action_content_and_legacy_precedence() {
        for (value, expected) in [
            (json!({"action": "accept"}), ReviewDecision::Approved),
            (json!({"action": "accept", "content": {}}), ReviewDecision::Approved),
            (json!({"action": "accept", "content": {"decision": "denied"}}), ReviewDecision::Denied),
            (json!({"action": "decline"}), ReviewDecision::Denied),
            (json!({"action": "cancel"}), ReviewDecision::Denied),
            (json!({"decision": "approved_for_session"}), ReviewDecision::ApprovedForSession),
            (json!({}), ReviewDecision::Denied),
            (json!({"action": "decline", "decision": "approved"}), ReviewDecision::Denied),
            (json!({"action": "accept", "content": {"decision": "denied"}, "decision": "approved"}), ReviewDecision::Denied),
            (json!({"action": "accept", "decision": "approved_for_session"}), ReviewDecision::ApprovedForSession),
            (json!({"action": "cancel", "content": {"decision": "approved"}, "decision": "approved"}), ReviewDecision::Denied),
            (json!({"content": {"decision": "approved"}}), ReviewDecision::Denied),
        ] {
            assert_eq!(review_decision_from_elicitation_response(value.clone()).unwrap(), expected, "{value}");
        }
    }

    #[test]
    fn malformed_approval_responses_are_rejected() {
        for (value, expected_error) in [
            (json!({"action": "approve"}), "unknown variant `approve`"),
            (json!({"action": "accept", "content": {"decision": "yes"}}), "unknown variant `yes`"),
            (json!({"action": "accept", "content": []}), "invalid type"),
        ] {
            let error = review_decision_from_elicitation_response(value.clone()).unwrap_err();
            assert!(error.is_data(), "{value}: {error}");
            assert!(error.to_string().contains(expected_error), "{value}: {error}");
        }
    }
}
