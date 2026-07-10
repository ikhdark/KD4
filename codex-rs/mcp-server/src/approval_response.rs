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
    fn parses_spec_accept_response_without_content() {
        let decision = review_decision_from_elicitation_response(json!({
            "action": "accept"
        }))
        .expect("parse response");

        assert_eq!(decision, ReviewDecision::Approved);
    }

    #[test]
    fn parses_spec_accept_response_with_empty_content() {
        let decision = review_decision_from_elicitation_response(json!({
            "action": "accept",
            "content": {}
        }))
        .expect("parse response");

        assert_eq!(decision, ReviewDecision::Approved);
    }

    #[test]
    fn parses_spec_accept_response_with_content_decision() {
        let decision = review_decision_from_elicitation_response(json!({
            "action": "accept",
            "content": {
                "decision": "denied"
            }
        }))
        .expect("parse response");

        assert_eq!(decision, ReviewDecision::Denied);
    }

    #[test]
    fn maps_spec_decline_response_to_denied() {
        let decision = review_decision_from_elicitation_response(json!({
            "action": "decline"
        }))
        .expect("parse response");

        assert_eq!(decision, ReviewDecision::Denied);
    }

    #[test]
    fn maps_spec_cancel_response_to_denied() {
        let decision = review_decision_from_elicitation_response(json!({
            "action": "cancel"
        }))
        .expect("parse response");

        assert_eq!(decision, ReviewDecision::Denied);
    }

    #[test]
    fn preserves_legacy_decision_response() {
        let decision = review_decision_from_elicitation_response(json!({
            "decision": "approved_for_session"
        }))
        .expect("parse response");

        assert_eq!(decision, ReviewDecision::ApprovedForSession);
    }

    #[test]
    fn missing_action_and_decision_defaults_to_denied() {
        let decision =
            review_decision_from_elicitation_response(json!({})).expect("parse response");

        assert_eq!(decision, ReviewDecision::Denied);
    }

    #[test]
    fn decline_overrides_conflicting_legacy_approval() {
        let decision = review_decision_from_elicitation_response(json!({
            "action": "decline",
            "decision": "approved"
        }))
        .expect("parse response");

        assert_eq!(decision, ReviewDecision::Denied);
    }

    #[test]
    fn accepted_content_decision_overrides_conflicting_legacy_approval() {
        let decision = review_decision_from_elicitation_response(json!({
            "action": "accept",
            "content": {
                "decision": "denied"
            },
            "decision": "approved"
        }))
        .expect("parse response");

        assert_eq!(decision, ReviewDecision::Denied);
    }

    #[test]
    fn invalid_action_is_rejected() {
        let result = review_decision_from_elicitation_response(json!({
            "action": "approve"
        }));

        assert!(result.is_err());
    }

    #[test]
    fn invalid_content_decision_is_rejected() {
        let result = review_decision_from_elicitation_response(json!({
            "action": "accept",
            "content": {
                "decision": "yes"
            }
        }));

        assert!(result.is_err());
    }
}
