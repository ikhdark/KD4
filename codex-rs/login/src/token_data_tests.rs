use super::*;
use chrono::TimeZone;
use chrono::Utc;
use codex_protocol::auth::KnownPlan;
use pretty_assertions::assert_eq;
use serde::Serialize;

fn fake_jwt(payload: serde_json::Value) -> String {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }
    let header = Header {
        alg: "none",
        typ: "JWT",
    };

    fn b64url_no_pad(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    let header_b64 = b64url_no_pad(&serde_json::to_vec(&header).unwrap());
    let payload_b64 = b64url_no_pad(&serde_json::to_vec(&payload).unwrap());
    let signature_b64 = b64url_no_pad(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
}

#[test]
fn id_token_info_parses_email_and_plan() {
    let fake_jwt = fake_jwt(serde_json::json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro"
        }
    }));

    let info = parse_chatgpt_jwt_claims(&fake_jwt).expect("should parse");
    assert_eq!(info.email.as_deref(), Some("user@example.com"));
    assert_eq!(info.get_chatgpt_plan_type().as_deref(), Some("Pro (More)"));
}

#[test]
fn id_token_info_parses_go_plan() {
    let fake_jwt = fake_jwt(serde_json::json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "go"
        }
    }));

    let info = parse_chatgpt_jwt_claims(&fake_jwt).expect("should parse");
    assert_eq!(info.email.as_deref(), Some("user@example.com"));
    assert_eq!(info.get_chatgpt_plan_type().as_deref(), Some("Go"));
}

#[test]
fn id_token_info_parses_hc_plan_as_enterprise() {
    let fake_jwt = fake_jwt(serde_json::json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "hc"
        }
    }));

    let info = parse_chatgpt_jwt_claims(&fake_jwt).expect("should parse");
    assert_eq!(info.email.as_deref(), Some("user@example.com"));
    assert_eq!(info.get_chatgpt_plan_type().as_deref(), Some("Enterprise"));
    assert_eq!(info.is_workspace_account(), true);
}

#[test]
fn id_token_info_parses_ent26_plan() {
    let fake_jwt = fake_jwt(serde_json::json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "ent26"
        }
    }));

    let info = parse_chatgpt_jwt_claims(&fake_jwt).expect("should parse");
    assert_eq!(info.get_chatgpt_plan_type().as_deref(), Some("Enterprise"));
    assert_eq!(info.get_chatgpt_plan_type_raw().as_deref(), Some("ent26"));
    assert_eq!(info.is_workspace_account(), true);
}

#[test]
fn id_token_info_parses_usage_based_business_plans() {
    let self_serve_business_jwt = fake_jwt(serde_json::json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "self_serve_business_usage_based"
        }
    }));
    let self_serve_business =
        parse_chatgpt_jwt_claims(&self_serve_business_jwt).expect("should parse");
    assert_eq!(
        self_serve_business.get_chatgpt_plan_type().as_deref(),
        Some("Self Serve Business Usage Based")
    );
    assert_eq!(
        self_serve_business.get_chatgpt_plan_type_raw().as_deref(),
        Some("self_serve_business_usage_based")
    );
    assert_eq!(self_serve_business.is_workspace_account(), true);

    let enterprise_cbp_jwt = fake_jwt(serde_json::json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "enterprise_cbp_usage_based"
        }
    }));
    let enterprise_cbp = parse_chatgpt_jwt_claims(&enterprise_cbp_jwt).expect("should parse");
    assert_eq!(
        enterprise_cbp.get_chatgpt_plan_type().as_deref(),
        Some("Enterprise CBP Usage Based")
    );
    assert_eq!(
        enterprise_cbp.get_chatgpt_plan_type_raw().as_deref(),
        Some("enterprise_cbp_usage_based")
    );
    assert_eq!(enterprise_cbp.is_workspace_account(), true);
}

#[test]
fn id_token_info_parses_enterprise_cbp_automation_plan() {
    let jwt = fake_jwt(serde_json::json!({
        "email": "service-account@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "enterprise_cbp_automation"
        }
    }));

    let info = parse_chatgpt_jwt_claims(&jwt).expect("should parse");
    assert_eq!(
        info.get_chatgpt_plan_type().as_deref(),
        Some("Enterprise (Automation)")
    );
    assert_eq!(
        info.get_chatgpt_plan_type_raw().as_deref(),
        Some("enterprise_cbp_automation")
    );
    assert_eq!(info.is_workspace_account(), true);
}

#[test]
fn id_token_info_parses_self_serve_business_prolite_plan() {
    let jwt = fake_jwt(serde_json::json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "self_serve_business_prolite"
        }
    }));

    let info = parse_chatgpt_jwt_claims(&jwt).expect("should parse");
    assert_eq!(
        info.get_chatgpt_plan_type().as_deref(),
        Some("Self Serve Business ProLite")
    );
    assert_eq!(
        info.get_chatgpt_plan_type_raw().as_deref(),
        Some("self_serve_business_prolite")
    );
    assert_eq!(info.is_workspace_account(), true);
}

#[test]
fn id_token_info_handles_missing_fields() {
    let fake_jwt = fake_jwt(serde_json::json!({ "sub": "123" }));

    let info = parse_chatgpt_jwt_claims(&fake_jwt).expect("should parse");
    assert!(info.email.is_none());
    assert!(info.get_chatgpt_plan_type().is_none());
    assert_eq!(info.is_fedramp_account(), false);
}

#[test]
fn id_token_info_parses_fedramp_account_claim() {
    let fake_jwt = fake_jwt(serde_json::json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "account-fed",
            "chatgpt_account_is_fedramp": true,
        }
    }));

    let info = parse_chatgpt_jwt_claims(&fake_jwt).expect("should parse");
    assert_eq!(info.chatgpt_account_id.as_deref(), Some("account-fed"));
    assert_eq!(info.is_fedramp_account(), true);
}

#[test]
fn jwt_expiration_parses_exp_claim() {
    let fake_jwt = fake_jwt(serde_json::json!({
        "exp": 1_700_000_000_i64,
    }));

    let expires_at = parse_jwt_expiration(&fake_jwt).expect("should parse");
    assert_eq!(expires_at, Utc.timestamp_opt(1_700_000_000, 0).single());
}

#[test]
fn jwt_expiration_handles_missing_exp() {
    let fake_jwt = fake_jwt(serde_json::json!({ "sub": "123" }));

    let expires_at = parse_jwt_expiration(&fake_jwt).expect("should parse");
    assert_eq!(expires_at, None);
}

#[test]
fn jwt_expiration_rejects_malformed_jwt() {
    let err = parse_jwt_expiration("not-a-jwt").expect_err("should fail");
    assert_eq!(err.to_string(), "invalid ID token format");
}

#[test]
fn token_data_round_trip_preserves_jwt_string_and_claim_projection() {
    let jwt = fake_jwt(serde_json::json!({
        "https://api.openai.com/profile": {"email": "profile@example.com"},
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "future-plan",
            "chatgpt_user_id": "preferred-user",
            "user_id": "fallback-user",
            "chatgpt_account_id": "workspace-123"
        }
    }));
    let stored = serde_json::json!({
        "id_token": jwt,
        "access_token": "access-secret",
        "refresh_token": "refresh-secret",
        "account_id": "workspace-123"
    });
    let tokens: TokenData = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(
        tokens.id_token.email.as_deref(),
        Some("profile@example.com")
    );
    assert_eq!(
        tokens.id_token.chatgpt_user_id.as_deref(),
        Some("preferred-user")
    );
    assert_eq!(
        tokens.id_token.get_chatgpt_plan_type_raw().as_deref(),
        Some("future-plan")
    );
    assert_eq!(
        tokens.id_token.chatgpt_account_id.as_deref(),
        Some("workspace-123")
    );
    assert_eq!(serde_json::to_value(&tokens).unwrap(), stored);
    assert_eq!(
        serde_json::from_str::<TokenData>(&serde_json::to_string(&tokens).unwrap()).unwrap(),
        tokens
    );
    let debug = format!("{tokens:?} {:?}", tokens.id_token);
    for secret in [jwt.as_str(), "access-secret", "refresh-secret"] {
        assert!(!debug.contains(secret), "Debug exposed a credential");
    }
}

#[test]
fn jwt_parsers_reject_invalid_segments_encoding_and_json() {
    let valid = fake_jwt(serde_json::json!({"exp": 1_700_000_000}));
    for invalid in [
        format!("{valid}.extra"),
        format!("{valid}."),
        ".e30.sig".to_string(),
        "e30..sig".to_string(),
        "e30.e30.".to_string(),
        "e30.!.sig".to_string(),
        "e30.bm90LWpzb24.sig".to_string(),
    ] {
        assert!(
            parse_chatgpt_jwt_claims(&invalid).is_err(),
            "accepted {invalid}"
        );
        assert!(
            parse_jwt_expiration(&invalid).is_err(),
            "accepted {invalid}"
        );
    }
}

#[test]
fn jwt_expiration_rejects_present_but_unrepresentable_timestamp() {
    for exp in [i64::MIN, i64::MAX] {
        let jwt = fake_jwt(serde_json::json!({"exp": exp}));
        assert!(matches!(
            parse_jwt_expiration(&jwt),
            Err(IdTokenInfoError::InvalidExpiration)
        ));
    }
}

#[test]
fn workspace_account_detection_matches_workspace_plans() {
    let workspace = IdTokenInfo {
        chatgpt_plan_type: Some(PlanType::Known(KnownPlan::Business)),
        ..IdTokenInfo::default()
    };
    assert_eq!(workspace.is_workspace_account(), true);

    let personal = IdTokenInfo {
        chatgpt_plan_type: Some(PlanType::Known(KnownPlan::Pro)),
        ..IdTokenInfo::default()
    };
    assert_eq!(personal.is_workspace_account(), false);

    let personal = IdTokenInfo {
        chatgpt_plan_type: Some(PlanType::Known(KnownPlan::ProLite)),
        ..IdTokenInfo::default()
    };
    assert_eq!(personal.is_workspace_account(), false);
}

#[test]
fn claims_preserve_email_and_user_id_precedence() {
    let info = parse_chatgpt_jwt_claims(&fake_jwt(serde_json::json!({
        "email": "primary", "https://api.openai.com/profile": {"email": "fallback"},
        "https://api.openai.com/auth": {"chatgpt_user_id": "primary-id", "user_id": "fallback-id"}
    })))
    .unwrap();
    assert_eq!(info.email.as_deref(), Some("primary"));
    assert_eq!(info.chatgpt_user_id.as_deref(), Some("primary-id"));
    let fallback = parse_chatgpt_jwt_claims(&fake_jwt(serde_json::json!({
        "https://api.openai.com/profile": {"email": "fallback"},
        "https://api.openai.com/auth": {"user_id": "fallback-id"}
    })))
    .unwrap();
    assert_eq!(fallback.email.as_deref(), Some("fallback"));
    assert_eq!(fallback.chatgpt_user_id.as_deref(), Some("fallback-id"));
}
